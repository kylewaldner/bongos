//! The `checks` done hold source: in every process, a task is held exactly while its run state
//! has an owed marker (`checks.owed/<task id>`) and no valid done record (`checks.done/<task id>`).
//! The reason is the decider's `hold/<task id>` row in `checks.live`, or "checking" until one
//! exists.
//!
//! The Work store asks a source while it updates, so the source never reads a store then: it
//! answers from a snapshot of those rows. The snapshot is read again on the store's background
//! executor each time the store reads its tree, right after the projects (a
//! [`DoneHoldRefresher`]), so a marker that committed with the write that finished a task is in
//! the snapshot that first shows the task finished. Writes to `checks.live` alone do not read the
//! tree again; in the process that holds a project's writer the source follows them itself and
//! tells [`DoneHolds::changed`] when a reason changed. A process without the writer gets them
//! through [`ChecksHolds::state_changed`].

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use bongos_project_repo::{StateEvent, StateHandle, StateRead, StateWriter};
use bongos_work::{DoneHoldRefresher, DoneHoldSource, DoneHolds, OpenProjectRepos};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, AsyncApp, Global};
use settings::{Settings as _, SettingsStore};

use crate::evidence_store::{DONE_NAMESPACE, DoneRecord, LIVE_NAMESPACE, OWED_NAMESPACE};
use crate::live::{HOLD_PREFIX, HoldRow};
use crate::settings::TaskChecksSettings;

/// The name of the source in [`DoneHolds`].
pub const CHECKS_HOLD_SOURCE: &str = "checks";
/// The reason of a held task before the decider wrote one.
pub const CHECKING: &str = "checking";

/// The holds in one project's run state, by task id: every task with a marker and no valid done
/// record, with its reason. A done record that cannot be read does not count.
pub fn read_holds(read: &StateRead) -> Result<BTreeMap<String, String>> {
    let owed = read.kv(OWED_NAMESPACE).scan("")?;
    if owed.is_empty() {
        return Ok(BTreeMap::new());
    }
    let done: BTreeMap<String, toml::Value> =
        read.kv(DONE_NAMESPACE).scan("")?.into_iter().collect();
    let prefix = format!("{HOLD_PREFIX}/");
    let reasons: BTreeMap<String, String> = read
        .kv(LIVE_NAMESPACE)
        .scan(&prefix)?
        .into_iter()
        .filter_map(|(key, value)| {
            let task_id = key.strip_prefix(&prefix)?.to_string();
            match value.try_into::<HoldRow>() {
                Ok(row) => Some((task_id, row.reason)),
                Err(error) => {
                    log::error!("reading the hold reason of {task_id}: {error}");
                    None
                }
            }
        })
        .collect();
    let mut holds = BTreeMap::new();
    for (task_id, _) in owed {
        let valid =
            done.get(&task_id)
                .is_some_and(|value| match value.clone().try_into::<DoneRecord>() {
                    Ok(_) => true,
                    Err(error) => {
                        log::error!("reading the done record of {task_id}: {error}");
                        false
                    }
                });
        if valid {
            continue;
        }
        let reason = reasons
            .get(&task_id)
            .cloned()
            .unwrap_or_else(|| CHECKING.to_string());
        holds.insert(task_id, reason);
    }
    Ok(holds)
}

/// What the `checks` source knows: the holds of each open project, by its id.
///
/// Refreshes may run at once (the store's tree read and a `checks.live` event each start one).
/// Each takes a read number before it lists the open projects or reads any store, and its snapshot replaces the current one
/// only when no read with a larger number has replaced it already. A read with a larger number
/// began after every smaller one began, so it saw every commit that came before their causes; a
/// snapshot read earlier never replaces one read later, and a marker it missed is never lost.
pub struct ChecksHolds {
    repos: OpenProjectRepos,
    holds: Mutex<Snapshot>,
    /// The number the next read takes.
    next_read: AtomicU64,
    /// The writers whose events are followed; weak, so no project's store is kept open.
    followed: Mutex<Vec<Weak<StateWriter>>>,
    /// Where the events of a newly seen writer go to be followed.
    follow: mpsc::UnboundedSender<mpsc::UnboundedReceiver<StateEvent>>,
}

