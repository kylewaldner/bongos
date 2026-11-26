//! The machine store: the pools every project of this user shares, in `machine.toml` of one
//! directory (mode 0700; `pools.machine_store_dir`, by default `pools` in the state directory),
//! outside every project folder and repository and never part of a snapshot.
//!
//! A transaction opens `machine.lock` in the same directory and takes `flock(LOCK_EX)` on it,
//! trying without blocking every 50 ms up to `pools.lock_timeout_seconds` and then refusing with
//! the pid of the holder (which it writes into the lock file while it holds it). Holding the lock
//! it reads `machine.toml` (none is an empty table), prunes, applies the change, grants, bumps the
//! generation, writes the file with `write_atomic` and unlocks. The kernel drops the lock of a
//! process that dies. Readers read the file without the lock: the rename is atomic, so they see
//! the old table or the new one, never a torn one.
//!
//! A transaction blocks while it waits for the lock: run it on the background executor
//! ([`MachineStore::transact_in_background`]); the foreground only ever tries once
//! ([`LockWait::once`]).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bongos_pools_model::{GuardState, Hold, PoolKind, PoolState, PoolTable, WaitEdge, Waiter};
use bongos_work_model::files::write_atomic;
use gpui::{BackgroundExecutor, Task};
use serde::{Deserialize, Serialize};

use crate::PoolsSettings;
use crate::declare::{Declaration, DeclareOutcome, KindClash, PoolConfig, ProjectRef, SizeWarning};
use crate::store::{PoolStore, StoreContext, Transacted, is_mixed};

pub const MACHINE_FILE: &str = "machine.toml";
pub const LOCK_FILE: &str = "machine.lock";
/// The version of `machine.toml` this build reads and writes.
pub const MACHINE_SCHEMA: u32 = 1;
/// How often a busy lock is tried again.
pub const LOCK_RETRY: Duration = Duration::from_millis(50);

/// The machine store's directory: `pools.machine_store_dir`, else `pools` in the state directory.
pub fn machine_store_dir(settings: &PoolsSettings) -> PathBuf {
    settings
        .machine_store_dir
        .clone()
        .unwrap_or_else(|| paths::state_dir().join("pools"))
}

/// How a transaction waits for a busy lock: how many tries in all, and what it does between two.
#[derive(Clone)]
pub struct LockWait {
    attempts: u64,
    pause: Arc<dyn Fn() + Send + Sync>,
}

impl LockWait {
    /// Tries every `interval` until `timeout` has passed.
    pub fn every(interval: Duration, timeout: Duration) -> LockWait {
        let interval_ms = interval.as_millis().max(1);
        let tries = timeout.as_millis().div_ceil(interval_ms);
        LockWait {
            attempts: u64::try_from(tries).unwrap_or(u64::MAX).saturating_add(1),
            pause: Arc::new(move || std::thread::sleep(interval)),
        }
    }

    /// Every [`LOCK_RETRY`] up to `pools.lock_timeout_seconds`.
    pub fn from_settings(settings: &PoolsSettings) -> LockWait {
        LockWait::every(LOCK_RETRY, settings.lock_timeout)
    }

    /// One try only, for the foreground: a busy lock answers at once.
    pub fn once() -> LockWait {
        LockWait {
            attempts: 1,
            pause: Arc::new(|| {}),
        }
    }

    /// How many tries in all.
    pub fn attempts(&self) -> u64 {
        self.attempts
    }

    /// `attempts` tries with `pause` between two: the seam tests use to wait without a clock.
    pub fn with_pause(attempts: u64, pause: impl Fn() + Send + Sync + 'static) -> LockWait {
        LockWait {
            attempts: attempts.max(1),
            pause: Arc::new(pause),
        }
    }
}

/// The lock stayed busy for every try.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the machine pool store is busy: {} holds {}", holder(.pid), .lock.display())]
pub struct LockBusy {
    pub lock: PathBuf,
    /// The holder's pid, when it could be read from the lock file.
    pub pid: Option<u32>,
}

