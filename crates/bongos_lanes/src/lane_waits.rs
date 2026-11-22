//! Lane wait handlers: what other features decide about a lane that waits.
//!
//! A lane is `Waiting` after an environment refusal, a failed `verify_lane`, a floor or create
//! rule failure, a `before_job` refusal or `set_wait`, or a pool. At the end of every pass the
//! dispatcher asks the registered [`LaneWaitHandler`]s about each waiting lane, at most once per
//! pass per lane and never while an earlier question about the same wait is still pending. The
//! handlers are asked in registration order; the first answer other than
//! [`LaneWaitAction::Keep`] decides and the later handlers are not asked. With no handler
//! registered every wait behaves as `Keep`.
//!
//! - `Keep`: the lane waits as before and is looked at again by the usual triggers.
//! - `RecheckAfter(d)`: the lane is also evaluated again once `d` has passed.
//! - `Ask(text)`: the lane waits on a person. The text is kept in an ask record next to the lane
//!   record (`ask/<lane id>` in the lanes namespace), shown by `status`, and reported through the
//!   `lanes` needs-you source on the top-level task the lane owns, which the Work panel rolls up
//!   to every ancestor. No pass evaluates the lane again until `lanes: resume` clears the ask.
//!
//! An ask is in effect only while its lane is still waiting with the reason it was asked about;
//! a lane that left that wait (stopped, paused, resumed, given a new reason) leaves its ask
//! behind, and the next pass removes it. An answer that comes after its lane left the wait it was
//! asked about, or after a newer question about the lane, is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use bongos_lanes_model::{LaneId, LaneRecord, LaneState, LanesState, WaitReason};
use bongos_project_repo::{StateRead, StateTx};
use bongos_work::{NeedsYou, NeedsYouRegistry, NeedsYouSource};
use gpui::{App, Global, Task};
use serde::{Deserialize, Serialize};

use crate::dispatcher::{Dispatcher, held_task};
use crate::store::{LANES_NAMESPACE, read_state, read_state_in};

/// The key prefix of the ask records, followed by the lane id.
pub const ASK_PREFIX: &str = "ask/";
/// The name of the needs-you source of lanes.
pub const NEEDS_YOU_SOURCE: &str = "lanes";

/// What a handler decides about a waiting lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneWaitAction {
    /// The lane waits as it does with no handler.
    Keep,
    /// The lane waits and is evaluated again after this long, as well as at the usual triggers.
    RecheckAfter(Duration),
    /// The lane waits on a person, who is shown this text, until `lanes: resume`.
    Ask(String),
}

/// A feature's view of waiting lanes.
pub trait LaneWaitHandler {
    /// Called when `lane` waits with `reason`: when it enters the wait and at each later pass that
    /// finds it still waiting. Dropping the task drops the question.
    fn on_wait(&self, lane: &LaneId, reason: &WaitReason, cx: &mut App) -> Task<LaneWaitAction>;
}

/// The registered lane wait handlers, asked in the order they were registered.
#[derive(Default)]
pub struct LaneWaitHandlers {
    handlers: Vec<Rc<dyn LaneWaitHandler>>,
}

impl Global for LaneWaitHandlers {}

impl LaneWaitHandlers {
    pub fn register(handler: Rc<dyn LaneWaitHandler>, cx: &mut App) {
        cx.default_global::<Self>().handlers.push(handler);
    }

    /// Every registered handler, in registration order.
    pub fn handlers(cx: &App) -> Vec<Rc<dyn LaneWaitHandler>> {
        cx.try_global::<Self>()
            .map(|handlers| handlers.handlers.clone())
            .unwrap_or_default()
    }
}

/// Asks the handlers in order; the first answer other than `Keep` is the answer.
pub(crate) fn ask_handlers(
    handlers: Vec<Rc<dyn LaneWaitHandler>>,
    lane: LaneId,
    reason: WaitReason,
    cx: &mut App,
) -> Task<LaneWaitAction> {
    cx.spawn(async move |cx| {
        for handler in handlers {
            let answer = cx.update(|cx| handler.on_wait(&lane, &reason, cx));
            match answer.await {
                LaneWaitAction::Keep => {}
                decided => return decided,
            }
        }
        LaneWaitAction::Keep
    })
}

/// A lane waiting on a person: the wait it was asked about and the text the person is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAsk {
    pub reason: WaitReason,
    pub text: String,
    pub since: String,
}

impl LaneAsk {
    /// Whether this ask is in effect for `lane`: the lane still waits with the reason asked about.
    pub fn applies_to(&self, lane: &LaneRecord) -> bool {
        lane.state == LaneState::Waiting && lane.reason.as_ref() == Some(&self.reason)
    }
}

