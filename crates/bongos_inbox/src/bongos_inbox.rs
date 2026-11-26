//! The inbox in the app: the `inbox` settings and the global [`Inbox`], an entity that scans the
//! inbox directory (`<bongos root>/inbox`) on the background executor, watches it for deliveries
//! and moves made by other processes, and marks items read or unread and archives them.
//!
//! While `inbox.enabled` is true the layout is created when missing, the root is watched and
//! `new/` and `cur/` are added to the watcher (the Linux watcher is not recursive). `tmp/` and
//! `archive/` are not watched, and the scan never reads `tmp/`, so a file still being written
//! never shows. An event under `new/` or `cur/` rescans; an event for the root, `new/` or `cur/`
//! itself (someone deleted and recreated the directory) adds them to the watcher again and
//! rescans. A periodic rescan, every [`RESCAN_INTERVAL`], is the safety net for events the
//! watcher missed. Rescans never overlap: triggers that arrive while one runs are merged into at
//! most one more.
//!
//! Operations go through the same queue as scans, so one background task runs at a time. An
//! operation names its item by unique and inode, finds it in the latest snapshot, checks the file
//! is still there, changes it and scans at once, so its row changes before the watcher event
//! arrives; the rescan that event causes finds the same names and changes nothing. An item moved
//! or removed by hand is reported and left alone.
//!
//! Every scan also resolves each item's `target` (see [`targets`]) to tell stale rows.

mod badge;
#[cfg(debug_assertions)]
mod dump;
mod inbox_settings;
pub mod rows;
pub mod targets;
mod view;

use bongos_inbox_model::format::Item;
use bongos_inbox_model::maildir::{self, ARCHIVE, CUR, Entry, NEW, ParseCache, ParseResult, TMP};
use bongos_inbox_model::view_model::{self, Filter, Row};
use bongos_work_model::store::WorkTree;
use command_palette_hooks::CommandPaletteFilter;
use fs::{Fs, PathEvent, PathEventKind, Watcher};
use futures::channel::{mpsc, oneshot};
use futures::stream::{BoxStream, FusedStream};
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, EventEmitter, Global,
    Subscription, Task, WeakEntity, actions,
};
use parking_lot::Mutex;
use settings::{Settings as _, SettingsStore};
use std::any::TypeId;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use targets::PreparedResolvers;
use util::ResultExt as _;
use workspace::Workspace;

pub use badge::{InboxBadge, badge_label};
pub use inbox_settings::InboxSettings;
pub use targets::{TargetResolvers, TargetState};
pub use view::{Clock, InboxView, now, set_clock};

#[cfg(debug_assertions)]
pub use dump::{DUMP_FILE, DumpDir};

/// How long the watcher gathers events before delivering them.
pub const WATCH_LATENCY: Duration = Duration::from_millis(200);
/// How often the inbox is scanned again even without watcher events.
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
/// What an operation on an item that is no longer where the last scan saw it reports.
pub const MOVED_MESSAGE: &str = "item moved outside bongos";

/// What opening the inbox says while it is off.
pub const OFF_MESSAGE: &str = "The inbox is off (inbox.enabled)";

actions!(
    inbox,
    [
        /// Opens the inbox, or focuses it when this window shows it already.
        Open,
        /// Moves the inbox cursor to the next row.
        Next,
        /// Moves the inbox cursor to the previous row.
        Previous,
        /// Scans the inbox directory again.
        Refresh,
        /// Marks the item under the inbox cursor read.
        OpenItem,
        /// Marks the item under the inbox cursor unread.
        MarkUnread,
    ]
);

#[cfg(debug_assertions)]
actions!(
    inbox,
    [
        /// Debug builds only: writes the unread count and the rows of the inbox (of this window's
        /// inbox view when it has one) to `inbox-view.txt` in the logs directory.
        DumpView
    ]
);

/// Where the inbox lives: `<bongos root>/inbox` and its four subdirectories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxDirs {
    pub root: PathBuf,
    pub new: PathBuf,
    pub cur: PathBuf,
    pub tmp: PathBuf,
    pub archive: PathBuf,
}

