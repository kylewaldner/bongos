//! The live state store of a project: `state/state.db`, one SQLite database in WAL mode with one
//! writer per project, taken with `flock` on `state/host.lock`, and any number of readers.
//!
//! Tables: `tasks(id, state_toml)` holds a task's state record without its todos and history,
//! `todos(task_id, position, todo_id, state, started)` the state of its todos, `history(task_id,
//! seq, entry_toml)` its history, `kv(namespace, key, value_toml)` the state of other features,
//! `staged_state(change_id, ops)` the state ops a pending change applies when it lands, and
//! `landings`, `changes`, `check_results`, `approvals`, `landing_queue`, `resolutions` and
//! `meta(key, value)` for the landing machinery and the store itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::{
    Author, DefinitionTree, StateLookup, TaskStateRecord, TodoStateRecord, layout,
};
use bongos_work_model::{HistoryEntry, TaskId, Timestamp, TodoState};
use futures::channel::mpsc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlez::connection::Connection;

use crate::Location;
use crate::changes::{self, ChangeRow};
use crate::state_hooks::{StateApplyChange, StateApplyHook, StateApplyHooks, TaskTouch};
use crate::state_ops::{self, StateOp};

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS tasks (
        id TEXT PRIMARY KEY NOT NULL,
        state_toml TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS todos (
        task_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        todo_id TEXT,
        state TEXT NOT NULL,
        started TEXT,
        PRIMARY KEY (task_id, position)
    )",
    "CREATE TABLE IF NOT EXISTS history (
        task_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        entry_toml TEXT NOT NULL,
        PRIMARY KEY (task_id, seq)
    )",
    "CREATE TABLE IF NOT EXISTS kv (
        namespace TEXT NOT NULL,
        key TEXT NOT NULL,
        value_toml TEXT NOT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS kv_namespace_key ON kv (namespace, key)",
    "CREATE TABLE IF NOT EXISTS landings (
        change_id TEXT PRIMARY KEY NOT NULL,
        record_toml TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS changes (
        change_id TEXT PRIMARY KEY NOT NULL,
        branch TEXT NOT NULL,
        author TEXT NOT NULL,
        created TEXT NOT NULL,
        meta_toml TEXT
    )",
    "CREATE TABLE IF NOT EXISTS staged_state (
        change_id TEXT PRIMARY KEY NOT NULL,
        ops TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS check_results (
        change_id TEXT NOT NULL,
        check_name TEXT NOT NULL,
        result_toml TEXT NOT NULL,
        PRIMARY KEY (change_id, check_name)
    )",
    "CREATE TABLE IF NOT EXISTS approvals (
        change_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        record_toml TEXT NOT NULL,
        PRIMARY KEY (change_id, seq)
    )",
    "CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS landing_queue (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        change_id TEXT NOT NULL UNIQUE
    )",
    "CREATE TABLE IF NOT EXISTS resolutions (
        commit_id TEXT PRIMARY KEY NOT NULL,
        record_toml TEXT NOT NULL
    )",
];

const GENERATION_KEY: &str = "generation";
/// The `meta` key of the last commit that passed the deterministic checks.
pub const VERIFIED_COMMIT_KEY: &str = "verified_commit";

/// How long a second process waits for the writer's pid to appear in `host.lock`, which the
/// writer empties and then fills right after it takes the lock.
const PID_READ_ATTEMPTS: usize = 20;
const PID_READ_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

/// Sent after every committed transaction that wrote something.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum StateEvent {
    Changed {
        generation: u64,
        task_ids: Vec<TaskId>,
        namespaces: Vec<String>,
    },
}

/// Why the writer could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Another process holds the writer; open a reader instead. `pid` is `None` when the
    /// holder's pid could not be read.
    #[error("this project is open in another bongos process ({})", held_by(.pid))]
    HeldBy { pid: Option<u32> },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

fn held_by(pid: &Option<u32>) -> String {
    match pid {
        Some(pid) => format!("pid {pid}"),
        None => "pid unknown".to_string(),
    }
}

/// Opens a project's state store.
pub struct StateStore;

impl StateStore {
    /// Takes the project's single writer, with the state apply hooks registered globally.
    pub fn open_writer(location: &Location) -> Result<StateWriter, OpenError> {
        Self::open_writer_with_hooks(location, StateApplyHooks::global())
    }

