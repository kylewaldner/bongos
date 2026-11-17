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
//! identifier. [`DoneHolds`] holds sources that keep a finished task from Done; the store fills
//! each task's `done_hold` from them. [`observe_view_opened`] holds observers told when the Work panel is shown or
//! `task: focus` runs. With nothing registered, neither changes anything.
//!
//! Adding or removing a source, and a source telling the registry its answer changed, notify the
//! store, which recomputes and emits an event. None of these calls may be made from inside an
//! update of the store.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

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

/// Something that may hold a task back from Done although its files say it is finished, such
/// as a required check with no pass on the task's current tree.
pub trait DoneHoldSource: 'static {
    /// Why `task_id` is held now, from what the source already knows; `None` when it is not.
    /// Synchronous: it never computes anything new. The store asks it while it updates, so it
    /// must not read the store.
    fn hold(&self, task_id: &str, cx: &App) -> Option<String>;

    /// Why `task_id` would be held if it were finished now; `None` when it would not. It may
    /// take time (a fresh look at the task's tree), so it answers through a task.
    fn would_hold(&self, task_id: &str, cx: &mut App) -> gpui::Task<Option<String>>;
}

/// Brings what a done hold source knows up to date from the place it reads (run state, say). The
/// store calls every registered refresher on its background executor each time it reads its
/// tree, right after it read the projects, so the holds it asks the sources for next are at least
/// as new as that tree: a hold that commits together with the write that finishes a task is
/// never missing from the snapshot that shows the task finished.
pub trait DoneHoldRefresher: Send + Sync + 'static {
    fn refresh(&self);
}

/// The registered refreshers. Cloning shares the registry, so the store's session sees a
/// refresher added after it started.
#[derive(Clone, Default)]
pub(crate) struct DoneHoldRefreshers(
    Arc<parking_lot::Mutex<Vec<(String, Arc<dyn DoneHoldRefresher>)>>>,
);

impl Global for DoneHoldRefreshers {}

impl DoneHoldRefreshers {
    pub(crate) fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    /// Calls every refresher, in the order they were added.
    pub(crate) fn refresh(&self) {
        let refreshers: Vec<_> = self
            .0
            .lock()
            .iter()
            .map(|(_, refresher)| refresher.clone())
            .collect();
        for refresher in refreshers {
            refresher.refresh();
        }
    }
}

#[derive(Default)]
struct DoneHoldSources(Vec<(String, Rc<dyn DoneHoldSource>)>);

impl Global for DoneHoldSources {}

/// The sources of done holds. A namespace for the global registry. With no source, nothing is
/// ever held: [`DoneHolds::hold`] is `None` and [`DoneHolds::would_hold`] resolves to `None` at
/// once.
pub struct DoneHolds;

impl DoneHolds {
    /// Adds the source `name`, replacing one of the same name, and notifies the store.
    pub fn add_source(name: &str, source: Rc<dyn DoneHoldSource>, cx: &mut App) {
        let sources = cx.default_global::<DoneHoldSources>();
        sources.0.retain(|(existing, _)| existing != name);
        sources.0.push((name.to_string(), source));
        Self::changed(cx);
    }

    /// Removes the source `name` and notifies the store.
    pub fn remove_source(name: &str, cx: &mut App) {
        cx.default_global::<DoneHoldSources>()
            .0
            .retain(|(existing, _)| existing != name);
        Self::changed(cx);
    }

    /// Adds the refresher `name`, replacing one of the same name. It runs from the store's next
    /// tree read on.
    pub fn add_refresher(name: &str, refresher: Arc<dyn DoneHoldRefresher>, cx: &mut App) {
        let refreshers = DoneHoldRefreshers::global(cx);
        let mut list = refreshers.0.lock();
        list.retain(|(existing, _)| existing != name);
        list.push((name.to_string(), refresher));
    }

    /// Removes the refresher `name`.
    pub fn remove_refresher(name: &str, cx: &mut App) {
        DoneHoldRefreshers::global(cx)
            .0
            .lock()
            .retain(|(existing, _)| existing != name);
    }

