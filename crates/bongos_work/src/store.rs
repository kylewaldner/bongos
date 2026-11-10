//! [`WorkStore`]: the loaded work tree, kept in step with the disk.
//!
//! All disk work happens in one session task per enabled period, one job at a time, each job on
//! the background executor: the first load, a reload after watcher events, and each write. So a
//! write never overlaps a reload and snapshots are published in the order they were read. A
//! process-wide lock also keeps the jobs of an old session (still finishing after `work.enabled`
//! went off and on again) from overlapping the new session's.
//!
//! Watching (D6): the root is watched with [`WATCH_LATENCY`], and every directory of the tree is
//! added to the watcher, because the Linux watcher is not recursive. Events for temporary files
//! and journals are ignored, and so are echoes of the store's own writes: after a write the store
//! records the state it left each touched path in, and an event on a path still in that state is
//! dropped. Any other event reloads the tree.
//!
//! Writing (D7): a command runs against the current snapshot. Before its change is written, every
//! file it replaces is reread and compared with the hash the snapshot was parsed from. If another
//! writer changed one, the tree is reloaded and the command applied again to the fresh state
//! (which may now refuse it), up to [`MAX_RETRIES`] times, so an external edit is never
//! overwritten by a stale copy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bongos_work_model::commands::{self, Change, Context, Outcome};
use bongos_work_model::files::content_hash;
use bongos_work_model::layout::{self, OP_FILE, TASKS_DIR, is_temp_file};
use bongos_work_model::rules::Refusal;
use bongos_work_model::store::{self as model_store, WorkTree};
use fs::{Fs, PathEvent, PathEventKind, Watcher};
use futures::channel::{mpsc, oneshot};
use futures::{StreamExt as _, select_biased};
use gpui::{AppContext as _, AsyncApp, EventEmitter, Subscription, Task, WeakEntity};
use parking_lot::Mutex;
use settings::{Settings as _, SettingsStore};
use util::ResultExt as _;

use crate::changes::ChangedCount;
use crate::rebase::{CHECK_INTERVAL, RebaseState};
use crate::{WorkDirs, WorkSettings};

/// How long the watcher gathers events before delivering them.
pub const WATCH_LATENCY: Duration = Duration::from_millis(200);

/// How many times a write is reapplied after its files changed under it.
pub const MAX_RETRIES: usize = 3;

/// How long the state a write left a path in is remembered for the self-echo filter.
pub const ECHO_TTL: Duration = Duration::from_secs(10);

/// A command as the writer runs it: the pure function from the tree to a change, which it may
/// call again on a fresh tree.
pub type WorkCommand = Box<dyn Fn(&WorkTree, &Context) -> Outcome + Send + 'static>;

