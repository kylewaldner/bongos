//! [`ChatStore`]: the index of chat records, built from the work tree's snapshot, and the serial
//! writer of those records.
//!
//! Indexing: the store runs no watcher of its own. Every `WorkStoreEvent::Changed` it walks the
//! files of each task for `chats/<name>.toml` and reparses, on the background executor, only the
//! files whose path or mtime differs from the cached entry. An edit by hand or by an agent, and a
//! task moved with its files, thus reach the index through the work store's watcher.
//!
//! Writing: every write runs on one queue, one at a time. An update reads the record fresh from
//! disk, applies the change and replaces the file atomically, so an edit made before the read is
//! kept. A record that is gone (deleted by hand) is never recreated: the update is refused. Only
//! [`ChatStore::create`] makes a new file.
//!
//! While `chats.enabled` is false nothing is indexed, nothing is seeded and writes answer
//! [`ChatWriteError::Off`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bongos_chat_model::{
    ChatEntry, ChatRecord, ErrorKind, create_record, is_record_file_name, read_record, seed,
    update_record,
};
use bongos_work::{WorkStore, WorkStoreEvent};
use bongos_work_model::layout::CHATS_DIR;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, Entity, EventEmitter, Subscription, Task};
use settings::{Settings as _, SettingsStore};

use crate::{ChatDirs, ChatsSettings};

/// One record file in the index.
#[derive(Clone, Debug)]
pub struct IndexedChat {
    /// The task or project directory whose `chats/` holds the file.
    pub scope_dir: PathBuf,
    /// The id of that task, when its `task.toml` parsed.
    pub task_id: Option<String>,
    /// The record, or why it could not be read.
    pub entry: Arc<ChatEntry>,
    mtime: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatStoreEvent {
    /// The index changed (or went away).
    Changed,
}

/// Why a write did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatWriteError {
    /// `chats.enabled` is false (or went false before the write ran).
    Off,
    /// The record file does not exist; it is not recreated.
    Missing(PathBuf),
    /// Reading, changing or writing the record failed.
    Failed(String),
}

impl fmt::Display for ChatWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatWriteError::Off => write!(f, "Chats are off (chats.enabled)"),
            ChatWriteError::Missing(path) => {
                write!(f, "The chat record {} no longer exists", path.display())
            }
            ChatWriteError::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for ChatWriteError {}

/// Counters of what the store did, for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatStats {
    /// Record files parsed by indexing.
    pub parses: usize,
    /// Times the session types were seeded.
    pub seeds: usize,
    /// Writes done.
    pub writes: usize,
}

type Job = Box<dyn FnOnce() + Send>;

/// The global chat index and writer. Reads and writes nothing while `chats.enabled` is false.
pub struct ChatStore {
    dirs: ChatDirs,
    work: Entity<WorkStore>,
    enabled: bool,
    index: BTreeMap<PathBuf, IndexedChat>,
    /// The writer queue of the current enabled period.
    writer: Option<Writer>,
    /// Kept across enabled periods, so a job of an old writer still running never overlaps the
    /// new writer's.
    disk_lock: Arc<Mutex<()>>,
    indexing: Option<Task<()>>,
    seeding: Option<Task<()>>,
    stats: ChatStats,
    _subscriptions: Vec<Subscription>,
}

struct Writer {
    jobs: mpsc::UnboundedSender<Job>,
    _run: Task<()>,
}

impl EventEmitter<ChatStoreEvent> for ChatStore {}

impl ChatStore {
    pub fn new(work: Entity<WorkStore>, dirs: ChatDirs, cx: &mut gpui::Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_global::<SettingsStore>(|this, cx| this.sync_enabled(cx)),
            cx.subscribe(&work, |this, _, event, cx| {
                if *event == WorkStoreEvent::Changed {
                    this.reindex(cx);
                }
            }),
        ];
        let mut this = Self {
            dirs,
            work,
            enabled: false,
            index: BTreeMap::new(),
            writer: None,
            disk_lock: Arc::new(Mutex::new(())),
            indexing: None,
            seeding: None,
            stats: ChatStats::default(),
            _subscriptions: subscriptions,
        };
        this.sync_enabled(cx);
        this
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The Work store this index is built from.
    pub fn work(&self) -> &Entity<WorkStore> {
        &self.work
    }