    /// Takes the project's single writer, running `hooks`. Seals `hooks`.
    pub fn open_writer_with_hooks(
        location: &Location,
        hooks: &StateApplyHooks,
    ) -> Result<StateWriter, OpenError> {
        let hooks = hooks.seal();
        let lock = take_host_lock(&location.state.join(layout::HOST_LOCK_FILE))?;
        let connection = open_connection(&location.state.join(layout::STATE_DB_FILE))?;
        exec(&connection, "PRAGMA journal_mode=WAL")?;
        for statement in SCHEMA {
            exec(&connection, statement).context("creating the state store tables")?;
        }
        Ok(StateWriter {
            connection: Mutex::new(connection),
            definitions: Mutex::new((None, Arc::new(DefinitionTree::default()))),
            hooks,
            subscribers: Mutex::new(Vec::new()),
            _lock: lock,
        })
    }

    /// Opens the store for reading only. It never takes the host lock, so it works beside the
    /// writer of another process.
    pub fn open_reader(location: &Location) -> Result<StateReader> {
        let path = location.state.join(layout::STATE_DB_FILE);
        if !path.exists() {
            bail!("no state store at {}", path.display());
        }
        let connection = open_connection(&path)?;
        exec(&connection, "PRAGMA query_only=ON")?;
        Ok(StateReader {
            connection: Mutex::new(connection),
        })
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let uri = path
        .to_str()
        .with_context(|| format!("the state store path {} is not UTF-8", path.display()))?;
    let connection = Connection::open_file(uri);
    if !connection.persistent() {
        bail!("could not open the state store at {}", path.display());
    }
    exec(&connection, "PRAGMA busy_timeout=500")?;
    exec(&connection, "PRAGMA synchronous=NORMAL")?;
    Ok(connection)
}

#[cfg(unix)]
fn take_host_lock(path: &Path) -> Result<File, OpenError> {
    use std::os::fd::AsRawFd as _;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: the descriptor is open for the duration of the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(OpenError::HeldBy {
                pid: read_holder_pid(&mut file),
            });
        }
        return Err(anyhow!(error)
            .context(format!("locking {}", path.display()))
            .into());
    }
    write_pid(&mut file).with_context(|| format!("writing the pid into {}", path.display()))?;
    Ok(file)
}

#[cfg(not(unix))]
fn take_host_lock(path: &Path) -> Result<File, OpenError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    write_pid(&mut file).with_context(|| format!("writing the pid into {}", path.display()))?;
    Ok(file)
}

/// The pid in a held `host.lock`. The holder empties the file and then writes its pid, so an
/// empty or zero read is retried briefly; `None` if no pid appears.
pub(crate) fn read_holder_pid(file: &mut File) -> Option<u32> {
    for attempt in 0..PID_READ_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(PID_READ_PAUSE);
        }
        let mut text = String::new();
        if file.seek(SeekFrom::Start(0)).is_err() || file.read_to_string(&mut text).is_err() {
            continue;
        }
        if let Ok(pid) = text.trim().parse::<u32>()
            && pid != 0
        {
            return Some(pid);
        }
    }
    None
}

fn write_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

fn exec(connection: &Connection, sql: &str) -> Result<()> {
    connection.exec(sql)?()
}

/// The single writer of a project's state. Dropping it releases the host lock.
pub struct StateWriter {
    // Declared before the lock so the database closes before the lock is released.
    connection: Mutex<Connection>,
    /// The definitions at the verified commit, which the hooks see, with that commit when known.
    definitions: Mutex<(Option<String>, Arc<DefinitionTree>)>,
    hooks: Arc<[Arc<dyn StateApplyHook>]>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<StateEvent>>>,
    _lock: File,
}

impl StateWriter {
    /// Runs `f` in one transaction, then the state apply hooks, then commits. An error from `f`
    /// or from a hook changes nothing.
    pub fn transact<T>(&self, f: impl FnOnce(&mut StateTx) -> Result<T>) -> Result<T> {
        self.transact_with(&Author::unattributed("state store"), &[], f)
    }

    /// [`Self::transact`] with the author the hooks see.
    pub fn transact_as<T>(
        &self,
        origin: &Author,
        f: impl FnOnce(&mut StateTx) -> Result<T>,
    ) -> Result<T> {
        self.transact_with(origin, &[], f)
    }