/// Why a write did not happen.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteError {
    /// `work.enabled` is false (or went false before the write ran).
    Off,
    /// The command refused, with the reason to show.
    Refused(Refusal),
    /// The files kept changing under the writer; nothing was written.
    Contended,
    /// Reading or writing the files failed.
    Failed(String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Off => write!(f, "The Work view is off (work.enabled)"),
            WriteError::Refused(reason) => write!(f, "{reason}"),
            WriteError::Contended => write!(
                f,
                "The task's files kept changing while bongos tried to write them; try again"
            ),
            WriteError::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Counters of what the store did, for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Loads at the start of a session.
    pub loads: usize,
    /// Reloads caused by watcher events.
    pub event_reloads: usize,
    /// Event batches dropped as temporary files, journals or echoes.
    pub ignored_batches: usize,
    /// Writes done.
    pub writes: usize,
    /// Times a command was applied again because its files changed under it.
    pub retries: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkStoreEvent {
    /// The snapshot changed (or went away).
    Changed,
    /// The rebase and retarget states of stacked tasks changed.
    RebaseChanged,
    /// The tasks that sources say need the owner changed.
    NeedsYouChanged,
    /// A live-modes source was added or removed, or said its answer changed.
    LiveModesChanged,
    /// A changed-file count changed.
    ChangedFilesChanged,
}

/// The global work tree. Reads nothing while `work.enabled` is false.
pub struct WorkStore {
    fs: Arc<dyn Fs>,
    dirs: WorkDirs,
    enabled: bool,
    snapshot: Option<Arc<WorkTree>>,
    session: Option<Session>,
    disk_lock: Arc<Mutex<()>>,
    stats: Stats,
    pub(crate) rebase: HashMap<String, RebaseState>,
    pub(crate) rebase_check: Option<Task<()>>,
    /// Tasks that need the owner, by id, with the reasons sources gave (D13). Never stored.
    pub(crate) needs_you: BTreeMap<String, Vec<String>>,
    pub(crate) changed: HashMap<PathBuf, ChangedCount>,
    pub(crate) status_runs: usize,
    _settings: Subscription,
}

/// An enabled period: the queue of writes and the task doing all disk work.
struct Session {
    requests: mpsc::UnboundedSender<WriteRequest>,
    /// Cleared when the session ends, so a write that has not started yet never starts.
    alive: Arc<AtomicBool>,
    _run: Task<()>,
    /// Rechecks the rebase states every [`CHECK_INTERVAL`].
    _rebase_timer: Task<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

struct WriteRequest {
    ctx: Context,
    command: WorkCommand,
    reply: oneshot::Sender<Result<Change, WriteError>>,
}

impl EventEmitter<WorkStoreEvent> for WorkStore {}

impl WorkStore {
    pub fn new(fs: Arc<dyn Fs>, dirs: WorkDirs, cx: &mut gpui::Context<Self>) -> Self {
        let settings = cx.observe_global::<SettingsStore>(|this, cx| this.sync_enabled(cx));
        let mut this = Self {
            fs,
            dirs,
            enabled: false,
            snapshot: None,
            session: None,
            disk_lock: Arc::new(Mutex::new(())),
            stats: Stats::default(),
            rebase: HashMap::new(),
            rebase_check: None,
            needs_you: BTreeMap::new(),
            changed: HashMap::new(),
            status_runs: 0,
            _settings: settings,
        };
        this.sync_enabled(cx);
        this
    }

    /// The root as configured. Snapshot paths are under its canonical form (symbolic links
    /// resolved), which is also the path that is watched.
    pub fn root(&self) -> &Path {
        &self.dirs.root
    }

    /// The file system the store reads and writes through.
    pub fn fs(&self) -> Arc<dyn Fs> {
        self.fs.clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The tree as last read, `None` while disabled or before the first load finishes.
    pub fn snapshot(&self) -> Option<Arc<WorkTree>> {
        self.snapshot.clone()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Queues `command` on the serial writer. The returned task resolves once the change is
    /// written and the snapshot includes it, or with why it was not.
    pub fn write(
        &mut self,
        ctx: Context,
        command: impl Fn(&WorkTree, &Context) -> Outcome + Send + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<Change, WriteError>> {
        let Some(session) = &self.session else {
            return Task::ready(Err(WriteError::Off));
        };
        let (reply, response) = oneshot::channel();
        let request = WriteRequest {
            ctx,
            command: Box::new(command),
            reply,
        };
        if session.requests.unbounded_send(request).is_err() {
            return Task::ready(Err(WriteError::Off));
        }
        cx.background_spawn(async move { response.await.unwrap_or(Err(WriteError::Off)) })
    }

    fn sync_enabled(&mut self, cx: &mut gpui::Context<Self>) {
        let enabled = WorkSettings::get_global(cx).enabled;
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.start(cx);
        } else {
            // Dropping the session cancels its task and the watcher; queued writes answer `Off`.
            self.session = None;
            self.rebase_check = None;
            self.set_rebase(HashMap::new(), cx);
            if self.snapshot.take().is_some() {
                cx.emit(WorkStoreEvent::Changed);
            }
            cx.notify();
        }
    }

    fn start(&mut self, cx: &mut gpui::Context<Self>) {
        let (requests, receiver) = mpsc::unbounded();
        let fs = self.fs.clone();
        let root = self.dirs.root.clone();
        let lock = self.disk_lock.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let run = cx.spawn({
            let alive = alive.clone();
            async move |this, cx| run(this, fs, root, lock, alive, receiver, cx).await
        });
        let rebase_timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHECK_INTERVAL).await;
                if this
                    .update(cx, |store, cx| store.refresh_rebase(cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        self.session = Some(Session {
            requests,
            alive,
            _run: run,
            _rebase_timer: rebase_timer,
        });
    }

    fn finish(&mut self, done: Done, cx: &mut gpui::Context<Self>) {
        match done.kind {
            DoneKind::Loaded => self.stats.loads += 1,
            DoneKind::Reloaded => self.stats.event_reloads += 1,
            DoneKind::Ignored => self.stats.ignored_batches += 1,
            DoneKind::Wrote { retries } => {
                self.stats.retries += retries;
                if done
                    .reply
                    .as_ref()
                    .is_some_and(|reply| matches!(reply.result, Some(Ok(_))))
                {
                    self.stats.writes += 1;
                }
            }
        }
        if let Some(tree) = done.tree
            && !self
                .snapshot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &tree))
        {
            self.snapshot = Some(tree);
            cx.emit(WorkStoreEvent::Changed);
            cx.notify();
            self.refresh_rebase(cx);
        }
        // After the snapshot, so a caller that awaits the write sees its change.
        if let Some(reply) = done.reply {
            reply.send();
        }
    }
}

/// The session: watch the root, load, then run one job at a time until the store or the session
/// goes away.
async fn run(
    this: WeakEntity<WorkStore>,
    fs: Arc<dyn Fs>,
    root: PathBuf,
    lock: Arc<Mutex<()>>,
    alive: Arc<AtomicBool>,
    mut requests: mpsc::UnboundedReceiver<WriteRequest>,
    cx: &mut AsyncApp,
) {
    let background = cx.background_executor().clone();
    let (root, events, watcher) = background
        .spawn(async move {
            // The root must exist to be watched. It is resolved once, so that a root reached
            // through a symbolic link is watched, loaded and compared with event paths under the
            // same spelling.
            fs.create_dir(&root).await.log_err();
            let root = fs.canonicalize(&root).await.unwrap_or(root);
            let (events, watcher) = fs.watch(&root, WATCH_LATENCY).await;
            (root, events, watcher)
        })
        .await;
    let mut events = events.fuse();
    let mut disk = Disk {
        root,
        watcher,
        lock,
        alive,
        tree: None,
        echoes: Echoes::default(),
        watched: BTreeSet::new(),
    };
    let mut job = Job::Load;
    loop {
        let (returned, done) = background
            .spawn(async move {
                let done = disk.run(job);
                (disk, done)
            })
            .await;
        disk = returned;
        if this.update(cx, |store, cx| store.finish(done, cx)).is_err() {
            return;
        }
        job = select_biased! {
            request = requests.next() => match request {
                Some(request) => Job::Write(request),
                None => return,
            },
            batch = events.next() => match batch {
                Some(batch) => Job::Events(batch),
                None => {
                    log::error!("the work tree watcher stopped; restart bongos to watch again");
                    return;
                }
            },
        };
    }
}

enum Job {
    Load,
    Events(Vec<PathEvent>),
    Write(WriteRequest),
}

enum DoneKind {
    Loaded,
    Reloaded,
    Ignored,
    Wrote { retries: usize },
}

struct Done {
    kind: DoneKind,
    /// The tree after the job, when it read one.
    tree: Option<Arc<WorkTree>>,
    reply: Option<Reply>,
}

/// The answer to a write that ran. The store sends it after publishing the snapshot; if the
/// session ended first (the store was disabled while the write ran), dropping it still sends the
/// real result, so a write that landed never reports `Off`.
struct Reply {
    sender: Option<oneshot::Sender<Result<Change, WriteError>>>,
    result: Option<Result<Change, WriteError>>,
}

impl Reply {
    fn send(mut self) {
        self.deliver();
    }

    fn deliver(&mut self) {
        if let (Some(sender), Some(result)) = (self.sender.take(), self.result.take()) {
            sender.send(result).ok();
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.deliver();
    }
}

/// The session's disk state, moved into each background job and back.
struct Disk {
    root: PathBuf,
    watcher: Arc<dyn Watcher>,
    lock: Arc<Mutex<()>>,
    alive: Arc<AtomicBool>,
    tree: Option<Arc<WorkTree>>,
    echoes: Echoes,
    /// The directories added to the watcher, apart from the root.
    watched: BTreeSet<PathBuf>,
}

impl Disk {
    fn run(&mut self, job: Job) -> Done {
        let lock = self.lock.clone();
        let _guard = lock.lock();
        match job {
            Job::Load => {
                self.reload();
                Done {
                    kind: DoneKind::Loaded,
                    tree: self.tree.clone(),
                    reply: None,
                }
            }
            Job::Events(events) => {
                if self.needs_reload(&events) {
                    // Someone else changed the tree: what the store wrote before is no longer
                    // what is there, so nothing more is an echo of it.
                    self.echoes.clear();
                    self.reload();
                    Done {
                        kind: DoneKind::Reloaded,
                        tree: self.tree.clone(),
                        reply: None,
                    }
                } else {
                    Done {
                        kind: DoneKind::Ignored,
                        tree: None,
                        reply: None,
                    }
                }
            }
            Job::Write(WriteRequest {
                ctx,
                command,
                reply,
            }) => {
                let (result, retries) = self.write(&ctx, &command);
                Done {
                    kind: DoneKind::Wrote { retries },
                    tree: self.tree.clone(),
                    reply: Some(Reply {
                        sender: Some(reply),
                        result: Some(result),
                    }),
                }
            }
        }
    }

    /// Reads the tree again (keeping last good parses) and brings the watcher up to date. A
    /// directory that appeared since the last read may have had files written into it before it
    /// was watched, which produced no event, so the tree is read once more after new directories
    /// are added.
    fn reload(&mut self) {
        self.read_tree();
        if self.refresh_watches() {
            self.read_tree();
            self.refresh_watches();
        }
    }

    fn read_tree(&mut self) {
        let tree = match &self.tree {
            Some(previous) => model_store::reload(&self.root, previous),
            None => model_store::load(&self.root),
        };
        self.tree = Some(Arc::new(tree));
    }

    /// Whether `events` name anything other than temporary files, journals and echoes of the
    /// store's own writes.
    fn needs_reload(&self, events: &[PathEvent]) -> bool {
        events.iter().any(|event| {
            if event.kind == Some(PathEventKind::Rescan) {
                return true;
            }
            if !event.path.starts_with(&self.root) {
                return false;
            }
            let in_git = event
                .path
                .strip_prefix(&self.root)
                .is_ok_and(|rel| rel.components().any(|c| c.as_os_str() == ".git"));
            if in_git {
                return false;
            }
            let ignored_name = event
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| is_temp_file(name) || name == OP_FILE);
            !ignored_name && !self.echoes.matches(&event.path, self.tree.as_deref())
        })
    }

    /// Adds every directory of the tree to the watcher and removes the ones that are gone.
    /// Returns whether any directory was added.
    fn refresh_watches(&mut self) -> bool {
        let wanted = match &self.tree {
            Some(tree) => watch_dirs(&self.root, tree),
            None => BTreeSet::new(),
        };
        for gone in self.watched.difference(&wanted) {
            self.watcher.remove(gone).log_err();
        }
        let mut added = false;
        for new in wanted.difference(&self.watched) {
            if self.watcher.add(new).log_err().is_some() {
                added = true;
            }
        }
        self.watched = wanted;
        added
    }

    /// Runs `command` against the current tree and writes its change if none of the files it
    /// replaces changed since they were read; otherwise reloads and applies it again. Returns the
    /// result and how many times the command was reapplied.
    fn write(
        &mut self,
        ctx: &Context,
        command: &WorkCommand,
    ) -> (Result<Change, WriteError>, usize) {
        if self.tree.is_none() {
            self.reload();
        }
        for attempt in 0..=MAX_RETRIES {
            let Some(tree) = self.tree.clone() else {
                break;
            };
            let change = match command(&tree, ctx) {
                Ok(change) => change,
                Err(refusal) => return (Err(WriteError::Refused(refusal)), attempt),
            };
            match unchanged_since_read(&tree, &change) {
                Ok(true) => {
                    if !self.alive.load(Ordering::SeqCst) {
                        return (Err(WriteError::Off), attempt);
                    }
                    let result = commands::apply(&self.root, &change);
                    // Even a failed write may have replaced files; remember what is there now.
                    self.echoes.record(&self.root, &change);
                    self.reload();
                    let result = result
                        .map(|()| change)
                        .map_err(|error| WriteError::Failed(error.to_string()));
                    return (result, attempt);
                }
                Ok(false) => {
                    log::info!(
                        "work: files of {} changed since they were read; applying {} again",
                        change.dir.display(),
                        change.entry.event
                    );
                    self.reload();
                }
                Err(error) => return (Err(WriteError::Failed(error.to_string())), attempt),
            }
        }
        (Err(WriteError::Contended), MAX_RETRIES)
    }
}

/// Whether every file `change` replaces is still as `tree` read it: same content, or still
/// missing.
fn unchanged_since_read(tree: &WorkTree, change: &Change) -> io::Result<bool> {
    for write in &change.writes {
        let path = before_moves(change, &write.path());
        let (Some(dir), Some(name)) = (
            path.parent(),
            path.file_name().and_then(|name| name.to_str()),
        ) else {
            continue;
        };
        let read = tree.task_by_dir(dir).and_then(|task| task.read_hash(name));
        if disk_hash(&path)? != read {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Where `path`, named by its place after `change`'s moves, is before them.
fn before_moves(change: &Change, path: &Path) -> PathBuf {
    change
        .moves
        .iter()
        .find_map(|moved| {
            path.strip_prefix(&moved.to)
                .ok()
                .map(|rest| moved.from.join(rest))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// The hash of the file at `path`, `None` when it does not exist.
fn disk_hash(path: &Path) -> io::Result<Option<u64>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(content_hash(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The directories to watch besides the root: `projects/`, `oneoffs/`, each activity, and for
/// each task its directory, its `tasks/` directory and every subdirectory it owns (`notes/`,
/// `docs/`, `chats/`, `runs/` and so on, recursively), but not `.git` or temporary directories.
fn watch_dirs(root: &Path, tree: &WorkTree) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    for dir in [layout::projects_dir(root), layout::oneoffs_dir(root)] {
        if dir.is_dir() {
            dirs.insert(dir);
        }
    }
    for activity in &tree.activities {
        dirs.insert(activity.dir.clone());
    }
    for task in tree.tasks() {
        dirs.insert(task.dir.clone());
        let children = layout::children_dir(&task.dir);
        if children.is_dir() {
            dirs.insert(children);
        }
        owned_subdirs(&task.dir, true, &mut dirs);
    }
    dirs
}

fn owned_subdirs(dir: &Path, top: bool, dirs: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_temp_file(&name) || name == ".git" || (top && name == TASKS_DIR) {
            continue;
        }
        // `file_type` does not follow a symbolic link, so a linked directory is not walked.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let path = entry.path();
            owned_subdirs(&path, false, dirs);
            dirs.insert(path);
        }
    }
}

/// What a path holds, as far as the self-echo filter cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathState {
    Absent,
    Dir,
    File(u64),
}

fn path_state(path: &Path) -> Option<PathState> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => Some(PathState::Dir),
        Ok(_) => std::fs::read(path)
            .ok()
            .map(|bytes| PathState::File(content_hash(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(PathState::Absent),
        Err(_) => None,
    }
}

/// What the store's own writes left behind, for the self-echo filter.
///
/// A model file (`task.toml`, `project.toml`, `artifacts.toml`) the store wrote is an echo while
/// its content is still what the current tree read: the tree is reloaded after every write, so
/// that is the store's own content, and any other content (an edit after the write, or a later
/// edit back) is not. Other paths (`history.old.toml`, directories created or moved) are an echo
/// while they are still in the state the write left them in. Entries last [`ECHO_TTL`] and are
/// cleared whenever watcher events reload the tree.
#[derive(Default)]
struct Echoes {
    entries: HashMap<PathBuf, (Echo, Instant)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Echo {
    /// Compared with the hash the current tree read.
    ModelFile,
    Other(PathState),
}

fn is_model_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name == layout::TASK_FILE || name == layout::PROJECT_FILE || name == layout::ARTIFACTS_FILE
    })
}

impl Echoes {
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Records every path `change` touched: the files it replaced (and `history.old.toml` and
    /// `prompts.txt` beside each), the directories it created or moved, and their parents up to
    /// the root.
    fn record(&mut self, root: &Path, change: &Change) {
        let now = Instant::now();
        self.entries
            .retain(|_, (_, at)| now.duration_since(*at) < ECHO_TTL);
        let mut paths = BTreeSet::new();
        let with_parents = |path: &Path, paths: &mut BTreeSet<PathBuf>| {
            for ancestor in path.ancestors() {
                if ancestor == root || !ancestor.starts_with(root) {
                    break;
                }
                paths.insert(ancestor.to_path_buf());
            }
        };
        for write in &change.writes {
            let path = write.path();
            if let Some(dir) = path.parent() {
                paths.insert(layout::history_old_file(dir));
                // `apply` creates it beside the `task.toml` of a new task.
                paths.insert(layout::prompts_file(dir));
            }
            with_parents(&path, &mut paths);
        }
        for moved in &change.moves {
            with_parents(&moved.from, &mut paths);
            with_parents(&moved.to, &mut paths);
        }
        for path in paths {
            let echo = if is_model_file(&path) {
                Some(Echo::ModelFile)
            } else {
                path_state(&path).map(Echo::Other)
            };
            if let Some(echo) = echo {
                self.entries.insert(path, (echo, now));
            }
        }
    }

    fn matches(&self, path: &Path, tree: Option<&WorkTree>) -> bool {
        let Some((echo, at)) = self.entries.get(path) else {
            return false;
        };
        if at.elapsed() >= ECHO_TTL {
            return false;
        }
        match echo {
            Echo::ModelFile => {
                let (Some(tree), Some(dir), Some(name)) = (
                    tree,
                    path.parent(),
                    path.file_name().and_then(|name| name.to_str()),
                ) else {
                    return false;
                };
                let read = tree.task_by_dir(dir).and_then(|task| task.read_hash(name));
                disk_hash(path).is_ok_and(|on_disk| on_disk.is_some() && on_disk == read)
            }
            Echo::Other(state) => path_state(path).as_ref() == Some(state),
        }
    }
}

#[cfg(test)]
pub(crate) fn reply_for_test(
    sender: oneshot::Sender<Result<Change, WriteError>>,
    result: Result<Change, WriteError>,
) -> impl Drop {
    Reply {
        sender: Some(sender),
        result: Some(result),
    }
}