fn holder(pid: &Option<u32>) -> String {
    match pid {
        Some(pid) => format!("process {pid}"),
        None => "another process".to_string(),
    }
}

/// A held `flock` on `machine.lock`. Dropping it empties the pid and unlocks.
pub(crate) struct MachineLock {
    file: File,
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // Emptied before the unlock, so a later holder's pid is never mistaken for this one's.
        self.file.set_len(0).ok();
        // SAFETY: flock only changes the lock on this open file.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Whether the lock was taken.
enum Try {
    Taken,
    Busy,
}

fn try_flock(file: &File) -> Result<Try> {
    loop {
        // SAFETY: flock only takes a lock on this open file; LOCK_NB never blocks.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Try::Taken);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            ErrorKind::WouldBlock => return Ok(Try::Busy),
            ErrorKind::Interrupted => continue,
            _ => return Err(error).context("locking the machine pool store"),
        }
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// What `machine.toml` keeps beside each pool's table: who set its kind and size and who
/// declares it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolMeta {
    /// The project whose kind and size stand; `None` once no project declares it.
    pub set_by: Option<ProjectRef>,
    /// Every project that declares it, with its kind and size.
    pub declared: Vec<Declaration>,
}

/// The whole of `machine.toml`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MachineFile {
    pub generation: u64,
    pub table: PoolTable,
    pub meta: BTreeMap<String, PoolMeta>,
}

#[derive(Serialize, Deserialize)]
struct DiskFile {
    schema: u32,
    generation: u64,
    next_ticket: u64,
    #[serde(default)]
    pools: BTreeMap<String, DiskPool>,
    /// The project side of the wait-for graph of each `mixed` waiter (D6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    edges: Vec<WaitEdge>,
}

#[derive(Serialize, Deserialize)]
struct DiskPool {
    kind: PoolKind,
    size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    set_by: Option<ProjectRef>,
    #[serde(default)]
    declared: Vec<Declaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guard: Option<GuardState>,
    #[serde(default)]
    holds: Vec<Hold>,
    #[serde(default)]
    waiters: Vec<Waiter>,
}

impl MachineFile {
    fn parse(text: &str) -> Result<MachineFile> {
        let disk: DiskFile = toml::from_str(text).context("reading machine.toml")?;
        if disk.schema != MACHINE_SCHEMA {
            bail!(
                "machine.toml has schema {}, which this version cannot read",
                disk.schema
            );
        }
        let mut file = MachineFile {
            generation: disk.generation,
            table: PoolTable {
                pools: BTreeMap::new(),
                next_ticket: disk.next_ticket,
                edges: disk.edges,
            },
            meta: BTreeMap::new(),
        };
        for (name, pool) in disk.pools {
            file.table.pools.insert(
                name.clone(),
                PoolState {
                    kind: pool.kind,
                    size: pool.size,
                    guard: pool.guard,
                    holds: pool.holds,
                    waiters: pool.waiters,
                },
            );
            file.meta.insert(
                name,
                PoolMeta {
                    set_by: pool.set_by,
                    declared: pool.declared,
                },
            );
        }
        Ok(file)
    }

    fn to_text(&self) -> Result<String> {
        let pools = self
            .table
            .pools
            .iter()
            .map(|(name, pool)| {
                let meta = self.meta.get(name).cloned().unwrap_or_default();
                (
                    name.clone(),
                    DiskPool {
                        kind: pool.kind,
                        size: pool.size,
                        set_by: meta.set_by,
                        declared: meta.declared,
                        guard: pool.guard.clone(),
                        holds: pool.holds.clone(),
                        waiters: pool.waiters.clone(),
                    },
                )
            })
            .collect();
        toml::to_string(&DiskFile {
            schema: MACHINE_SCHEMA,
            generation: self.generation,
            next_ticket: self.table.next_ticket,
            pools,
            edges: self.table.edges.clone(),
        })
        .context("writing machine.toml")
    }
}