impl InboxDirs {
    /// The inbox under a bongos root (the app passes `~/.bongos`; tests pass a temporary
    /// directory).
    pub fn new(bongos_root: &Path) -> Self {
        Self::at(bongos_root.join("inbox"))
    }

    /// The inbox rooted at `root` itself.
    pub fn at(root: PathBuf) -> Self {
        Self {
            new: root.join(NEW),
            cur: root.join(CUR),
            tmp: root.join(TMP),
            archive: root.join(ARCHIVE),
            root,
        }
    }
}

/// The items in `new/` and `cur/` as last scanned, each with its parse result and what its target
/// resolved to. An unreadable file stays in the list with an unreadable result.
#[derive(Debug, Default, PartialEq)]
pub struct Snapshot {
    items: Vec<(Entry, ParseResult)>,
    /// One per item: `None` for an item without a target and for an unreadable file.
    targets: Vec<Option<TargetState>>,
}

impl Snapshot {
    /// The items sorted by path, in the shape the view model takes.
    pub fn items(&self) -> &view_model::Snapshot {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The parsed item at `index`, `None` for an unreadable file.
    pub fn item(&self, index: usize) -> Option<&Item> {
        self.items
            .get(index)
            .and_then(|(_, result)| result.as_ref().as_ref().ok())
    }

    /// What the target of the item at `index` resolved to, `None` when it has none.
    pub fn target(&self, index: usize) -> Option<TargetState> {
        self.targets.get(index).copied().flatten()
    }

    /// The item named by `unique` and `inode`.
    pub fn find(&self, unique: &str, inode: u64) -> Option<&Entry> {
        self.items
            .iter()
            .map(|(entry, _)| entry)
            .find(|entry| entry.unique == unique && entry.inode == inode)
    }

    /// The items that are read (a name with the `S` flag outside `new/`).
    pub fn read(&self) -> impl Iterator<Item = &Entry> {
        self.items
            .iter()
            .map(|(entry, _)| entry)
            .filter(|entry| entry.is_read())
    }

    /// The items that are not read.
    pub fn unread(&self) -> impl Iterator<Item = &Entry> {
        self.items
            .iter()
            .map(|(entry, _)| entry)
            .filter(|entry| !entry.is_read())
    }

    /// How many unread items the view model counts.
    pub fn unread_count(&self) -> usize {
        view_model::unread_count(&self.items)
    }

    /// The visible rows, with stale items marked.
    pub fn rows(&self, filter: &Filter, known: &dyn Fn(&str) -> bool) -> Vec<Row> {
        let stale: HashSet<(&str, u64)> = self
            .items
            .iter()
            .zip(&self.targets)
            .filter(|(_, target)| **target == Some(TargetState::Stale))
            .map(|((entry, _), _)| (entry.unique.as_str(), entry.inode))
            .collect();
        view_model::rows_marked(&self.items, filter, known, &|entry| {
            stale.contains(&(entry.unique.as_str(), entry.inode))
        })
    }
}

/// The ticks of the periodic rescan, made once per enabled period.
pub type RescanTimer = Arc<dyn Fn(&BackgroundExecutor) -> BoxStream<'static, ()> + Send + Sync>;

/// A tick every `interval` on the executor's timer.
pub fn periodic_rescan(interval: Duration) -> RescanTimer {
    Arc::new(move |executor: &BackgroundExecutor| {
        futures::stream::unfold(executor.clone(), move |executor| async move {
            executor.timer(interval).await;
            Some(((), executor))
        })
        .boxed()
    })
}

/// Shows a problem with an operation to the user.
pub type Reporter = Rc<dyn Fn(&str, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxEvent {
    /// The snapshot changed, or went away because the inbox was turned off.
    Changed,
}

/// A change to one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    MarkRead,
    MarkUnread,
    Archive,
}

impl Operation {
    fn verb(self) -> &'static str {
        match self {
            Operation::MarkRead => "mark read",
            Operation::MarkUnread => "mark unread",
            Operation::Archive => "archive",
        }
    }
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    /// Done; the item's path afterwards.
    Done(PathBuf),
    /// The item was not where the last scan saw it (moved or removed by hand, or by another
    /// process first): reported, rescanned, nothing changed.
    Moved,
    /// The file system refused; reported and logged.
    Failed(String),
    /// The inbox is off, or was turned off before the operation ran.
    Off,
}