fn ask_key(lane: &LaneId) -> String {
    format!("{ASK_PREFIX}{lane}")
}

fn asks_from(rows: Vec<(String, toml::Value)>) -> Result<BTreeMap<LaneId, LaneAsk>> {
    let mut asks = BTreeMap::new();
    for (key, value) in rows {
        let id = &key[ASK_PREFIX.len()..];
        let Some(lane) = LaneId::parse(id) else {
            log::warn!("lanes: the ask record {key} names no lane");
            continue;
        };
        let ask = value
            .try_into()
            .with_context(|| format!("reading the ask of lane {id}"))?;
        asks.insert(lane, ask);
    }
    Ok(asks)
}

/// The ask of `lane` in `tx`, in effect or not.
pub(crate) fn ask_in(tx: &mut StateTx, lane: &LaneId) -> Result<Option<LaneAsk>> {
    tx.kv(LANES_NAMESPACE)
        .get(&ask_key(lane))?
        .map(|value| {
            value
                .try_into()
                .with_context(|| format!("reading the ask of lane {lane}"))
        })
        .transpose()
}

/// Every ask record in `tx`, by lane.
pub(crate) fn asks_in(tx: &mut StateTx) -> Result<BTreeMap<LaneId, LaneAsk>> {
    asks_from(tx.kv(LANES_NAMESPACE).scan(ASK_PREFIX)?)
}

/// Every ask record, by lane.
pub fn read_asks(read: &StateRead) -> Result<BTreeMap<LaneId, LaneAsk>> {
    asks_from(read.kv(LANES_NAMESPACE).scan(ASK_PREFIX)?)
}

pub(crate) fn put_ask(tx: &mut StateTx, lane: &LaneId, ask: &LaneAsk) -> Result<()> {
    let value = toml::Value::try_from(ask).context("serializing a lane ask")?;
    tx.kv(LANES_NAMESPACE).put(&ask_key(lane), value)
}

pub(crate) fn delete_ask(tx: &mut StateTx, lane: &LaneId) -> Result<()> {
    tx.kv(LANES_NAMESPACE).delete(&ask_key(lane))
}

/// The text of the ask in effect for `lane` in `state`, if any.
pub(crate) fn asked_text(
    state: &LanesState,
    asks: &BTreeMap<LaneId, LaneAsk>,
    lane: &LaneId,
) -> Option<String> {
    let ask = asks.get(lane)?;
    let record = state.lanes.get(lane)?;
    ask.applies_to(record).then(|| ask.text.clone())
}

/// The needs-you items of the asks in effect: each on the top-level task its lane owns. A lane
/// owning no task shows its ask only on its own row.
pub(crate) fn needs_you_items(
    state: &LanesState,
    asks: &BTreeMap<LaneId, LaneAsk>,
) -> Vec<NeedsYou> {
    asks.keys()
        .filter_map(|lane| {
            let text = asked_text(state, asks, lane)?;
            let (task, _) = held_task(state, lane)?;
            Some(NeedsYou {
                task_id: task.0.clone(),
                reason: format!("{lane}: {text}"),
            })
        })
        .collect()
}

/// What the lanes of each project ask of a person, by project id.
#[derive(Default)]
struct AskedTasks(BTreeMap<String, Vec<NeedsYou>>);

impl Global for AskedTasks {}

/// Sets what the lanes of `project` ask, telling the needs-you registry when it changed.
pub(crate) fn publish_asks(project: &str, items: Vec<NeedsYou>, cx: &mut App) {
    let asked = cx.default_global::<AskedTasks>();
    let current = asked.0.get(project).map(Vec::as_slice).unwrap_or(&[]);
    if current == items.as_slice() {
        return;
    }
    if items.is_empty() {
        asked.0.remove(project);
    } else {
        asked.0.insert(project.to_string(), items);
    }
    NeedsYouRegistry::changed(cx);
}

/// Reports the top-level tasks whose lane waits on a person.
struct LanesNeedsYou;

impl NeedsYouSource for LanesNeedsYou {
    fn needs_you(&self, cx: &App) -> Vec<NeedsYou> {
        cx.try_global::<AskedTasks>()
            .map(|asked| asked.0.values().flatten().cloned().collect())
            .unwrap_or_default()
    }
}

/// Registers the `lanes` needs-you source.
pub fn register_needs_you(cx: &mut App) {
    NeedsYouRegistry::add_source(NEEDS_YOU_SOURCE, Rc::new(LanesNeedsYou), cx);
}

/// A question to the handlers about one wait of a lane, until it is answered; dropping it drops
/// the question.
struct PendingWait {
    reason: WaitReason,
    seq: u64,
    task: Task<()>,
}