    pub(crate) fn transact_with<T>(
        &self,
        origin: &Author,
        ops: &[StateOp],
        f: impl FnOnce(&mut StateTx) -> Result<T>,
    ) -> Result<T> {
        let connection = self.connection.lock();
        exec(&connection, "BEGIN IMMEDIATE").context("starting a state transaction")?;
        // Rolls back if `f` or a hook panics, so the connection is never left inside a
        // transaction.
        let mut guard = RollbackGuard {
            connection: &connection,
            open: true,
        };
        let definitions = self.definitions.lock().1.clone();
        let outcome = run_transaction(&connection, &self.hooks, &definitions, origin, ops, f);
        let (value, event, new_definitions) = match outcome {
            Ok(done) => {
                guard.open = false;
                if let Err(error) = exec(&connection, "COMMIT") {
                    exec(&connection, "ROLLBACK").ok();
                    return Err(error.context("committing a state transaction"));
                }
                done
            }
            Err(error) => return Err(error),
        };
        drop(guard);
        if let Some((commit, definitions)) = new_definitions {
            *self.definitions.lock() = (Some(commit), definitions);
        }
        // Sent while the connection is still held, so events reach subscribers in the order
        // their transactions committed.
        if let Some(event) = event {
            self.subscribers
                .lock()
                .retain(|subscriber| subscriber.unbounded_send(event.clone()).is_ok());
        }
        drop(connection);
        Ok(value)
    }

    /// The definitions the hooks see as the verified commit's.
    pub fn definitions(&self) -> Arc<DefinitionTree> {
        self.definitions.lock().1.clone()
    }

    /// The commit whose definitions the hooks see, when known.
    pub fn definitions_commit(&self) -> Option<String> {
        self.definitions.lock().0.clone()
    }

    /// Sets the definitions the hooks see without a transaction: when a project opens, before
    /// any write.
    pub fn set_definitions(&self, definitions: Arc<DefinitionTree>) {
        *self.definitions.lock() = (None, definitions);
    }

    /// [`Self::set_definitions`] for the definitions of `commit`.
    pub fn set_definitions_at(&self, commit: &str, definitions: Arc<DefinitionTree>) {
        *self.definitions.lock() = (Some(commit.to_string()), definitions);
    }

    /// A consistent read on the writer's connection. Not callable from inside `transact`.
    pub fn read<T>(&self, f: impl FnOnce(&StateRead) -> Result<T>) -> Result<T> {
        read_on(&self.connection.lock(), f)
    }

    /// The events of every transaction committed from now on.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<StateEvent> {
        let (sender, receiver) = mpsc::unbounded();
        self.subscribers.lock().push(sender);
        receiver
    }

    pub fn generation(&self) -> Result<u64> {
        self.read(|read| read.generation())
    }
}

/// Rolls back the open transaction when dropped while `open`.
struct RollbackGuard<'a> {
    connection: &'a Connection,
    open: bool,
}

impl Drop for RollbackGuard<'_> {
    fn drop(&mut self) {
        if self.open {
            exec(self.connection, "ROLLBACK").ok();
        }
    }
}

type TransactionDone<T> = (T, Option<StateEvent>, Option<(String, Arc<DefinitionTree>)>);