impl ChecksHolds {
    /// Holds over the projects of `repos`; the event streams of writers first seen are sent to
    /// `follow`.
    pub fn new(
        repos: OpenProjectRepos,
        follow: mpsc::UnboundedSender<mpsc::UnboundedReceiver<StateEvent>>,
    ) -> Self {
        Self {
            repos,
            holds: Mutex::new(Snapshot::default()),
            next_read: AtomicU64::new(1),
            followed: Mutex::new(Vec::new()),
            follow,
        }
    }

    /// The reason `task_id` is held, from the snapshot.
    pub fn hold(&self, task_id: &str) -> Option<String> {
        lock(&self.holds)
            .by_project
            .values()
            .find_map(|holds| holds.get(task_id).cloned())
    }

    /// Reads the holds of every open project again. Returns whether any changed.
    pub fn refresh(&self) -> bool {
        // Taken before the projects are listed, so a read that missed a project opened since
        // never replaces one that saw it.
        let number = self.begin_read();
        let handles: Vec<(String, StateHandle)> = self
            .repos
            .list()
            .into_iter()
            .filter_map(|loaded| {
                let repo = loaded.repo();
                Some((repo.location().id.clone(), repo.state()?.clone()))
            })
            .collect();
        self.apply(self.read_from(number, &handles))
    }

    /// Reads the holds of the projects `handles` (project id and its open store), replacing the
    /// snapshot unless a read that began later replaced it first. A project whose read fails
    /// keeps what was read before, so a failed read never lets a held task through. Returns
    /// whether any hold changed.
    pub fn refresh_from(&self, handles: &[(String, StateHandle)]) -> bool {
        let number = self.begin_read();
        self.apply(self.read_from(number, handles))
    }

    /// Takes the number of a read that begins now. A caller takes it before it lists the
    /// projects or reads any store: see the type's comment.
    pub(crate) fn begin_read(&self) -> u64 {
        self.next_read.fetch_add(1, Ordering::SeqCst)
    }

    /// Reads the holds of the projects `handles` as the read `number` (from
    /// [`Self::begin_read`]), without touching the snapshot.
    pub(crate) fn read_from(&self, number: u64, handles: &[(String, StateHandle)]) -> HoldsRead {
        let projects = handles
            .iter()
            .map(|(project, handle)| {
                if let StateHandle::Writer(writer) = handle {
                    self.follow_writer(writer);
                }
                let holds = handle.read(read_holds).map_err(|error| {
                    log::error!("reading the checks holds of project {project}: {error:#}");
                });
                (project.clone(), holds.ok())
            })
            .collect();
        HoldsRead { number, projects }
    }

    /// Replaces the snapshot with `read`, unless a read that began later already replaced it.
    /// Returns whether any hold changed.
    pub(crate) fn apply(&self, read: HoldsRead) -> bool {
        let mut snapshot = lock(&self.holds);
        if read.number <= snapshot.number {
            return false;
        }
        let mut next = BTreeMap::new();
        for (project, holds) in read.projects {
            match holds {
                // A project that holds nothing has no entry, so only holds count as a change.
                Some(holds) if holds.is_empty() => {}
                Some(holds) => {
                    next.insert(project, holds);
                }
                None => {
                    if let Some(kept) = snapshot.by_project.get(&project) {
                        next.insert(project, kept.clone());
                    }
                }
            }
        }
        snapshot.number = read.number;
        let changed = snapshot.by_project != next;
        snapshot.by_project = next;
        changed
    }

    fn follow_writer(&self, writer: &Arc<StateWriter>) {
        let mut followed = lock(&self.followed);
        followed.retain(|known| known.strong_count() > 0);
        if followed
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), Arc::as_ptr(writer)))
        {
            return;
        }
        followed.push(Arc::downgrade(writer));
        self.follow.unbounded_send(writer.subscribe()).ok();
    }

    /// Tells the source that a transaction wrote `namespaces` in some project's run state (in a
    /// process without the writer, as the host relays it). When they include `checks.live`,
    /// the holds are read again and the store told if one changed. The other namespaces that
    /// decide holds refresh the tree, which reads the holds with it. Nothing relays a writer's
    /// events to another process yet, so nothing calls this yet.
    pub fn state_changed(namespaces: &[String], cx: &mut App) {
        if !namespaces
            .iter()
            .any(|namespace| namespace == LIVE_NAMESPACE)
        {
            return;
        }
        let Some(holds) = Self::global(cx) else {
            return;
        };
        cx.spawn(async move |cx| reread(holds, cx).await).detach();
    }

    /// The holds of this app, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Arc<Self>> {
        cx.try_global::<GlobalHolds>()
            .map(|global| global.holds.clone())
    }
}

