//! Registries other features fill (D13). This crate stores none of what they report: a source is
//! asked every time, and the answers are only folded into the roll-ups and badges the store
//! computes.
//!
//! [`NeedsYouRegistry`] holds sources of tasks that need the owner (gates, waiting todos and review
//! comments), each with a short reason. Every ancestor of a reported task counts it in its
//! roll-up (`Rollup::needs_you`), at any depth, until the source stops reporting it or is removed.
//!
//! [`LiveModesRegistry`] holds sources of the live modes of a task: open chats and the active
//! run. It also answers "one active run per leaf" ([`LiveModesRegistry::can_start_run`]), which
//! the workflow engine enforces.
//!
//! [`RowChipsRegistry`] holds sources of extra chips for a task's row, shown after its Linear
//! identifier. [`observe_view_opened`] holds observers told when the Work panel is shown or
//! `task: focus` runs. With nothing registered, neither changes anything.
//!
//! Adding or removing a source, and a source telling the registry its answer changed, notify the
//! store, which recomputes and emits an event. None of these calls may be made from inside an
//! update of the store.

use std::collections::BTreeMap;
use std::rc::Rc;

use bongos_work_model::rollup::{self, Rollup};
use gpui::{App, Context, Global, SharedString};

use crate::{WorkStore, WorkStoreEvent};

/// A task that needs the owner, and why in a few words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsYou {
    pub task_id: String,
    pub reason: String,
}

/// Something that knows which tasks need the owner.
pub trait NeedsYouSource: 'static {
    fn needs_you(&self, cx: &App) -> Vec<NeedsYou>;
}

#[derive(Default)]
struct NeedsYouSources(Vec<(String, Rc<dyn NeedsYouSource>)>);

impl Global for NeedsYouSources {}

/// The sources of "needs you". A namespace for the global registry.
pub struct NeedsYouRegistry;

impl NeedsYouRegistry {
    /// Adds the source `name`, replacing one of the same name, and notifies the store.
    pub fn add_source(name: &str, source: Rc<dyn NeedsYouSource>, cx: &mut App) {
        let sources = cx.default_global::<NeedsYouSources>();
        sources.0.retain(|(existing, _)| existing != name);
        sources.0.push((name.to_string(), source));
        Self::changed(cx);
    }

    /// Removes the source `name` and notifies the store.
    pub fn remove_source(name: &str, cx: &mut App) {
        cx.default_global::<NeedsYouSources>()
            .0
            .retain(|(existing, _)| existing != name);
        Self::changed(cx);
    }

    /// Tells the store that what a source reports changed.
    pub fn changed(cx: &mut App) {
        if let Some(store) = WorkStore::global(cx) {
            store.update(cx, |store, cx| store.refresh_needs_you(cx));
        }
    }

    /// Every reported task id with its reasons, in the order the sources gave them.
    pub fn collect(cx: &App) -> BTreeMap<String, Vec<String>> {
        let sources: Vec<_> = cx
            .try_global::<NeedsYouSources>()
            .map(|sources| sources.0.iter().map(|(_, source)| source.clone()).collect())
            .unwrap_or_default();
        let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for source in sources {
            for item in source.needs_you(cx) {
                let reasons = found.entry(item.task_id).or_default();
                if !reasons.contains(&item.reason) {
                    reasons.push(item.reason);
                }
            }
        }
        found
    }
}

/// The run active on a task, as its source reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveRun {
    pub workflow: String,
    pub status: String,
    pub step: String,
    /// Runs of this task that already ended.
    pub past_runs: usize,
}

impl ActiveRun {
    /// The text of the run chip on a row.
    pub fn chip(&self) -> String {
        format!(
            "Run in progress: {}, {}, step {}",
            self.workflow, self.status, self.step
        )
    }
}

/// What a source knows about the live modes of one task.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveModes {
    /// Chat sessions open on the task.
    pub open_chats: usize,
    /// The run active on the task, if any.
    pub run: Option<ActiveRun>,
}