fn run_transaction<T>(
    connection: &Connection,
    hooks: &[Arc<dyn StateApplyHook>],
    definitions: &Arc<DefinitionTree>,
    origin: &Author,
    ops: &[StateOp],
    f: impl FnOnce(&mut StateTx) -> Result<T>,
) -> Result<TransactionDone<T>> {
    let mut tx = StateTx::new(connection);
    let value = f(&mut tx)?;
    run_hooks(&mut tx, hooks, definitions, origin, ops)?;
    let new_definitions = tx.verified.take();
    if !tx.dirty {
        return Ok((value, None, new_definitions));
    }
    let generation = read_generation(connection)? + 1;
    connection.exec_bound::<(&str, String)>(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?((GENERATION_KEY, generation.to_string()))?;
    let event = StateEvent::Changed {
        generation,
        task_ids: tx.task_ids.into_iter().collect(),
        namespaces: tx.namespaces.into_iter().collect(),
    };
    Ok((value, Some(event), new_definitions))
}

fn run_hooks(
    tx: &mut StateTx,
    hooks: &[Arc<dyn StateApplyHook>],
    definitions: &Arc<DefinitionTree>,
    origin: &Author,
    ops: &[StateOp],
) -> Result<()> {
    let new_tree = tx.verified.as_ref().map(|(_, tree)| tree.clone());
    if (tx.before.is_empty() && new_tree.is_none()) || hooks.is_empty() {
        return Ok(());
    }
    // When the verified commit changes, the hooks see the old and the new definitions, and
    // every task whose definition differs counts as touched.
    let (old_definitions, current) = match &new_tree {
        Some(new_tree) => (Some(definitions.clone()), new_tree.clone()),
        None => (None, definitions.clone()),
    };
    let mut ids: BTreeSet<TaskId> = tx.before.keys().cloned().collect();
    if let Some(old) = &old_definitions {
        ids.extend(old.differing_ids(&current));
    }
    let mut touched = Vec::with_capacity(ids.len());
    for id in ids {
        let after = tx.task(&id)?;
        let before = match tx.before.get(&id) {
            Some(before) => before.clone(),
            None => after.clone(),
        };
        let definition_before = old_definitions.as_deref().unwrap_or(&current).task(&id);
        let definition_after = current.task(&id);
        touched.push(TaskTouch {
            id,
            before,
            after,
            definition_before,
            definition_after,
        });
    }
    let namespaces: Vec<String> = tx.namespaces.iter().cloned().collect();
    let change = StateApplyChange {
        ops,
        touched: &touched,
        namespaces: &namespaces,
        old_definitions: old_definitions.as_deref(),
        definitions: &current,
        origin,
    };
    for hook in hooks {
        if !hook.wants(&change) {
            continue;
        }
        tx.hook = Some(hook.name().to_string());
        let result = hook.apply(tx, &change);
        tx.hook = None;
        result.map_err(|error| anyhow!("state hook {} failed: {error:#}", hook.name()))?;
    }
    Ok(())
}

/// A reader of a project's state, beside the writer of this or another process.
pub struct StateReader {
    connection: Mutex<Connection>,
}

impl StateReader {
    /// A consistent read: one SQLite read transaction.
    pub fn read<T>(&self, f: impl FnOnce(&StateRead) -> Result<T>) -> Result<T> {
        read_on(&self.connection.lock(), f)
    }
}

#[cfg(test)]
impl StateReader {
    /// Runs `sql` on the reader's connection, to show that it cannot write.
    pub(crate) fn raw_exec(&self, sql: &str) -> Result<()> {
        exec(&self.connection.lock(), sql)
    }
}

fn read_on<T>(connection: &Connection, f: impl FnOnce(&StateRead) -> Result<T>) -> Result<T> {
    exec(connection, "BEGIN").context("starting a state read")?;
    let result = f(&StateRead { connection });
    let end = exec(
        connection,
        if result.is_ok() { "COMMIT" } else { "ROLLBACK" },
    );
    let value = result?;
    end.context("ending a state read")?;
    Ok(value)
}

/// One consistent view of the state.
pub struct StateRead<'a> {
    connection: &'a Connection,
}

impl<'a> StateRead<'a> {
    /// The task's state, or `None` when it has no row (it reads as
    /// [`TaskStateRecord::default`]).
    pub fn task(&self, id: &str) -> Result<Option<TaskStateRecord>> {
        read_task(self.connection, id)
    }

    /// Every task id with a state row, in order.
    pub fn task_ids(&self) -> Result<Vec<TaskId>> {
        read_task_ids(self.connection)
    }

    /// State rows whose task is not among `known`, the tasks of the verified commit. They are
    /// kept, and reported.
    pub fn orphans(&self, known: &BTreeSet<TaskId>) -> Result<Vec<TaskId>> {
        Ok(self
            .task_ids()?
            .into_iter()
            .filter(|id| !known.contains(id))
            .collect())
    }

    /// The change with `change_id`.
    pub fn change(&self, change_id: &str) -> Result<Option<ChangeRow>> {
        changes::read_change(self.connection, change_id)
    }

    /// Every change, oldest first, whatever its state.
    pub fn changes(&self) -> Result<Vec<ChangeRow>> {
        changes::read_changes(self.connection)
    }

    pub fn kv(&self, namespace: &str) -> KvRead<'_> {
        KvRead {
            connection: self.connection,
            namespace: namespace.to_string(),
        }
    }

    pub fn staged(&self, change_id: &str) -> Result<Option<Vec<StateOp>>> {
        read_staged(self.connection, change_id)
    }

    pub fn generation(&self) -> Result<u64> {
        read_generation(self.connection)
    }

    /// The last commit that passed the deterministic checks; `None` before one is recorded.
    pub fn verified_commit(&self) -> Result<Option<String>> {
        read_meta(self.connection, VERIFIED_COMMIT_KEY)
    }

    /// The value of the `meta` key `key`.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        read_meta(self.connection, key)
    }

    /// Every landing as `(change id, record)`, newest first.
    pub(crate) fn landings(&self) -> Result<Vec<(String, String)>> {
        self.connection.select::<(String, String)>(
            "SELECT change_id, record_toml FROM landings ORDER BY rowid DESC",
        )?()
    }

    /// Every resolution of a split master as `(commit, record)`, newest first.
    pub(crate) fn resolutions(&self) -> Result<Vec<(String, String)>> {
        self.connection.select::<(String, String)>(
            "SELECT commit_id, record_toml FROM resolutions ORDER BY rowid DESC",
        )?()
    }

    /// The change ids in the landing queue, first in first.
    pub fn landing_queue(&self) -> Result<Vec<String>> {
        read_queue(self.connection)
    }

    /// The stored results of the checks of `change_id` as `(check, record)`.
    pub(crate) fn check_results(&self, change_id: &str) -> Result<Vec<(String, String)>> {
        self.connection.select_bound::<&str, (String, String)>(
            "SELECT check_name, result_toml FROM check_results WHERE change_id = ? ORDER BY check_name",
        )?(change_id)
    }
}