/// A recheck a handler asked for, until it comes due; dropping it drops the timer.
struct Recheck {
    reason: WaitReason,
    seq: u64,
    task: Task<()>,
}

/// What the dispatcher tracks of the lane wait handlers.
#[derive(Default)]
pub(crate) struct WaitTracking {
    pending: BTreeMap<LaneId, PendingWait>,
    rechecks: BTreeMap<LaneId, Recheck>,
    /// Lanes whose recheck came due, evaluated by the next pass as a full pass would.
    due: BTreeSet<LaneId>,
    /// Numbers each question and recheck, so a late answer of a replaced one is told apart.
    seq: u64,
}

impl WaitTracking {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

/// Whether `lane` waits with `reason` in `state`.
fn waiting_with(state: &LanesState, lane: &LaneId, reason: &WaitReason) -> bool {
    state.lanes.get(lane).is_some_and(|record| {
        record.state == LaneState::Waiting && record.reason.as_ref() == Some(reason)
    })
}

impl Dispatcher {
    /// The lanes whose recheck came due since the last pass.
    pub(crate) fn take_due(&self) -> BTreeSet<LaneId> {
        std::mem::take(&mut self.0.waits.borrow_mut().due)
    }

    /// At the end of a pass: removes the asks no longer in effect, reports the ones in effect,
    /// drops the questions and rechecks of waits the lanes left, and asks the handlers about every
    /// waiting lane that has no ask in effect and no question pending.
    pub(crate) fn settle_waits(&self, cx: &mut App) {
        let writer = self.parts().writer.clone();
        let read = writer.read(|read| Ok((read_state(read)?, read_asks(read)?)));
        let (state, mut asks) = match read {
            Ok(read) => read,
            Err(error) => {
                log::error!("lanes: reading the lane waits: {error:#}");
                return;
            }
        };
        let in_effect = |asks: &BTreeMap<LaneId, LaneAsk>, lane: &LaneId| {
            asked_text(&state, asks, lane).is_some()
        };
        let stale: Vec<LaneId> = asks
            .keys()
            .filter(|lane| !in_effect(&asks, lane))
            .cloned()
            .collect();
        if !stale.is_empty() {
            let removed = writer.transact(|tx| {
                let state = read_state_in(tx)?;
                for lane in &stale {
                    let applies = |ask: &LaneAsk| {
                        state
                            .lanes
                            .get(lane)
                            .is_some_and(|record| ask.applies_to(record))
                    };
                    if ask_in(tx, lane)?.is_some_and(|ask| !applies(&ask)) {
                        delete_ask(tx, lane)?;
                    }
                }
                Ok(())
            });
            match removed {
                Ok(()) => asks.retain(|lane, _| !stale.contains(lane)),
                Err(error) => {
                    log::error!("lanes: removing the asks no longer in effect: {error:#}")
                }
            }
        }
        publish_asks(&self.0.config.project, needs_you_items(&state, &asks), cx);

        // Dropped after the borrow ends, since dropping a task drops its future.
        let mut dropped_pending = Vec::new();
        let mut dropped_rechecks = Vec::new();
        let mut to_ask = Vec::new();
        let handlers = LaneWaitHandlers::handlers(cx);
        {
            let mut waits = self.0.waits.borrow_mut();
            let left: Vec<LaneId> = waits
                .pending
                .iter()
                .filter(|(lane, pending)| !waiting_with(&state, lane, &pending.reason))
                .map(|(lane, _)| lane.clone())
                .collect();
            for lane in left {
                dropped_pending.extend(waits.pending.remove(&lane));
            }
            let left: Vec<LaneId> = waits
                .rechecks
                .iter()
                .filter(|(lane, recheck)| {
                    !waiting_with(&state, lane, &recheck.reason) || in_effect(&asks, lane)
                })
                .map(|(lane, _)| lane.clone())
                .collect();
            for lane in left {
                dropped_rechecks.extend(waits.rechecks.remove(&lane));
            }
            if !handlers.is_empty() {
                for record in state.lanes.values() {
                    let (LaneState::Waiting, Some(reason)) = (record.state, &record.reason) else {
                        continue;
                    };
                    let lane = record.id();
                    if in_effect(&asks, &lane) || waits.pending.contains_key(&lane) {
                        continue;
                    }
                    to_ask.push((lane, reason.clone()));
                }
            }
        }
        drop(dropped_pending);
        drop(dropped_rechecks);
        for (lane, reason) in to_ask {
            self.ask_about(lane, reason, handlers.clone(), cx);
        }
    }