/// Counters for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Watchers set up, one per enabled period.
    pub watches: usize,
    /// Scans that finished, including the scan after each operation.
    pub scans: usize,
    /// Times `new/` and `cur/` were added to the watcher again after a root-level event.
    pub rewatches: usize,
    /// Operations that ran, whatever their outcome.
    pub operations: usize,
}

/// The inbox state behind the global [`Inbox`]. Creates, reads and watches nothing while
/// `inbox.enabled` is false.
pub struct InboxState {
    fs: Arc<dyn Fs>,
    dirs: InboxDirs,
    rescan_timer: RescanTimer,
    enabled: bool,
    snapshot: Option<Arc<Snapshot>>,
    session: Option<Session>,
    stats: Stats,
    resolvers: TargetResolvers,
    reporter: Reporter,
    _settings: Subscription,
}

/// An enabled period: the task that watches, scans and runs operations, and its requests.
/// Dropping it cancels the task, which drops the watcher and the rescan timer.
struct Session {
    requests: mpsc::UnboundedSender<Request>,
    /// Cleared when the period ends, so that work already on the background executor creates
    /// no directory after the inbox was turned off.
    live: Arc<AtomicBool>,
    _run: Task<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.live.store(false, Ordering::SeqCst);
    }
}

enum Request {
    Refresh,
    Operate(OpRequest),
}

struct OpRequest {
    operation: Operation,
    unique: String,
    inode: u64,
    done: oneshot::Sender<OpOutcome>,
}

impl EventEmitter<InboxEvent> for InboxState {}

impl InboxState {
    pub fn new(
        fs: Arc<dyn Fs>,
        dirs: InboxDirs,
        rescan_timer: RescanTimer,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = cx.observe_global::<SettingsStore>(|this, cx| this.sync_enabled(cx));
        let mut resolvers = TargetResolvers::default();
        resolvers.set_worktree_roots(open_worktree_roots);
        let mut this = Self {
            fs,
            dirs,
            rescan_timer,
            enabled: false,
            snapshot: None,
            session: None,
            stats: Stats::default(),
            resolvers,
            reporter: Rc::new(report_in_minibuffer),
            _settings: settings,
        };
        this.sync_enabled(cx);
        this
    }

    pub fn dirs(&self) -> &InboxDirs {
        &self.dirs
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a watcher is set up (only while enabled).
    pub fn is_watching(&self) -> bool {
        self.session.is_some()
    }

    /// The items as last scanned; `None` while disabled or before the first scan finishes.
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.clone()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The target resolvers; changes apply from the next scan.
    pub fn resolvers_mut(&mut self) -> &mut TargetResolvers {
        &mut self.resolvers
    }

    /// Where problems with operations are shown (the minibuffer of the active window unless
    /// replaced).
    pub fn set_reporter(&mut self, reporter: impl Fn(&str, &mut App) + 'static) {
        self.reporter = Rc::new(reporter);
    }

    /// Scans again soon, merged with any rescan already pending. Does nothing while disabled.
    pub fn refresh(&mut self) {
        if let Some(session) = &self.session {
            session.requests.unbounded_send(Request::Refresh).ok();
        }
    }

    /// Marks the item `unique` with inode `inode` read; see [`InboxState::operate`].
    pub fn mark_read(
        &mut self,
        unique: &str,
        inode: u64,
        cx: &mut Context<Self>,
    ) -> Task<OpOutcome> {
        self.operate(Operation::MarkRead, unique, inode, cx)
    }

    /// Marks the item `unique` with inode `inode` unread; see [`InboxState::operate`].
    pub fn mark_unread(
        &mut self,
        unique: &str,
        inode: u64,
        cx: &mut Context<Self>,
    ) -> Task<OpOutcome> {
        self.operate(Operation::MarkUnread, unique, inode, cx)
    }

    /// Archives the item `unique` with inode `inode`; see [`InboxState::operate`].
    pub fn archive(&mut self, unique: &str, inode: u64, cx: &mut Context<Self>) -> Task<OpOutcome> {
        self.operate(Operation::Archive, unique, inode, cx)
    }

    /// Queues `operation` on the item named by `unique` and `inode`. Operations run one at a time,
    /// in the order asked, each followed at once by a scan; the task resolves after that scan is
    /// in the snapshot.
    pub fn operate(
        &mut self,
        operation: Operation,
        unique: &str,
        inode: u64,
        cx: &mut Context<Self>,
    ) -> Task<OpOutcome> {
        let (done, outcome) = oneshot::channel();
        if let Some(session) = &self.session {
            session
                .requests
                .unbounded_send(Request::Operate(OpRequest {
                    operation,
                    unique: unique.to_string(),
                    inode,
                    done,
                }))
                .ok();
        }
        cx.background_spawn(async move { outcome.await.unwrap_or(OpOutcome::Off) })
    }

    fn sync_enabled(&mut self, cx: &mut Context<Self>) {
        let enabled = InboxSettings::get_global(cx).enabled;
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            let (requests, receiver) = mpsc::unbounded();
            let live = Arc::new(AtomicBool::new(true));
            let session = SessionParts {
                fs: self.fs.clone(),
                dirs: self.dirs.clone(),
                timer: self.rescan_timer.clone(),
                live: live.clone(),
            };
            let run = cx.spawn(async move |this, cx| run(this, session, receiver, cx).await);
            self.session = Some(Session {
                requests,
                live,
                _run: run,
            });
        } else {
            self.session = None;
            if self.snapshot.take().is_some() {
                cx.emit(InboxEvent::Changed);
            }
            cx.notify();
        }
    }