impl StateLookup for StateRead<'_> {
    fn task_state(&self, id: &str) -> std::result::Result<Option<TaskStateRecord>, String> {
        self.task(id).map_err(|error| format!("{error:#}"))
    }
}

/// One `kv` namespace in a read.
pub struct KvRead<'a> {
    connection: &'a Connection,
    namespace: String,
}

impl KvRead<'_> {
    pub fn get(&self, key: &str) -> Result<Option<toml::Value>> {
        kv_get(self.connection, &self.namespace, key)
    }

    /// The entries whose key starts with `prefix`, ordered by key.
    pub fn scan(&self, prefix: &str) -> Result<Vec<(String, toml::Value)>> {
        kv_scan(self.connection, &self.namespace, prefix)
    }
}

/// The writes of one transaction. Before a task's first write to its `tasks` or `todos` rows it
/// keeps the task's whole record, so the state apply hooks see it before and after.
pub struct StateTx<'a> {
    connection: &'a Connection,
    before: BTreeMap<TaskId, Option<TaskStateRecord>>,
    task_ids: BTreeSet<TaskId>,
    namespaces: BTreeSet<String>,
    dirty: bool,
    /// The hook running now; it may write only `kv`.
    hook: Option<String>,
    /// The verified commit this transaction records, with its parsed definitions.
    verified: Option<(String, Arc<DefinitionTree>)>,
}