    /// Tells the store that what a source holds changed, so it fills the holds of its tree again.
    pub fn changed(cx: &mut App) {
        if let Some(store) = WorkStore::global(cx) {
            store.update(cx, |store, cx| store.refresh_done_holds(cx));
        }
    }

    /// Whether any source is registered.
    pub fn has_sources(cx: &App) -> bool {
        cx.try_global::<DoneHoldSources>()
            .is_some_and(|sources| !sources.0.is_empty())
    }

    fn sources(cx: &App) -> Vec<Rc<dyn DoneHoldSource>> {
        cx.try_global::<DoneHoldSources>()
            .map(|sources| sources.0.iter().map(|(_, source)| source.clone()).collect())
            .unwrap_or_default()
    }

    /// Why `task_id` is held now: the answer of the first source, in the order they were added,
    /// that holds it.
    pub fn hold(task_id: &str, cx: &App) -> Option<String> {
        Self::sources(cx)
            .into_iter()
            .find_map(|source| source.hold(task_id, cx))
    }

    /// Why `task_id` would be held if it were finished now: the answer of the first source, in
    /// the order they were added, that would hold it. Every source is asked at once.
    pub fn would_hold(task_id: &str, cx: &mut App) -> gpui::Task<Option<String>> {
        let sources = Self::sources(cx);
        if sources.is_empty() {
            return gpui::Task::ready(None);
        }
        let answers: Vec<_> = sources
            .into_iter()
            .map(|source| source.would_hold(task_id, cx))
            .collect();
        cx.background_executor().spawn(async move {
            futures::future::join_all(answers)
                .await
                .into_iter()
                .flatten()
                .next()
        })
    }
}

/// The name of the source of pending marks in the registry.
pub(crate) const PENDING_SOURCE: &str = "pending";

/// The chip of a task whose change waits to land or did not: its mark's label, `landing` or
/// `not landed`, the latter with why ("not landed: check tests failed: ..."). Drawn in the row
/// like every chip, so in the theme's colors.
pub(crate) fn pending_chip(task: &bongos_work_model::store::Task) -> Option<SharedString> {
    task.pending.as_ref().map(|mark| match &mark.reason {
        Some(reason) => SharedString::from(format!("{}: {reason}", mark.label)),
        None => SharedString::from(mark.label.clone()),
    })
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

/// The name of the source of the marks of worktrees and run records missing on this machine.
pub(crate) const MISSING_SOURCE: &str = "missing on this machine";

/// Shows "worktree missing" and "interrupted, record not on this machine" on the rows of the
/// tasks of open project repositories whose state names a worktree or run record this machine
/// lacks (after a restore from another machine, say). Nothing is recreated.
struct MissingChips;

impl RowChipsSource for MissingChips {
    fn chips(&self, task_id: &str, cx: &App) -> Vec<SharedString> {
        crate::project_repos::open_repos(cx)
            .iter()
            .flat_map(|loaded| loaded.marks_of(task_id))
            .map(SharedString::from)
            .collect()
    }
}

/// Registers the chips of missing worktrees and run records; the refreshes of the store
/// redraw them.
pub(crate) fn register_missing_chips(cx: &mut App) {
    RowChipsRegistry::add_source(MISSING_SOURCE, Rc::new(MissingChips), cx);
}

/// The name of the source of the master warnings of open project repositories.
pub(crate) const MASTER_SOURCE: &str = "master";

/// Shows the warning of a project repository whose master was changed outside landing (an
/// outside commit waiting for approval, a split master) on the row of its project.
struct MasterChips;

impl RowChipsSource for MasterChips {
    fn chips(&self, task_id: &str, cx: &App) -> Vec<SharedString> {
        let Some(snapshot) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot())
        else {
            return Vec::new();
        };
        snapshot
            .projects
            .iter()
            .filter(|project| project.task.id() == Some(task_id))
            .filter_map(|project| crate::project_repos::master_warning_of(&project.task, cx))
            .map(SharedString::from)
            .collect()
    }
}

/// Registers the master warning chips; a change of a warning redraws them.
pub(crate) fn register_master_chips(cx: &mut App) {
    RowChipsRegistry::add_source(MASTER_SOURCE, Rc::new(MasterChips), cx);
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