/// Something that knows the live modes of a task: chats (feature 04) or runs (feature 20).
pub trait LiveModesSource: 'static {
    fn live_modes(&self, task_id: &str, cx: &App) -> LiveModes;
}

#[derive(Default)]
struct LiveModesSources(Vec<(String, Rc<dyn LiveModesSource>)>);

impl Global for LiveModesSources {}

/// Something that adds chips to the row of a task, such as the state of its Linear ticket.
pub trait RowChipsSource: 'static {
    fn chips(&self, task_id: &str, cx: &App) -> Vec<SharedString>;
}

#[derive(Default)]
struct RowChipsSources(Vec<(String, Rc<dyn RowChipsSource>)>);

impl Global for RowChipsSources {}

/// The sources of row chips. A namespace for the global registry.
pub struct RowChipsRegistry;

impl RowChipsRegistry {
    /// Adds the source `name`, replacing one of the same name, and notifies the store.
    pub fn add_source(name: &str, source: Rc<dyn RowChipsSource>, cx: &mut App) {
        let sources = cx.default_global::<RowChipsSources>();
        sources.0.retain(|(existing, _)| existing != name);
        sources.0.push((name.to_string(), source));
        Self::changed(cx);
    }

    /// Removes the source `name` and notifies the store.
    pub fn remove_source(name: &str, cx: &mut App) {
        cx.default_global::<RowChipsSources>()
            .0
            .retain(|(existing, _)| existing != name);
        Self::changed(cx);
    }

    /// Tells the store that what a source reports changed.
    pub fn changed(cx: &mut App) {
        if let Some(store) = WorkStore::global(cx) {
            store.update(cx, |store, cx| store.live_modes_changed(cx));
        }
    }

    /// The chips of `task_id` across all sources, in the order the sources were added.
    pub fn chips(task_id: &str, cx: &App) -> Vec<SharedString> {
        let sources: Vec<_> = cx
            .try_global::<RowChipsSources>()
            .map(|sources| sources.0.iter().map(|(_, source)| source.clone()).collect())
            .unwrap_or_default();
        sources
            .into_iter()
            .flat_map(|source| source.chips(task_id, cx))
            .collect()
    }
}

/// The name of the source of pending marks in the registry.
pub(crate) const PENDING_SOURCE: &str = "pending";

/// The chip of a task whose change waits to land or did not: its mark's label, `landing` or
/// `not landed`. Drawn in the row like every chip, so in the theme's colors.
pub(crate) fn pending_chip(task: &bongos_work_model::store::Task) -> Option<SharedString> {
    task.pending
        .as_ref()
        .map(|mark| SharedString::from(mark.label.clone()))
}

/// Shows the pending marks the snapshot carries (`Task::pending`) as row chips.
struct PendingChips;

impl RowChipsSource for PendingChips {
    fn chips(&self, task_id: &str, cx: &App) -> Vec<SharedString> {
        let Some(snapshot) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot())
        else {
            return Vec::new();
        };
        snapshot
            .task_by_id(task_id)
            .and_then(pending_chip)
            .into_iter()
            .collect()
    }
}

/// Registers the pending-mark chips; the refreshes of the store redraw them.
pub(crate) fn register_pending_chips(cx: &mut App) {
    RowChipsRegistry::add_source(PENDING_SOURCE, Rc::new(PendingChips), cx);
}

#[derive(Default)]
struct ViewOpenedObservers(Vec<Rc<dyn Fn(&mut App)>>);

impl Global for ViewOpenedObservers {}

/// Calls `f` each time the Work panel becomes active and each time `task: focus` runs. Observers
/// stay for the life of the app.
pub fn observe_view_opened(cx: &mut App, f: impl Fn(&mut App) + 'static) {
    cx.default_global::<ViewOpenedObservers>()
        .0
        .push(Rc::new(f));
}