impl<'a> StateTx<'a> {
    fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            before: BTreeMap::new(),
            task_ids: BTreeSet::new(),
            namespaces: BTreeSet::new(),
            dirty: false,
            hook: None,
            verified: None,
        }
    }

    pub fn task(&self, id: &str) -> Result<Option<TaskStateRecord>> {
        read_task(self.connection, id)
    }

    pub fn task_ids(&self) -> Result<Vec<TaskId>> {
        read_task_ids(self.connection)
    }

    pub fn generation(&self) -> Result<u64> {
        read_generation(self.connection)
    }

    pub fn verified_commit(&self) -> Result<Option<String>> {
        read_meta(self.connection, VERIFIED_COMMIT_KEY)
    }

    /// Records `commit` as the verified commit, whose parsed definitions are `definitions`. The
    /// hooks of this transaction see the old and the new definitions, and once it commits the
    /// writer's definitions are the new ones.
    pub fn set_verified_commit(
        &mut self,
        commit: &str,
        definitions: Arc<DefinitionTree>,
    ) -> Result<()> {
        self.only_kv_in_hook("the verified commit")?;
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?((VERIFIED_COMMIT_KEY, commit))?;
        self.verified = Some((commit.to_string(), definitions));
        self.dirty = true;
        Ok(())
    }

    fn only_kv_in_hook(&self, what: &str) -> Result<()> {
        match &self.hook {
            Some(hook) => {
                bail!("a state hook writes only kv namespaces, and {hook} wrote {what}")
            }
            None => Ok(()),
        }
    }

    /// Before a write to a task's `tasks` or `todos` rows.
    fn touch_task(&mut self, id: &str) -> Result<()> {
        self.only_kv_in_hook(&format!("the state of task {id}"))?;
        if !self.before.contains_key(id) {
            let before = read_task(self.connection, id)?;
            self.before.insert(id.to_string(), before);
        }
        self.task_ids.insert(id.to_string());
        self.dirty = true;
        Ok(())
    }

    /// Replaces the task's whole state: its row, its todos and its history.
    pub fn set_task(&mut self, id: &str, mut record: TaskStateRecord) -> Result<()> {
        self.touch_task(id)?;
        let todos = std::mem::take(&mut record.todos);
        let history = std::mem::take(&mut record.history);
        let state_toml = toml::to_string(&record).context("serializing a task state")?;
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO tasks (id, state_toml) VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET state_toml = excluded.state_toml",
        )?((id, &state_toml))?;
        self.connection
            .exec_bound::<&str>("DELETE FROM todos WHERE task_id = ?")?(id)?;
        for (position, todo) in todos.iter().enumerate() {
            insert_todo(self.connection, id, position, todo)?;
        }
        self.connection
            .exec_bound::<&str>("DELETE FROM history WHERE task_id = ?")?(id)?;
        for entry in history {
            insert_history(self.connection, id, &entry)?;
        }
        Ok(())
    }

    /// Writes one todo's state, matched by its id, touching only the `todos` table.
    pub fn set_todo(&mut self, id: &str, todo: TodoStateRecord) -> Result<()> {
        let Some(todo_id) = todo.id.clone() else {
            bail!("a todo state written alone needs a todo id (task {id})");
        };
        self.touch_task(id)?;
        let position = self.connection.select_row_bound::<(&str, &str), i64>(
            "SELECT position FROM todos WHERE task_id = ?1 AND todo_id = ?2",
        )?((id, &todo_id))?;
        match position {
            Some(position) => {
                self.connection.exec_bound::<(String, Option<String>, &str, i64)>(
                    "UPDATE todos SET state = ?1, started = ?2 WHERE task_id = ?3 AND position = ?4",
                )?((
                    enum_to_text(&todo.state)?,
                    todo.started.map(|started| started.to_string()),
                    id,
                    position,
                ))?;
            }
            None => {
                let next = self.connection.select_row_bound::<&str, i64>(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM todos WHERE task_id = ?",
                )?(id)?
                .unwrap_or(0);
                insert_todo(self.connection, id, next as usize, &todo)?;
            }
        }
        Ok(())
    }

    pub fn append_history(&mut self, id: &str, entry: HistoryEntry) -> Result<()> {
        self.only_kv_in_hook(&format!("the history of task {id}"))?;
        insert_history(self.connection, id, &entry)?;
        self.task_ids.insert(id.to_string());
        self.dirty = true;
        Ok(())
    }

    /// Removes every row of the task.
    pub fn delete_task(&mut self, id: &str) -> Result<()> {
        self.touch_task(id)?;
        for statement in [
            "DELETE FROM tasks WHERE id = ?",
            "DELETE FROM todos WHERE task_id = ?",
            "DELETE FROM history WHERE task_id = ?",
        ] {
            self.connection.exec_bound::<&str>(statement)?(id)?;
        }
        Ok(())
    }

    pub fn change(&self, change_id: &str) -> Result<Option<ChangeRow>> {
        changes::read_change(self.connection, change_id)
    }

    /// Inserts `row`, or replaces the row with its id.
    pub fn set_change(&mut self, row: &ChangeRow) -> Result<()> {
        self.only_kv_in_hook("a change")?;
        changes::write_change(self.connection, row)?;
        self.dirty = true;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        read_meta(self.connection, key)
    }

    /// Sets the `meta` key `key`; not for the verified commit or the generation.
    pub(crate) fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.only_kv_in_hook(key)?;
        debug_assert!(key != VERIFIED_COMMIT_KEY && key != GENERATION_KEY);
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?((key, value))?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn delete_meta(&mut self, key: &str) -> Result<()> {
        self.only_kv_in_hook(key)?;
        self.connection
            .exec_bound::<&str>("DELETE FROM meta WHERE key = ?")?(key)?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn add_landing(&mut self, change_id: &str, record: &str) -> Result<()> {
        self.only_kv_in_hook("a landing")?;
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO landings (change_id, record_toml) VALUES (?1, ?2)",
        )?((change_id, record))?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn add_resolution(&mut self, commit: &str, record: &str) -> Result<()> {
        self.only_kv_in_hook("a resolution")?;
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO resolutions (commit_id, record_toml) VALUES (?1, ?2)",
        )?((commit, record))?;
        self.dirty = true;
        Ok(())
    }

    pub fn landing_queue(&self) -> Result<Vec<String>> {
        read_queue(self.connection)
    }

    /// Puts `change_id` at the back of the landing queue, unless it is in it already.
    pub(crate) fn queue_push(&mut self, change_id: &str) -> Result<()> {
        self.only_kv_in_hook("the landing queue")?;
        self.connection.exec_bound::<&str>(
            "INSERT INTO landing_queue (change_id) VALUES (?) ON CONFLICT (change_id) DO NOTHING",
        )?(change_id)?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn queue_remove(&mut self, change_id: &str) -> Result<()> {
        self.only_kv_in_hook("the landing queue")?;
        self.connection
            .exec_bound::<&str>("DELETE FROM landing_queue WHERE change_id = ?")?(
            change_id
        )?;
        self.dirty = true;
        Ok(())
    }

    /// Empties the landing queue: at startup, when nobody of a former process waits in it.
    pub(crate) fn queue_clear(&mut self) -> Result<()> {
        self.only_kv_in_hook("the landing queue")?;
        self.connection.exec("DELETE FROM landing_queue")?()?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn set_check_result(
        &mut self,
        change_id: &str,
        check: &str,
        record: &str,
    ) -> Result<()> {
        self.only_kv_in_hook("a check result")?;
        self.connection.exec_bound::<(&str, &str, &str)>(
            "INSERT INTO check_results (change_id, check_name, result_toml) VALUES (?1, ?2, ?3)
             ON CONFLICT (change_id, check_name) DO UPDATE SET result_toml = excluded.result_toml",
        )?((change_id, check, record))?;
        self.dirty = true;
        Ok(())
    }

    pub fn kv(&mut self, namespace: &str) -> KvTx<'_, 'a> {
        KvTx {
            tx: self,
            namespace: namespace.to_string(),
        }
    }

    /// Applies `ops` to the current rows (staged state, a router's state-only write). Returns
    /// the notes of ops that kept a newer value.
    pub fn apply_ops(&mut self, ops: &[StateOp]) -> Result<Vec<String>> {
        state_ops::apply(self, ops)
    }

    /// Keeps `ops` for a pending change, to apply when it lands.
    pub fn stage(&mut self, change_id: &str, ops: &[StateOp]) -> Result<()> {
        self.only_kv_in_hook("staged state")?;
        let json = serde_json::to_string(ops).context("serializing staged state")?;
        self.connection.exec_bound::<(&str, &str)>(
            "INSERT INTO staged_state (change_id, ops) VALUES (?1, ?2)
             ON CONFLICT (change_id) DO UPDATE SET ops = excluded.ops",
        )?((change_id, &json))?;
        self.dirty = true;
        Ok(())
    }

    pub fn staged(&self, change_id: &str) -> Result<Option<Vec<StateOp>>> {
        read_staged(self.connection, change_id)
    }

    pub fn drop_staged(&mut self, change_id: &str) -> Result<()> {
        self.only_kv_in_hook("staged state")?;
        self.connection
            .exec_bound::<&str>("DELETE FROM staged_state WHERE change_id = ?")?(change_id)?;
        self.dirty = true;
        Ok(())
    }
}

/// One `kv` namespace in a transaction.
pub struct KvTx<'t, 'a> {
    tx: &'t mut StateTx<'a>,
    namespace: String,
}

impl KvTx<'_, '_> {
    pub fn get(&self, key: &str) -> Result<Option<toml::Value>> {
        kv_get(self.tx.connection, &self.namespace, key)
    }

    /// The entries whose key starts with `prefix`, ordered by key.
    pub fn scan(&self, prefix: &str) -> Result<Vec<(String, toml::Value)>> {
        kv_scan(self.tx.connection, &self.namespace, prefix)
    }

    pub fn put(&mut self, key: &str, value: toml::Value) -> Result<()> {
        let text = value_to_text(value)?;
        self.tx.connection.exec_bound::<(&str, &str, &str)>(
            "INSERT INTO kv (namespace, key, value_toml) VALUES (?1, ?2, ?3)
             ON CONFLICT (namespace, key) DO UPDATE SET value_toml = excluded.value_toml",
        )?((&self.namespace, key, &text))?;
        self.written();
        Ok(())
    }

    pub fn delete(&mut self, key: &str) -> Result<()> {
        self.tx
            .connection
            .exec_bound::<(&str, &str)>("DELETE FROM kv WHERE namespace = ?1 AND key = ?2")?(
            (
            &self.namespace,
            key,
        )
        )?;
        self.written();
        Ok(())
    }

    fn written(&mut self) {
        self.tx.namespaces.insert(self.namespace.clone());
        self.tx.dirty = true;
    }
}

fn insert_todo(
    connection: &Connection,
    task_id: &str,
    position: usize,
    todo: &TodoStateRecord,
) -> Result<()> {
    connection.exec_bound::<(&str, i64, Option<&str>, String, Option<String>)>(
        "INSERT INTO todos (task_id, position, todo_id, state, started) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?((
        task_id,
        position as i64,
        todo.id.as_deref(),
        enum_to_text(&todo.state)?,
        todo.started.map(|started| started.to_string()),
    ))
}