    fn watching(&mut self, cx: &mut Context<Self>) {
        self.stats.watches += 1;
        cx.notify();
    }

    fn finish(&mut self, scan: ScanDone, cx: &mut Context<Self>) {
        self.stats.scans += 1;
        if scan.rewatched {
            self.stats.rewatches += 1;
        }
        // The stats changed even when the snapshot did not.
        cx.notify();
        let snapshot = match scan.scanned {
            Scanned::Unchanged => return,
            Scanned::Changed(snapshot) => snapshot,
            // A failed first scan still says the inbox is on: it shows empty.
            Scanned::Failed if self.snapshot.is_none() => Snapshot::default(),
            Scanned::Failed => return,
        };
        self.snapshot = Some(Arc::new(snapshot));
        cx.emit(InboxEvent::Changed);
    }

    fn finish_operation(
        &mut self,
        operation: Operation,
        outcome: &OpOutcome,
        scan: ScanDone,
        cx: &mut Context<Self>,
    ) {
        self.stats.operations += 1;
        if matches!(scan.scanned, Scanned::Failed) {
            log::warn!(
                "inbox: the scan after {} failed; the rows may be out of date",
                operation.verb()
            );
        }
        self.finish(scan, cx);
        let message = match outcome {
            OpOutcome::Moved => MOVED_MESSAGE.to_string(),
            OpOutcome::Failed(error) => format!("cannot {} the item: {error}", operation.verb()),
            OpOutcome::Done(_) | OpOutcome::Off => return,
        };
        let reporter = self.reporter.clone();
        reporter(&message, cx);
    }
}

/// Shows `text` as an error in the active window's minibuffer, or logs it without a window.
fn report_in_minibuffer(text: &str, cx: &mut App) {
    let shown = cx.active_window().is_some_and(|window| {
        window
            .update(cx, |_, window, cx| {
                bongos_minibuffer::error(window, cx, text.to_string());
            })
            .is_ok()
    });
    if !shown {
        log::error!("inbox: {text}");
    }
}

/// The roots of the visible worktrees of every open workspace.
fn open_worktree_roots(cx: &App) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let Ok(multi) = window.read(cx) else {
            continue;
        };
        for workspace in multi.workspaces() {
            for worktree in workspace.read(cx).visible_worktrees(cx) {
                roots.push(worktree.read(cx).abs_path().to_path_buf());
            }
        }
    }
    roots
}

/// The global inbox, set by [`init`].
pub struct Inbox(pub Entity<InboxState>);

impl Global for Inbox {}

impl Inbox {
    pub fn global(cx: &App) -> Option<Entity<InboxState>> {
        cx.try_global::<Inbox>().map(|inbox| inbox.0.clone())
    }
}