/// Tells the observers of [`observe_view_opened`]. Does nothing with none.
pub(crate) fn view_opened(cx: &mut App) {
    let observers: Vec<_> = cx
        .try_global::<ViewOpenedObservers>()
        .map(|observers| observers.0.clone())
        .unwrap_or_default();
    for observer in observers {
        observer(cx);
    }
}

/// The answer to "may a run start on this leaf".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStart {
    pub allowed: bool,
    /// Why not, in the words of the run chip.
    pub chip: Option<String>,
}

/// The sources of live modes. A namespace for the global registry.
pub struct LiveModesRegistry;

impl LiveModesRegistry {
    /// Adds the source `name`, replacing one of the same name, and notifies the store.
    pub fn add_source(name: &str, source: Rc<dyn LiveModesSource>, cx: &mut App) {
        let sources = cx.default_global::<LiveModesSources>();
        sources.0.retain(|(existing, _)| existing != name);
        sources.0.push((name.to_string(), source));
        Self::changed(cx);
    }

    /// Removes the source `name` and notifies the store.
    pub fn remove_source(name: &str, cx: &mut App) {
        cx.default_global::<LiveModesSources>()
            .0
            .retain(|(existing, _)| existing != name);
        Self::changed(cx);
    }

    /// Tells the store that what a source reports changed.
    pub fn changed(cx: &mut App) {
        if let Some(store) = WorkStore::global(cx) {
            store.update(cx, |store, cx| store.live_modes_changed(cx));
        }
    }

    /// The live modes of `task_id` across all sources: chats add up, the first run found wins.
    pub fn live_modes(task_id: &str, cx: &App) -> LiveModes {
        let sources: Vec<_> = cx
            .try_global::<LiveModesSources>()
            .map(|sources| sources.0.iter().map(|(_, source)| source.clone()).collect())
            .unwrap_or_default();
        let mut live = LiveModes::default();
        for source in sources {
            let reported = source.live_modes(task_id, cx);
            live.open_chats += reported.open_chats;
            if live.run.is_none() {
                live.run = reported.run;
            }
        }
        live
    }

    /// One active run per leaf: not allowed while a source reports a run on `task_id`.
    pub fn can_start_run(task_id: &str, cx: &App) -> RunStart {
        match Self::live_modes(task_id, cx).run {
            Some(run) => RunStart {
                allowed: false,
                chip: Some(run.chip()),
            },
            None => RunStart {
                allowed: true,
                chip: None,
            },
        }
    }
}

impl WorkStore {
    /// The tasks sources say need the owner, with the reasons.
    pub fn needs_you(&self) -> &BTreeMap<String, Vec<String>> {
        &self.needs_you
    }

    /// Why the task `id` itself needs the owner. Empty when it does not.
    pub fn needs_you_reasons(&self, id: &str) -> &[String] {
        self.needs_you.get(id).map_or(&[], Vec::as_slice)
    }

    /// The roll-up of the task `id` and everything under it, with the reported tasks as the
    /// needs-you set. `None` while there is no snapshot or no such task.
    pub fn rollup(&self, id: &str) -> Option<Rollup> {
        let tree = self.snapshot()?;
        let task = tree.task_by_id(id)?;
        let set = self.needs_you.keys().cloned().collect();
        Some(rollup::rollup(task, &tree, &set))
    }

    /// Asks every needs-you source again and emits [`WorkStoreEvent::NeedsYouChanged`] when the
    /// answer differs.
    pub fn refresh_needs_you(&mut self, cx: &mut Context<Self>) {
        let found = NeedsYouRegistry::collect(cx);
        if found != self.needs_you {
            self.needs_you = found;
            cx.emit(WorkStoreEvent::NeedsYouChanged);
            cx.notify();
        }
    }

    /// Emits [`WorkStoreEvent::LiveModesChanged`].
    pub fn live_modes_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(WorkStoreEvent::LiveModesChanged);
        cx.notify();
    }
}