fn insert_history(connection: &Connection, task_id: &str, entry: &HistoryEntry) -> Result<()> {
    let text = toml::to_string(entry).context("serializing a history entry")?;
    connection.exec_bound::<(&str, &str)>(
        "INSERT INTO history (task_id, seq, entry_toml)
         VALUES (?1, (SELECT COALESCE(MAX(seq) + 1, 0) FROM history WHERE task_id = ?1), ?2)",
    )?((task_id, &text))
}

fn read_task(connection: &Connection, id: &str) -> Result<Option<TaskStateRecord>> {
    let state = connection
        .select_row_bound::<&str, String>("SELECT state_toml FROM tasks WHERE id = ?")?(
        id
    )?;
    let todos = connection.select_bound::<&str, (Option<String>, String, Option<String>)>(
        "SELECT todo_id, state, started FROM todos WHERE task_id = ? ORDER BY position",
    )?(id)?;
    let history = connection.select_bound::<&str, String>(
        "SELECT entry_toml FROM history WHERE task_id = ? ORDER BY seq",
    )?(id)?;
    if state.is_none() && todos.is_empty() && history.is_empty() {
        return Ok(None);
    }
    let mut record = match state {
        Some(text) => toml::from_str::<TaskStateRecord>(&text)
            .with_context(|| format!("reading the state of task {id}"))?,
        None => TaskStateRecord::default(),
    };
    record.todos = todos
        .into_iter()
        .map(|(todo_id, state, started)| {
            Ok(TodoStateRecord {
                id: todo_id,
                state: enum_from_text::<TodoState>(&state)?,
                started: started
                    .map(|text| text.parse::<Timestamp>())
                    .transpose()
                    .with_context(|| format!("reading a todo start time of task {id}"))?,
            })
        })
        .collect::<Result<_>>()?;
    record.history = history
        .iter()
        .map(|text| {
            toml::from_str::<HistoryEntry>(text)
                .with_context(|| format!("reading the history of task {id}"))
        })
        .collect::<Result<_>>()?;
    Ok(Some(record))
}