/// One handle on the machine store. Handles on the same directory, in this process or others,
/// serialize their transactions on the lock.
pub struct MachineStore {
    dir: PathBuf,
    context: StoreContext,
    wait: LockWait,
    generation: AtomicU64,
    /// Makes the next write of `machine.toml` fail: the seam tests use to inject a failed write.
    #[cfg(test)]
    pub(crate) fail_next_write: std::sync::atomic::AtomicBool,
}

impl MachineStore {
    /// A handle on the store in `dir`, which is created with mode 0700 (and set to it). Takes no
    /// lock and reads nothing.
    pub fn open(dir: PathBuf, context: StoreContext, wait: LockWait) -> Result<MachineStore> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making {} private", dir.display()))?;
        Ok(MachineStore {
            dir,
            context,
            wait,
            generation: AtomicU64::new(0),
            #[cfg(test)]
            fail_next_write: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn context(&self) -> &StoreContext {
        &self.context
    }

    pub fn file_path(&self) -> PathBuf {
        self.dir.join(MACHINE_FILE)
    }

    pub(crate) fn note_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::SeqCst);
    }

    /// Takes the lock, trying as [`LockWait`] says.
    pub(crate) fn lock(&self) -> Result<MachineLock> {
        let path = self.dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        for attempt in 1..=self.wait.attempts {
            match try_flock(&file)? {
                Try::Taken => {
                    file.set_len(0)?;
                    file.seek(SeekFrom::Start(0))?;
                    write!(file, "{}", std::process::id())?;
                    return Ok(MachineLock { file });
                }
                Try::Busy if attempt < self.wait.attempts => (self.wait.pause)(),
                Try::Busy => {}
            }
        }
        let pid = read_pid(&mut file);
        Err(LockBusy { lock: path, pid }.into())
    }