    pub fn dirs(&self) -> &ChatDirs {
        &self.dirs
    }

    pub fn stats(&self) -> ChatStats {
        self.stats
    }

    /// Every indexed record file, by path.
    pub fn index(&self) -> &BTreeMap<PathBuf, IndexedChat> {
        &self.index
    }

    /// The indexed file of the chat with `id` (for an invalid file, its name without `.toml`).
    pub fn find(&self, id: &str) -> Option<&IndexedChat> {
        self.index.values().find(|chat| chat.entry.id() == id)
    }

    /// The session types directory: `chats.types_dir` when set, else the default of
    /// [`ChatDirs`].
    pub fn types_root(&self, cx: &App) -> PathBuf {
        match &ChatsSettings::get_global(cx).types_dir {
            Some(dir) => expand_home(dir),
            None => self.dirs.types_root.clone(),
        }
    }

    /// Queues an update of the record at `path`: the record is read fresh, `change` applied and
    /// the file replaced. Refused with [`ChatWriteError::Missing`] when the file is gone.
    pub fn update(
        &mut self,
        path: PathBuf,
        change: impl FnOnce(&mut ChatRecord) -> bongos_chat_model::Result<()> + Send + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<ChatRecord, ChatWriteError>> {
        self.queue(
            move || {
                update_now(&path, |record| {
                    let id = record.id.clone();
                    change(record)?;
                    if record.id != id {
                        // The id names the file; a record that changed it would be written
                        // under the old name with a new id.
                        return Err(model_error(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "a chat's id cannot be changed",
                        )));
                    }
                    Ok(())
                })
            },
            cx,
        )
    }

    /// Queues the creation of `record` in `<scope_dir>/chats/`. Refused when the file exists.
    pub fn create(
        &mut self,
        scope_dir: PathBuf,
        record: ChatRecord,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<PathBuf, ChatWriteError>> {
        self.queue(
            move || {
                if !is_valid_id(&record.id) {
                    return Err(ChatWriteError::Failed(format!(
                        "`{}` is not a usable chat id",
                        record.id
                    )));
                }
                create_record(&scope_dir, &record)
                    .map_err(|error| ChatWriteError::Failed(error.to_string()))
            },
            cx,
        )
    }

    fn queue<T: Send + 'static>(
        &mut self,
        job: impl FnOnce() -> Result<T, ChatWriteError> + Send + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<T, ChatWriteError>> {
        let Some(writer) = &self.writer else {
            return Task::ready(Err(ChatWriteError::Off));
        };
        let (reply, response) = oneshot::channel();
        let lock = self.disk_lock.clone();
        let job: Job = Box::new(move || {
            let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            reply.send(job()).ok();
        });
        if writer.jobs.unbounded_send(job).is_err() {
            return Task::ready(Err(ChatWriteError::Off));
        }
        cx.spawn(async move |this, cx| {
            let result = response.await.unwrap_or(Err(ChatWriteError::Off));
            if result.is_ok() {
                this.update(cx, |this, _| this.stats.writes += 1).ok();
            }
            result
        })
    }

    fn sync_enabled(&mut self, cx: &mut gpui::Context<Self>) {
        let enabled = ChatsSettings::get_global(cx).enabled;
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            let (jobs, mut queued) = mpsc::unbounded::<Job>();
            let run = cx.background_spawn(async move {
                while let Some(job) = queued.next().await {
                    job();
                }
            });
            self.writer = Some(Writer { jobs, _run: run });
            self.seed(cx);
            self.reindex(cx);
        } else {
            // Dropping the writer drops the queued jobs, whose callers get `Off`.
            self.writer = None;
            self.indexing = None;
            self.seeding = None;
            if !self.index.is_empty() {
                self.index.clear();
                cx.emit(ChatStoreEvent::Changed);
            }
            cx.notify();
        }
    }

