//! Gates and what they tell the app (D14, D15): the `workflows` [`NeedsYouSource`] (a leaf with a
//! branch at a gate, a failed run, an interrupted run or a run held on a check without a verdict
//! needs Kyle, and feature 03 rolls that up through every ancestor), and one toast with an
//! "Open run" action per gate or failure.
//!
//! A gate itself is two events in the run log: the runtime appends `GateOpened` the moment the
//! scheduler offers it, which sets that branch waiting on Kyle (the other branches of a fan-out
//! keep running); the lifecycle queues a [`WorkflowEvent::Changed`] in the same update, and
//! [`WorkflowEngine::publish`] tells the sources then. The gate is in the log, so a reload shows
//! it open again.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use bongos_work::{NeedsYou, NeedsYouRegistry, NeedsYouSource};
use bongos_workflow_model::record::{CapReached, ConsentPending, EndState};
use bongos_workflow_model::{NodeKey, RunState};
use gpui::{App, Global};
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Toast};

use crate::actions::workflow;
use crate::engine::WorkflowEngine;
use crate::lifecycle::{Lifecycle, RunId, WorkflowEvent};
use crate::runtime::Runner;
use crate::status_item;

/// What a run says it needs from Kyle, one line per reason, in the words of the work view.
pub fn reasons(state: &RunState) -> Vec<String> {
    if state.cancelled {
        return Vec::new();
    }
    match &state.ended {
        Some((EndState::Failed, failure)) => {
            let detail = failure
                .as_ref()
                .map(|failure| failure.detail.trim())
                .filter(|detail| !detail.is_empty())
                .or_else(|| failure.as_ref().map(|failure| failure.reason.label()))
                .unwrap_or("unknown reason");
            return vec![format!("run failed: {detail}")];
        }
        Some(_) => return Vec::new(),
        None => {}
    }
    let mut found: Vec<String> = state.cap_reached.iter().map(CapReached::reason).collect();
    found.extend(
        state
            .open_gates
            .values()
            .map(|question| format!("gate: {question}")),
    );
    if !state.interrupted.is_empty() {
        found.push("run interrupted".to_string());
    }
    found.extend(
        state
            .held
            .values()
            .map(|failure| format!("check had no verdict: {}", failure.detail.trim())),
    );
    if let Some(pending) = &state.consent_pending {
        found.push(format!(
            "sandbox: {} needs your answer",
            pending_node(pending)
        ));
    }
    found
}

/// The node a pending sandbox ask names: the first key it holds.
fn pending_node(pending: &ConsentPending) -> &str {
    pending.keys.first().map_or("", |key| key.node.as_str())
}

/// The tasks of the active runs that need Kyle, with why: the runs this process drives and those
/// it shows for another process.
pub fn needs_you<R: Runner>(lifecycle: &Lifecycle<R>) -> Vec<NeedsYou> {
    let mut found = Vec::new();
    for run in lifecycle.shown_active_runs() {
        let (Some(state), Some(task)) =
            (lifecycle.shown_state(&run), lifecycle.shown_task_of(&run))
        else {
            continue;
        };
        for reason in reasons(&state) {
            found.push(NeedsYou {
                task_id: task.to_string(),
                reason,
            });
        }
    }
    found
}

/// Reports the runs that need Kyle from the engine's lifecycle.
struct WorkflowNeedsYou {
    engine: WorkflowEngine,
}

impl NeedsYouSource for WorkflowNeedsYou {
    fn needs_you(&self, _: &App) -> Vec<NeedsYou> {
        needs_you(&self.engine.lifecycle())
    }
}

/// One toast to show: a gate opened or a run failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub run: RunId,
    pub message: String,
}

/// The gates and failures that already had their toast, so an update that changes nothing about
/// them shows none.
#[derive(Default)]
pub struct Announced {
    gates: BTreeMap<RunId, BTreeSet<NodeKey>>,
    failed: BTreeSet<RunId>,
    asked: BTreeSet<RunId>,
}

impl Announced {
    /// The toasts `state` of `run` earns that were not shown yet: one per gate newly open, one for
    /// a failure, one for a sandbox ask while it is pending. A gate that closed and opens again (a revision) earns a new one.
    pub fn notices(&mut self, run: &str, state: &RunState) -> Vec<Notice> {
        let mut found = Vec::new();
        let open: BTreeSet<NodeKey> = state.open_gates.keys().cloned().collect();
        let before = self.gates.entry(run.to_string()).or_default();
        for key in open.difference(before) {
            let question = &state.open_gates[key];
            found.push(Notice {
                run: run.to_string(),
                message: format!("Workflow needs you at {}: {question}", key.node),
            });
        }
        *before = open;
        match &state.consent_pending {
            Some(pending) => {
                if self.asked.insert(run.to_string()) {
                    found.push(Notice {
                        run: run.to_string(),
                        message: format!(
                            "Workflow sandbox: {} needs your answer",
                            pending_node(pending)
                        ),
                    });
                }
            }
            None => {
                self.asked.remove(run);
            }
        }
        let failed = matches!(state.ended, Some((EndState::Failed, _)));
        if failed {
            if self.failed.insert(run.to_string()) {
                let reason = reasons(state).into_iter().next().unwrap_or_default();
                found.push(Notice {
                    run: run.to_string(),
                    message: format!("Workflow {reason}"),
                });
            }
        } else {
            self.failed.remove(run);
        }
        found
    }
}

/// The toasts shown, for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Delivered(pub Vec<Notice>);

#[cfg(test)]
impl Global for Delivered {}

#[derive(Default)]
struct AnnouncedGlobal(Announced);

impl Global for AnnouncedGlobal {}

/// Shows `notice` as a toast with an "Open run" action in the active window, if there is one.
fn deliver(notice: Notice, cx: &mut App) {
    #[cfg(test)]
    cx.default_global::<Delivered>().0.push(notice.clone());
    let Some(window) = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
    else {
        return;
    };
    window
        .update(cx, |multi, _, cx| {
            let toast = Toast::new(
                NotificationId::unique::<GateToast>(),
                notice.message.clone(),
            )
            .on_click("Open run", |_, cx| {
                cx.dispatch_action(&workflow::ShowRun);
            });
            multi
                .workspace()
                .update(cx, |workspace, cx| workspace.show_toast(toast, cx));
        })
        .ok();
}

/// The toast of a gate or a failure.
pub struct GateToast;

/// Hands one engine event on: the toasts it earns, and the status item's refresh.
fn on_event(engine: &WorkflowEngine, event: &WorkflowEvent, cx: &mut App) {
    let WorkflowEvent::Changed { run, .. } = event;
    let state = engine.lifecycle().state(run);
    if let Some(state) = state {
        let notices = cx
            .default_global::<AnnouncedGlobal>()
            .0
            .notices(run, &state);
        for notice in notices {
            deliver(notice, cx);
        }
    }
    status_item::changed(cx);
}

/// Registers the `workflows` needs-you source over `engine` and the toasts.
pub fn register(engine: &WorkflowEngine, cx: &mut App) {
    NeedsYouRegistry::add_source(
        crate::sources::SOURCE_NAME,
        Rc::new(WorkflowNeedsYou {
            engine: engine.clone(),
        }),
        cx,
    );
    let engine = engine.clone();
    WorkflowEngine::observe(cx, move |event, cx| on_event(&engine, event, cx));
}