/// The snapshot of [`ChecksHolds`] and the number of the read it came from.
#[derive(Default)]
struct Snapshot {
    number: u64,
    by_project: BTreeMap<String, BTreeMap<String, String>>,
}

/// One read of the holds, not yet applied: its number and each project's holds (`None` when the
/// read failed).
pub(crate) struct HoldsRead {
    number: u64,
    projects: Vec<(String, Option<BTreeMap<String, String>>)>,
}

/// Reads the holds again on the background executor and tells the store when one changed.
async fn reread(holds: Arc<ChecksHolds>, cx: &mut AsyncApp) {
    let changed = cx.background_spawn(async move { holds.refresh() }).await;
    if changed {
        cx.update(|cx| {
            if DoneHolds::has_sources(cx) {
                DoneHolds::changed(cx);
            }
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl DoneHoldRefresher for ChecksHolds {
    fn refresh(&self) {
        ChecksHolds::refresh(self);
    }
}

/// The source the Work store asks: the snapshot only.
pub struct ChecksHoldSource(pub Arc<ChecksHolds>);

impl DoneHoldSource for ChecksHoldSource {
    fn hold(&self, task_id: &str, _cx: &App) -> Option<String> {
        self.0.hold(task_id)
    }

    /// Nothing is judged before the write yet: the marker the write puts is what holds the task.
    fn would_hold(&self, _task_id: &str, _cx: &mut App) -> gpui::Task<Option<String>> {
        gpui::Task::ready(None)
    }
}

struct GlobalHolds {
    holds: Arc<ChecksHolds>,
    registered: bool,
}

impl Global for GlobalHolds {}

/// Creates the holds of this app, follows the events of the writers it sees, and registers the
/// source and its refresher while `task_checks.enabled` is on (and removes them while it is
/// off: with the setting off nothing is held, and the markers stay for when it is on again).
pub fn init(cx: &mut App) {
    let (follow, mut followed) = mpsc::unbounded();
    let holds = Arc::new(ChecksHolds::new(OpenProjectRepos::global(cx), follow));
    cx.set_global(GlobalHolds {
        holds: holds.clone(),
        registered: false,
    });
    cx.spawn(async move |cx| {
        while let Some(mut events) = followed.next().await {
            let holds = holds.clone();
            cx.spawn(async move |cx| {
                while let Some(StateEvent::Changed { namespaces, .. }) = events.next().await {
                    if namespaces
                        .iter()
                        .any(|namespace| namespace == LIVE_NAMESPACE)
                    {
                        reread(holds.clone(), cx).await;
                    }
                }
            })
            .detach();
        }
    })
    .detach();
    sync_registration(cx);
    cx.observe_global::<SettingsStore>(sync_registration)
        .detach();
}

fn sync_registration(cx: &mut App) {
    let enabled = TaskChecksSettings::get_global(cx).enabled;
    let Some(global) = cx.try_global::<GlobalHolds>() else {
        return;
    };
    if global.registered == enabled {
        return;
    }
    let holds = global.holds.clone();
    cx.global_mut::<GlobalHolds>().registered = enabled;
    if enabled {
        // Read once before the store first asks, so nothing finished is missed in between.
        holds.refresh();
        DoneHolds::add_refresher(CHECKS_HOLD_SOURCE, holds.clone(), cx);
        DoneHolds::add_source(CHECKS_HOLD_SOURCE, Rc::new(ChecksHoldSource(holds)), cx);
    } else {
        DoneHolds::remove_refresher(CHECKS_HOLD_SOURCE, cx);
        DoneHolds::remove_source(CHECKS_HOLD_SOURCE, cx);
    }
}