    /// Creates the shipped session types that are missing. Never replaces anything.
    fn seed(&mut self, cx: &mut gpui::Context<Self>) {
        let types_root = self.types_root(cx);
        self.seeding = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let types_root = types_root.clone();
                    async move { seed(&types_root) }
                })
                .await;
            match result {
                Ok(written) if !written.is_empty() => log::info!(
                    "chats: seeded session types {} under {}",
                    written.join(", "),
                    types_root.display()
                ),
                Ok(_) => {}
                Err(error) => log::error!("chats: seeding the session types failed: {error}"),
            }
            this.update(cx, |this, _| this.stats.seeds += 1).ok();
        }));
    }

    /// Rebuilds the index from the work tree's snapshot, reparsing only changed files.
    fn reindex(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.enabled {
            return;
        }
        let Some(tree) = self.work.read(cx).snapshot() else {
            self.indexing = None;
            if !self.index.is_empty() {
                self.index.clear();
                cx.emit(ChatStoreEvent::Changed);
                cx.notify();
            }
            return;
        };
        let mut found = Vec::new();
        for task in tree.tasks() {
            for (relative, mtime) in &task.files {
                if is_record(relative) {
                    found.push(Found {
                        path: task.dir.join(relative),
                        scope_dir: task.dir.clone(),
                        task_id: task.id().map(str::to_string),
                        mtime: *mtime,
                    });
                }
            }
        }
        let previous = self.index.clone();
        self.indexing = Some(cx.spawn(async move |this, cx| {
            let (index, parses) = cx
                .background_spawn(async move { build_index(found, &previous) })
                .await;
            this.update(cx, |this, cx| {
                let paths_changed = !this.index.keys().eq(index.keys());
                this.stats.parses += parses;
                this.index = index;
                if parses > 0 || paths_changed {
                    cx.emit(ChatStoreEvent::Changed);
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

struct Found {
    path: PathBuf,
    scope_dir: PathBuf,
    task_id: Option<String>,
    mtime: SystemTime,
}

/// Whether `relative`, a file a task owns, is `chats/<name>.toml`.
fn is_record(relative: &Path) -> bool {
    let mut components = relative.components();
    match (components.next(), components.next(), components.next()) {
        (Some(dir), Some(name), None) => {
            dir.as_os_str() == CHATS_DIR
                && name.as_os_str().to_str().is_some_and(is_record_file_name)
        }
        _ => false,
    }
}

/// The index of `found`, reusing an entry of `previous` whose path and mtime are unchanged.
/// Returns it with the number of files parsed.
fn build_index(
    found: Vec<Found>,
    previous: &BTreeMap<PathBuf, IndexedChat>,
) -> (BTreeMap<PathBuf, IndexedChat>, usize) {
    let mut parses = 0;
    let index = found
        .into_iter()
        .map(|found| {
            let entry = match previous.get(&found.path) {
                Some(cached) if cached.mtime == found.mtime => cached.entry.clone(),
                _ => {
                    parses += 1;
                    Arc::new(ChatEntry {
                        chat: read_record(&found.path),
                        path: found.path.clone(),
                    })
                }
            };
            let chat = IndexedChat {
                scope_dir: found.scope_dir,
                task_id: found.task_id,
                entry,
                mtime: found.mtime,
            };
            (found.path, chat)
        })
        .collect();
    (index, parses)
}

/// Whether `id` is one path component that makes a record file name: it is never a path that
/// leaves the `chats` directory.
fn is_valid_id(id: &str) -> bool {
    let one_component =
        !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']);
    one_component && is_record_file_name(&format!("{id}.toml"))
}

fn model_error(error: std::io::Error) -> bongos_chat_model::Error {
    bongos_chat_model::Error {
        path: None,
        line: None,
        column: None,
        kind: ErrorKind::Io(error),
    }
}

/// The update of a record, run on the writer queue.
fn update_now(
    path: &Path,
    change: impl FnOnce(&mut ChatRecord) -> bongos_chat_model::Result<()>,
) -> Result<ChatRecord, ChatWriteError> {
    if !path.is_file() {
        return Err(ChatWriteError::Missing(path.to_path_buf()));
    }
    update_record(path, change).map_err(|error| match &error.kind {
        ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            ChatWriteError::Missing(path.to_path_buf())
        }
        _ => ChatWriteError::Failed(error.to_string()),
    })
}

/// `~` or `~/...` under the home directory; any other path as written.
pub(crate) fn expand_home(dir: &str) -> PathBuf {
    if dir == "~" {
        return util::paths::home_dir().clone();
    }
    match dir.strip_prefix("~/") {
        Some(rest) => util::paths::home_dir().join(rest),
        None => PathBuf::from(dir),
    }
}