fn read_task_ids(connection: &Connection) -> Result<Vec<TaskId>> {
    connection.select::<String>(
        "SELECT id FROM tasks UNION SELECT task_id FROM todos UNION SELECT task_id FROM history
         ORDER BY 1",
    )?()
}

fn read_queue(connection: &Connection) -> Result<Vec<String>> {
    connection.select::<String>("SELECT change_id FROM landing_queue ORDER BY seq")?()
}

fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection.select_row_bound::<&str, String>("SELECT value FROM meta WHERE key = ?")?(key)
}

fn read_generation(connection: &Connection) -> Result<u64> {
    let value = connection
        .select_row_bound::<&str, String>("SELECT value FROM meta WHERE key = ?")?(
        GENERATION_KEY
    )?;
    match value {
        Some(text) => text
            .parse()
            .with_context(|| format!("reading the generation {text:?}")),
        None => Ok(0),
    }
}

fn read_staged(connection: &Connection, change_id: &str) -> Result<Option<Vec<StateOp>>> {
    let ops = connection
        .select_row_bound::<&str, String>("SELECT ops FROM staged_state WHERE change_id = ?")?(
        change_id,
    )?;
    ops.map(|json| {
        serde_json::from_str(&json)
            .with_context(|| format!("reading the staged state of {change_id}"))
    })
    .transpose()
}

fn kv_get(connection: &Connection, namespace: &str, key: &str) -> Result<Option<toml::Value>> {
    connection.select_row_bound::<(&str, &str), String>(
        "SELECT value_toml FROM kv WHERE namespace = ?1 AND key = ?2",
    )?((namespace, key))?
    .map(|text| value_from_text(&text))
    .transpose()
}

fn kv_scan(
    connection: &Connection,
    namespace: &str,
    prefix: &str,
) -> Result<Vec<(String, toml::Value)>> {
    let rows = connection.select_bound::<(&str, &str), (String, String)>(
        "SELECT key, value_toml FROM kv
         WHERE namespace = ?1 AND substr(key, 1, length(?2)) = ?2
         ORDER BY key",
    )?((namespace, prefix))?;
    rows.into_iter()
        .map(|(key, text)| Ok((key, value_from_text(&text)?)))
        .collect()
}

/// A `kv` value is kept as the TOML of a one-key table, since TOML has no bare values.
fn value_to_text(value: toml::Value) -> Result<String> {
    let mut table = toml::Table::new();
    table.insert("value".to_string(), value);
    toml::to_string(&table).context("serializing a kv value")
}

fn value_from_text(text: &str) -> Result<toml::Value> {
    let mut table: toml::Table = toml::from_str(text).context("reading a kv value")?;
    table
        .remove("value")
        .context("a kv value has no `value` key")
}

fn enum_to_text<T: Serialize>(value: &T) -> Result<String> {
    match toml::Value::try_from(value)? {
        toml::Value::String(text) => Ok(text),
        other => bail!("expected a name, got {other}"),
    }
}

fn enum_from_text<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    Ok(toml::Value::String(text.to_string()).try_into()?)
}