    /// Asks the handlers about `lane` waiting with `reason`.
    fn ask_about(
        &self,
        lane: LaneId,
        reason: WaitReason,
        handlers: Vec<Rc<dyn LaneWaitHandler>>,
        cx: &mut App,
    ) {
        let seq = self.0.waits.borrow_mut().next_seq();
        let answer = ask_handlers(handlers, lane.clone(), reason.clone(), cx);
        let weak = self.downgrade();
        let task = cx.spawn({
            let lane = lane.clone();
            let reason = reason.clone();
            async move |cx| {
                let action = answer.await;
                if let Some(inner) = weak.upgrade() {
                    cx.update(|cx| {
                        Dispatcher(inner).wait_answered(&lane, &reason, seq, action, cx)
                    });
                }
            }
        });
        let replaced = self
            .0
            .waits
            .borrow_mut()
            .pending
            .insert(lane, PendingWait { reason, seq, task });
        drop(replaced);
    }

    /// The handlers answered the question `seq` about `lane` waiting with `reason`. An answer to
    /// a question that was replaced or dropped, or about a wait the lane left, is dropped.
    fn wait_answered(
        &self,
        lane: &LaneId,
        reason: &WaitReason,
        seq: u64,
        action: LaneWaitAction,
        cx: &mut App,
    ) {
        let answered = {
            let mut waits = self.0.waits.borrow_mut();
            match waits.pending.get(lane) {
                Some(pending) if pending.seq == seq => waits.pending.remove(lane),
                _ => None,
            }
        };
        let Some(answered) = answered else {
            return;
        };
        // This runs inside that task, at its end; it is let finish rather than cancelled.
        answered.task.detach();
        if self.has_ended() {
            return;
        }
        let writer = self.parts().writer.clone();
        match writer.read(read_state) {
            Ok(state) if waiting_with(&state, lane, reason) => {}
            Ok(_) => return,
            Err(error) => {
                log::error!("lanes: reading the wait of {lane}: {error:#}");
                return;
            }
        }
        match action {
            LaneWaitAction::Keep => {
                let recheck = self.0.waits.borrow_mut().rechecks.remove(lane);
                drop(recheck);
            }
            LaneWaitAction::RecheckAfter(after) => self.recheck_after(lane, reason, after, cx),
            LaneWaitAction::Ask(text) => {
                let recheck = self.0.waits.borrow_mut().rechecks.remove(lane);
                drop(recheck);
                let ask = LaneAsk {
                    reason: reason.clone(),
                    text,
                    since: self.parts().host.now(),
                };
                let written = writer.transact(|tx| {
                    let state = read_state_in(tx)?;
                    let Some(record) = state.lanes.get(lane) else {
                        return Ok(());
                    };
                    let asked =
                        ask_in(tx, lane)?.is_some_and(|existing| existing.applies_to(record));
                    if ask.applies_to(record) && !asked {
                        put_ask(tx, lane, &ask)?;
                    }
                    Ok(())
                });
                if let Err(error) = written {
                    log::error!("lanes: recording the ask of {lane}: {error:#}");
                }
            }
        }
    }

    /// Evaluates `lane` again after `after`, unless a recheck of the same wait is already set,
    /// which keeps its earlier time.
    fn recheck_after(&self, lane: &LaneId, reason: &WaitReason, after: Duration, cx: &mut App) {
        let seq = {
            let mut waits = self.0.waits.borrow_mut();
            if waits
                .rechecks
                .get(lane)
                .is_some_and(|recheck| recheck.reason == *reason)
            {
                return;
            }
            waits.next_seq()
        };
        let weak = self.downgrade();
        let task = cx.spawn({
            let lane = lane.clone();
            async move |cx| {
                cx.background_executor().timer(after).await;
                if let Some(inner) = weak.upgrade() {
                    cx.update(|cx| Dispatcher(inner).recheck_due(&lane, seq, cx));
                }
            }
        });
        let replaced = self.0.waits.borrow_mut().rechecks.insert(
            lane.clone(),
            Recheck {
                reason: reason.clone(),
                seq,
                task,
            },
        );
        drop(replaced);
    }

    /// The recheck `seq` of `lane` came due: the lane is evaluated as a full pass would.
    fn recheck_due(&self, lane: &LaneId, seq: u64, cx: &mut App) {
        let due = {
            let mut waits = self.0.waits.borrow_mut();
            match waits.rechecks.get(lane) {
                Some(recheck) if recheck.seq == seq => {
                    waits.due.insert(lane.clone());
                    waits.rechecks.remove(lane)
                }
                _ => None,
            }
        };
        let Some(due) = due else {
            return;
        };
        due.task.detach();
        self.evaluate(false, cx);
    }
}