    /// The whole file without the lock; an empty table when there is none.
    pub fn read_file(&self) -> Result<MachineFile> {
        let path = self.file_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => MachineFile::parse(&text),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(MachineFile::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes `file` in place of `machine.toml`. The caller holds the lock.
    pub(crate) fn write_file(&self, _lock: &MachineLock, file: &MachineFile) -> Result<()> {
        let path = self.file_path();
        #[cfg(test)]
        if self.fail_next_write.swap(false, Ordering::SeqCst) {
            bail!("writing {}: injected failure", path.display());
        }
        write_atomic(&path, file.to_text()?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// One transaction over the whole file: lock, read, prune, `f`, grant, bump, write, unlock.
    /// An error from `f` writes nothing.
    pub fn try_transact_file<T>(
        &self,
        f: impl FnOnce(&mut MachineFile) -> Result<T>,
    ) -> Result<Transacted<T>> {
        let lock = self.lock()?;
        let mut file = self.read_file()?;
        let pruned = self.context.prune(&mut file.table);
        let value = f(&mut file)?;
        let grants = self.context.grant(&mut file.table, is_mixed);
        file.table.drop_stale_edges();
        file.generation += 1;
        self.write_file(&lock, &file)?;
        drop(lock);
        self.note_generation(file.generation);
        Ok(Transacted {
            value,
            pruned,
            grants,
            generation: file.generation,
        })
    }

    /// Registers `project`'s machine pools. The first project to declare a pool sets its kind and
    /// size (`set_by`); a later one with the same kind and another size is recorded in `declared`
    /// and warned; one with another kind is refused and not recorded. The `set_by` project's own
    /// declaration updates the pool, its kind included while nobody holds or waits for it (the
    /// declarations of the old kind are dropped). A pool `project` no longer declares loses its
    /// declaration; when `project` had set it, the first project still declaring it sets it from
    /// then on. A pool nobody declares any more is set afresh by the next project to declare it,
    /// any kind, once nobody holds or waits for it.
    pub fn declare(
        &self,
        project: &ProjectRef,
        configs: &[PoolConfig],
    ) -> Result<Transacted<DeclareOutcome>> {
        self.try_transact_file(|file| Ok(declare_machine(file, project, configs)))
    }

    /// [`MachineStore::declare`] on the background executor, where the lock may be waited for.
    pub fn declare_in_background(
        self: &Arc<Self>,
        executor: &BackgroundExecutor,
        project: ProjectRef,
        configs: Vec<PoolConfig>,
    ) -> Task<Result<Transacted<DeclareOutcome>>> {
        let store = self.clone();
        executor.spawn(async move { store.declare(&project, &configs) })
    }

    /// [`PoolStore::transact`] on the background executor, where the lock may be waited for.
    pub fn transact_in_background<T: Send + 'static>(
        self: &Arc<Self>,
        executor: &BackgroundExecutor,
        f: impl FnOnce(&mut PoolTable) -> T + Send + 'static,
    ) -> Task<Result<T>> {
        let store = self.clone();
        executor.spawn(async move { store.transact(f) })
    }
}

pub(crate) fn declare_machine(
    file: &mut MachineFile,
    project: &ProjectRef,
    configs: &[PoolConfig],
) -> DeclareOutcome {
    let mut outcome = DeclareOutcome::default();
    let declared: Vec<&str> = configs.iter().map(|config| config.name.as_str()).collect();
    // Declarations this project dropped.
    for (name, meta) in &mut file.meta {
        if declared.contains(&name.as_str()) {
            continue;
        }
        meta.declared
            .retain(|declaration| declaration.project.id != project.id);
        if meta
            .set_by
            .as_ref()
            .is_some_and(|set_by| set_by.id == project.id)
        {
            meta.set_by = meta.declared.first().map(|next| next.project.clone());
            if let (Some(next), Some(pool)) =
                (meta.declared.first(), file.table.pools.get_mut(name))
            {
                if pool.kind != PoolKind::Mutex {
                    pool.size = next.size;
                }
            }
        }
    }
    for config in configs {
        let declaration = Declaration {
            project: project.clone(),
            kind: config.kind,
            size: config.size,
        };
        let Some(pool) = file.table.pools.get_mut(&config.name) else {
            file.table.pools.insert(config.name.clone(), config.state());
            file.meta.insert(
                config.name.clone(),
                PoolMeta {
                    set_by: Some(project.clone()),
                    declared: vec![declaration],
                },
            );
            continue;
        };
        let meta = file.meta.entry(config.name.clone()).or_default();
        if pool.kind != config.kind {
            // Only the setting project, or anybody once nobody declares it, may change its kind,
            // and only while nobody holds or waits for it.
            let may_set = meta
                .set_by
                .as_ref()
                .is_none_or(|set_by| set_by.id == project.id);
            let in_use = !pool.holds.is_empty() || !pool.waiters.is_empty();
            if !may_set || in_use {
                outcome.refused.push(KindClash {
                    pool: config.name.clone(),
                    declared: config.kind,
                    stands: pool.kind,
                    set_by: if may_set { None } else { meta.set_by.clone() },
                });
                if !may_set {
                    meta.declared
                        .retain(|declaration| declaration.project.id != project.id);
                }
                continue;
            }
            meta.declared
                .retain(|declaration| declaration.kind == config.kind);
            meta.set_by = Some(project.clone());
        }
        match meta
            .declared
            .iter_mut()
            .find(|entry| entry.project.id == project.id)
        {
            Some(entry) => *entry = declaration,
            None => meta.declared.push(declaration),
        }
        let set_by = match &meta.set_by {
            Some(set_by) => set_by.clone(),
            None => {
                meta.set_by = Some(project.clone());
                project.clone()
            }
        };
        if set_by.id == project.id {
            config.apply_to(pool);
        } else if pool.kind == PoolKind::Slots && pool.size != config.size {
            outcome.warnings.push(SizeWarning {
                pool: config.name.clone(),
                declared: config.size,
                stands: pool.size,
                set_by: Some(set_by),
            });
        }
    }
    outcome
}

impl PoolStore for MachineStore {
    fn try_transact_full<T>(
        &self,
        f: impl FnOnce(&mut PoolTable) -> Result<T>,
    ) -> Result<Transacted<T>> {
        self.try_transact_file(|file| f(&mut file.table))
    }

    fn read(&self) -> Result<PoolTable> {
        let file = self.read_file()?;
        self.note_generation(file.generation);
        Ok(file.table)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}