/// Registers the `inbox` settings and creates the global [`Inbox`], which does nothing until
/// `inbox.enabled` is true.
pub fn init(dirs: InboxDirs, fs: Arc<dyn Fs>, cx: &mut App) {
    InboxSettings::register(cx);
    let state = cx.new(|cx| InboxState::new(fs, dirs, periodic_rescan(RESCAN_INTERVAL), cx));
    cx.set_global(Inbox(state));
    register_actions(cx);
}

/// Registers the workspace-level handlers on every workspace: `inbox: open`, `inbox: refresh`
/// and, in debug builds, `inbox: dump view`. The cursor and read actions are the view's.
fn register_actions(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &Open, window, cx| view::open(workspace, window, cx))
        .register_action(|_, _: &Refresh, _, cx| {
            if let Some(inbox) = Inbox::global(cx) {
                inbox.update(cx, |inbox, _| inbox.refresh());
            }
        });
    #[cfg(debug_assertions)]
    workspace.register_action(|workspace, _: &DumpView, window, cx| {
        dump::dump_view(workspace, window, cx)
    });
}

/// The inbox's actions, which the command palette shows only while `inbox.enabled` is true.
pub fn action_types() -> Vec<TypeId> {
    #[allow(unused_mut)]
    let mut types = vec![
        TypeId::of::<Open>(),
        TypeId::of::<Next>(),
        TypeId::of::<Previous>(),
        TypeId::of::<Refresh>(),
        TypeId::of::<OpenItem>(),
        TypeId::of::<MarkUnread>(),
    ];
    #[cfg(debug_assertions)]
    types.push(TypeId::of::<DumpView>());
    types
}

/// Hides the inbox's actions from the command palette while `inbox.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init`.
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = InboxSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// Tells the global inbox's `task:` and `run:` resolvers where to read the work tree.
pub fn set_work_tree(source: impl Fn(&App) -> Option<Arc<WorkTree>> + 'static, cx: &mut App) {
    if let Some(inbox) = Inbox::global(cx) {
        inbox.update(cx, |state, _| state.resolvers.set_work_tree(source));
    }
}

/// What one wait for triggers asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Trigger {
    rescan: bool,
    rewatch: bool,
    /// The root itself was deleted and recreated: watch it again too.
    rewatch_root: bool,
}

impl Trigger {
    fn add_events(&mut self, root: &Path, events: &[PathEvent]) {
        for event in events {
            let watched_dir = |path: &Path| path == root.join(NEW) || path == root.join(CUR);
            if event.path == root {
                self.rewatch_root = true;
            }
            if event.kind == Some(PathEventKind::Rescan)
                || event.path == root
                || watched_dir(&event.path)
            {
                self.rewatch = true;
                self.rescan = true;
            } else if event.path.parent().is_some_and(watched_dir) {
                self.rescan = true;
            }
        }
    }

    fn any(&self) -> bool {
        self.rescan || self.rewatch
    }
}

/// The triggers and operations waiting for the next background task.
#[derive(Default)]
struct Pending {
    trigger: Trigger,
    operations: VecDeque<OpRequest>,
}

impl Pending {
    fn add(&mut self, request: Request) {
        match request {
            Request::Refresh => self.trigger.rescan = true,
            Request::Operate(operation) => self.operations.push_back(operation),
        }
    }

    fn any(&self) -> bool {
        self.trigger.any() || !self.operations.is_empty()
    }
}

enum Scanned {
    Failed,
    /// Equal to the snapshot the scan was given.
    Unchanged,
    Changed(Snapshot),
}

struct ScanDone {
    scanned: Scanned,
    rewatched: bool,
}

/// What a session needs to start.
struct SessionParts {
    fs: Arc<dyn Fs>,
    dirs: InboxDirs,
    timer: RescanTimer,
    live: Arc<AtomicBool>,
}

/// The session: set up the layout and the watcher, scan, then wait for triggers and operations
/// and handle them, one background task at a time, until the session or the state goes away.
async fn run(
    this: WeakEntity<InboxState>,
    parts: SessionParts,
    mut requests: mpsc::UnboundedReceiver<Request>,
    cx: &mut AsyncApp,
) {
    let SessionParts {
        fs,
        dirs,
        timer,
        live,
    } = parts;
    let background = cx.background_executor().clone();
    let setup = background
        .spawn({
            let live = live.clone();
            async move {
                // The inbox may have been turned off since this task was queued.
                if !live.load(Ordering::SeqCst) {
                    return None;
                }
                ensure_layout(&dirs.root);
                // Resolved once, so that event paths compare with the watched spelling.
                let root = std::fs::canonicalize(&dirs.root).unwrap_or(dirs.root);
                let (events, watcher) = fs.watch(&root, WATCH_LATENCY).await;
                for dir in [NEW, CUR] {
                    watcher.add(&root.join(dir)).log_err();
                }
                Some((root, events, watcher))
            }
        })
        .await;
    let Some((root, events, watcher)) = setup else {
        return;
    };
    if this.update(cx, |state, cx| state.watching(cx)).is_err() {
        return;
    }
    let mut events = events.fuse();
    let mut ticks = timer(&background).fuse();
    let cache = Arc::new(Mutex::new(ParseCache::new()));
    let mut pending = Pending::default();
    pending.trigger.rescan = true;
    loop {
        if !pending.any() {
            select_biased! {
                request = requests.next() => match request {
                    Some(request) => pending.add(request),
                    None => return,
                },
                batch = events.next() => match batch {
                    Some(batch) => pending.trigger.add_events(&root, &batch),
                    None => log::error!(
                        "the inbox watcher stopped; only the periodic rescan sees changes now"
                    ),
                },
                () = ticks.select_next_some() => pending.trigger.rescan = true,
                complete => return,
            }
            // Whatever else arrived meanwhile joins this one rescan.
            if !drain(&mut pending, &root, &mut requests, &mut events, &mut ticks) {
                return;
            }
            continue;
        }
        let Ok((previous, resolvers)) = this.update(cx, |state, cx| {
            (state.snapshot.clone(), state.resolvers.prepare(cx))
        }) else {
            return;
        };
        // Operations wait for the first scan, which they find their items in.
        let request = if previous.is_some() {
            pending.operations.pop_front()
        } else {
            None
        };
        let operation = request
            .as_ref()
            .map(|request| (request.operation, request.unique.clone(), request.inode));
        // An operation scans afterwards, which serves any pending rescan as well.
        let trigger = std::mem::take(&mut pending.trigger);
        let (outcome, done) = background
            .spawn({
                let root = root.clone();
                let watcher = watcher.clone();
                let cache = cache.clone();
                let live = live.clone();
                async move {
                    if trigger.rewatch {
                        rewatch(&root, watcher.as_ref(), trigger.rewatch_root, &live);
                    }
                    let outcome = operation.map(|(operation, unique, inode)| {
                        operate(&root, operation, &unique, inode, previous.as_deref())
                    });
                    let done = ScanDone {
                        scanned: scan(&root, &mut cache.lock(), &resolvers, previous.as_deref()),
                        rewatched: trigger.rewatch,
                    };
                    (outcome, done)
                }
            })
            .await;
        let updated = match (request, outcome) {
            (Some(request), Some(outcome)) => {
                let updated = this.update(cx, |state, cx| {
                    state.finish_operation(request.operation, &outcome, done, cx)
                });
                request.done.send(outcome).ok();
                updated
            }
            _ => this.update(cx, |state, cx| state.finish(done, cx)),
        };
        if updated.is_err() {
            return;
        }
    }
}

/// Takes every trigger and operation that is ready without waiting. Returns false when the
/// session ended.
fn drain(
    pending: &mut Pending,
    root: &Path,
    requests: &mut mpsc::UnboundedReceiver<Request>,
    events: &mut (impl FusedStream<Item = Vec<PathEvent>> + Unpin),
    ticks: &mut (impl FusedStream<Item = ()> + Unpin),
) -> bool {
    loop {
        let mut took = false;
        match requests.next().now_or_never() {
            Some(Some(request)) => {
                pending.add(request);
                took = true;
            }
            Some(None) => return false,
            None => {}
        }
        if !events.is_terminated()
            && let Some(Some(batch)) = events.next().now_or_never()
        {
            pending.trigger.add_events(root, &batch);
            took = true;
        }
        if !ticks.is_terminated()
            && let Some(Some(())) = ticks.next().now_or_never()
        {
            pending.trigger.rescan = true;
            took = true;
        }
        if !took {
            return true;
        }
    }
}

fn ensure_layout(root: &Path) {
    if let Err(error) = maildir::ensure_layout(root) {
        log::error!("cannot create the inbox at {}: {error}", root.display());
    }
}

/// Creates the layout again if parts of it went away, and adds `new/` and `cur/` (and the root,
/// with `with_root`) to the watcher afresh: a directory deleted and recreated is a new
/// directory, which the old registration no longer watches.
fn rewatch(root: &Path, watcher: &dyn Watcher, with_root: bool, live: &AtomicBool) {
    if !live.load(Ordering::SeqCst) {
        return;
    }
    ensure_layout(root);
    if with_root {
        watcher.remove(root).log_err();
        watcher.add(root).log_err();
    }
    for dir in [NEW, CUR] {
        let path = root.join(dir);
        watcher.remove(&path).log_err();
        watcher.add(&path).log_err();
    }
}

/// Runs `operation` on the item named by `unique` and `inode` in `snapshot`, unless its file is
/// no longer where the snapshot saw it.
fn operate(
    root: &Path,
    operation: Operation,
    unique: &str,
    inode: u64,
    snapshot: Option<&Snapshot>,
) -> OpOutcome {
    let Some(entry) = snapshot.and_then(|snapshot| snapshot.find(unique, inode)) else {
        return OpOutcome::Moved;
    };
    // Checked again right before the link: someone may have moved or removed it since the scan.
    match std::fs::symlink_metadata(&entry.path) {
        Ok(meta) if meta.ino() == entry.inode && meta.dev() == entry.dev() => {}
        _ => return OpOutcome::Moved,
    }
    let result = match operation {
        Operation::MarkRead => maildir::mark_read(root, entry),
        Operation::MarkUnread => maildir::mark_unread(root, entry),
        Operation::Archive => maildir::archive(root, entry),
    };
    match result {
        Ok(path) => OpOutcome::Done(path),
        Err(error) => {
            let outcome = outcome_of_error(operation, &error);
            if outcome == OpOutcome::Moved {
                log::warn!(
                    "inbox: cannot {} {}: {error}",
                    operation.verb(),
                    entry.file_name()
                );
            } else {
                log::error!(
                    "inbox: cannot {} {}: {error}",
                    operation.verb(),
                    entry.file_name()
                );
            }
            outcome
        }
    }
}

/// How an error from the file system ends `operation`. Another process moving the file first
/// (`NotFound`), or winning the lock and changing its names (`AlreadyExists` of a read or unread),
/// is a move. `AlreadyExists` from an archive means no free name in `archive/`: the item is
/// still where it was, so that is a failure to report as such.
fn outcome_of_error(operation: Operation, error: &io::Error) -> OpOutcome {
    match error.kind() {
        io::ErrorKind::NotFound => OpOutcome::Moved,
        io::ErrorKind::AlreadyExists if operation != Operation::Archive => OpOutcome::Moved,
        _ => OpOutcome::Failed(error.to_string()),
    }
}

/// Scans `new/` and `cur/`, parses what changed since the last scan and resolves the targets.
/// Compares the result with `previous` here, off the main thread. Errors name files only, never
/// item text.
fn scan(
    root: &Path,
    cache: &mut ParseCache,
    resolvers: &PreparedResolvers,
    previous: Option<&Snapshot>,
) -> Scanned {
    let entries = match maildir::scan(root) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("cannot scan the inbox at {}: {error}", root.display());
            return Scanned::Failed;
        }
    };
    let results = cache.parse(&entries);
    let targets = results
        .iter()
        .map(|result| {
            let target = result.as_ref().as_ref().ok()?.target.as_deref()?;
            Some(resolvers.resolve(target))
        })
        .collect();
    let snapshot = Snapshot {
        items: entries.into_iter().zip(results).collect(),
        targets,
    };
    if previous == Some(&snapshot) {
        Scanned::Unchanged
    } else {
        Scanned::Changed(snapshot)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod badge_tests;

#[cfg(test)]
mod view_tests;
