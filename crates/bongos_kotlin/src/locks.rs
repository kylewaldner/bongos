//! Cross-process advisory locks on files, shared by every task in one process.
//!
//! `flock` belongs to an open file description, so two handles on one lock file in one process
//! exclude each other and can deadlock that process. The registry therefore owns at most one open
//! `File` per lock path. A caller first takes the registry's async mutex for the path (tasks of one
//! process queue there, never on the file), then the file lock with `File::try_lock`, retried
//! after an async timer (10 ms doubling to 250 ms). Nothing calls the blocking `File::lock`, so no
//! executor thread ever waits on another process. Dropping the guard unlocks the file, then frees
//! the mutex.
//!
//! Two kinds of lock exist:
//! - the state lock, `<install_root>/manifest/.lock`, held only for short local reads and writes of
//!   the manifest cache and state file; a wait gives up after `STATE_LOCK_LIMIT`;
//! - one version lock per build, `<server dir>/.<version>.lock`, held while that build is
//!   installed; a wait has no limit but stops when its cancel flag is set.
//!
//! A task holding a lock never waits for another one. Lock files are never deleted: deleting a file
//! another process has open would split the lock.

use std::{
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use futures::{
    FutureExt as _,
    future::{BoxFuture, Either, select},
    lock::{Mutex as AsyncMutex, OwnedMutexGuard},
};

use crate::manifest::is_plain_name;

/// How long a wait for the state lock lasts before it gives up.
pub const STATE_LOCK_LIMIT: Duration = Duration::from_secs(5);

/// The directory under the install root that holds the manifest cache, the state file and the
/// state lock.
pub const MANIFEST_DIR: &str = "manifest";

const FIRST_BACKOFF: Duration = Duration::from_millis(10);
const MAX_BACKOFF: Duration = Duration::from_millis(250);

/// Time for the lock waits: the app passes its background executor, so tests run on fake time.
pub trait LockClock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

impl LockClock for gpui::BackgroundExecutor {
    fn now(&self) -> Instant {
        gpui::BackgroundExecutor::now(self)
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        self.timer(duration).boxed()
    }
}

/// A wait for a lock passed its limit.
#[derive(Debug)]
pub struct LockTimedOut {
    pub path: PathBuf,
    pub limit: Duration,
}

impl fmt::Display for LockTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} stayed locked for more than {} ms",
            self.path.display(),
            self.limit.as_millis()
        )
    }
}

impl std::error::Error for LockTimedOut {}

/// A wait for a lock stopped because its cancel flag was set.
#[derive(Debug)]
pub struct LockWaitCancelled {
    pub path: PathBuf,
}

impl fmt::Display for LockWaitCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the wait for {} was cancelled", self.path.display())
    }
}

impl std::error::Error for LockWaitCancelled {}

/// The lock file, opened once and kept open for the life of the registry.
#[derive(Default)]
struct Slot {
    file: Option<File>,
}

/// A held lock: the path's async mutex and the file lock. Dropping it unlocks the file first, then
/// frees the mutex.
pub struct LockGuard {
    path: PathBuf,
    slot: OwnedMutexGuard<Slot>,
}

impl LockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.slot.file
            && let Err(error) = file.unlock()
        {
            log::error!("unlocking {}: {error}", self.path.display());
        }
    }
}

impl fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockGuard")
            .field("path", &self.path)
            .finish()
    }
}

struct GlobalLockRegistry(Arc<LockRegistry>);

impl gpui::Global for GlobalLockRegistry {}

/// The process-wide lock registry. The app creates one; a test creates one per simulated process.
pub struct LockRegistry {
    clock: Arc<dyn LockClock>,
    state_limit: Duration,
    slots: Mutex<HashMap<PathBuf, Arc<AsyncMutex<Slot>>>>,
}

impl LockRegistry {
    pub fn new(clock: Arc<dyn LockClock>) -> Self {
        Self {
            clock,
            state_limit: STATE_LOCK_LIMIT,
            slots: Mutex::default(),
        }
    }

    /// The registry every task of this process shares (the installer and the background updates),
    /// made on first use on the background executor.
    pub fn global(cx: &mut gpui::App) -> Arc<Self> {
        if let Some(global) = cx.try_global::<GlobalLockRegistry>() {
            return global.0.clone();
        }
        let registry = Arc::new(Self::new(Arc::new(cx.background_executor().clone())));
        cx.set_global(GlobalLockRegistry(registry.clone()));
        registry
    }

    /// Replaces `STATE_LOCK_LIMIT`, for tests.
    pub fn with_state_limit(mut self, limit: Duration) -> Self {
        self.state_limit = limit;
        self
    }

    /// Takes the state lock of `install_root`, waiting at most the state limit.
    pub async fn state_lock(&self, install_root: &Path) -> Result<LockGuard> {
        self.acquire(&state_lock_path(install_root), Some(self.state_limit), None)
            .await
    }

    /// Takes the install lock of `version` under `server_dir`, waiting until it is free or `cancel`
    /// is set (then the error is `LockWaitCancelled`).
    pub async fn version_lock(
        &self,
        server_dir: &Path,
        version: &str,
        cancel: &AtomicBool,
    ) -> Result<LockGuard> {
        let path = version_lock_path(server_dir, version)?;
        self.acquire(&path, None, Some(cancel)).await
    }

    /// Takes the install lock of `version` under `server_dir` if it is free right now, in this
    /// process and in every other. `None` means busy; nothing waits.
    pub fn try_version_lock(&self, server_dir: &Path, version: &str) -> Result<Option<LockGuard>> {
        let path = version_lock_path(server_dir, version)?;
        self.try_acquire(&path)
    }

    /// Takes the lock at `path` if it is free right now. `None` means busy; nothing waits.
    pub fn try_acquire(&self, path: &Path) -> Result<Option<LockGuard>> {
        let (key, slot) = self.slot(path)?;
        let Some(mut slot) = slot.try_lock_owned() else {
            return Ok(None);
        };
        Ok(try_lock_file(&key, &mut slot)?.then(|| LockGuard { path: key, slot }))
    }

    /// Takes the lock at `path`. The wait fails with `LockTimedOut` after `limit`, or with
    /// `LockWaitCancelled` once `cancel` is set; it checks both between tries.
    pub async fn acquire(
        &self,
        path: &Path,
        limit: Option<Duration>,
        cancel: Option<&AtomicBool>,
    ) -> Result<LockGuard> {
        let deadline = limit.map(|limit| self.clock.now() + limit);
        let (key, slot) = self.slot(path)?;
        let mut backoff = FIRST_BACKOFF;

        // Queue on the mutex, waking between tries to honor the limit and the cancel flag. The
        // same lock future is kept, so this task keeps its place in the queue.
        let mut slot = match slot.try_lock_owned() {
            Some(slot) => slot,
            None => {
                let mut turn = slot.lock_owned();
                loop {
                    self.check_cancel(&key, cancel)?;
                    let wait = self.next_wait(&key, deadline, limit, backoff)?;
                    match select(turn, self.clock.sleep(wait)).await {
                        Either::Left((slot, _)) => break slot,
                        Either::Right((_, pending)) => turn = pending,
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        };

        loop {
            self.check_cancel(&key, cancel)?;
            if try_lock_file(&key, &mut slot)? {
                return Ok(LockGuard { path: key, slot });
            }
            let wait = self.next_wait(&key, deadline, limit, backoff)?;
            self.clock.sleep(wait).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn check_cancel(&self, path: &Path, cancel: Option<&AtomicBool>) -> Result<()> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::SeqCst)) {
            return Err(LockWaitCancelled {
                path: path.to_path_buf(),
            }
            .into());
        }
        Ok(())
    }

    /// How long to sleep before the next try, or `LockTimedOut` when the deadline has passed.
    fn next_wait(
        &self,
        path: &Path,
        deadline: Option<Instant>,
        limit: Option<Duration>,
        backoff: Duration,
    ) -> Result<Duration> {
        let Some(deadline) = deadline else {
            return Ok(backoff);
        };
        let now = self.clock.now();
        if now >= deadline {
            return Err(LockTimedOut {
                path: path.to_path_buf(),
                limit: limit.unwrap_or_default(),
            }
            .into());
        }
        Ok(backoff.min(deadline - now))
    }

    /// The registry entry for `path`, keyed by its canonical directory so that two spellings of one
    /// file share one handle.
    fn slot(&self, path: &Path) -> Result<(PathBuf, Arc<AsyncMutex<Slot>>)> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            bail!("{} is not a lock file path", path.display());
        };
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let key = fs::canonicalize(parent)
            .with_context(|| format!("resolving {}", parent.display()))?
            .join(name);
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        Ok((key, slot))
    }
}

/// One `try_lock` on the slot's file, opening it the first time. `false` means another process
/// (or another registry) holds it.
fn try_lock_file(path: &Path, slot: &mut Slot) -> Result<bool> {
    let file = match &mut slot.file {
        Some(file) => file,
        empty => empty.insert(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?,
        ),
    };
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// `<install_root>/manifest/.lock`.
pub fn state_lock_path(install_root: &Path) -> PathBuf {
    install_root.join(MANIFEST_DIR).join(".lock")
}

/// `<server_dir>/.<version>.lock`. A dot-file, so pruning the version dirs never sees it.
pub fn version_lock_path(server_dir: &Path, version: &str) -> Result<PathBuf> {
    if !is_plain_name(version) {
        bail!("{version:?} is not a plain version name");
    }
    Ok(server_dir.join(format!(".{version}.lock")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::executor::block_on;
    use gpui::TestAppContext;
    use std::sync::atomic::AtomicUsize;

    /// Real time, for tests that run on plain threads. A sleep blocks the thread, then yields once,
    /// so other futures joined on the same thread (another task of this "process") get polled.
    pub(crate) struct ThreadClock;

    impl LockClock for ThreadClock {
        fn now(&self) -> Instant {
            Instant::now()
        }

        fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
            let mut slept = false;
            futures::future::poll_fn(move |cx| {
                if slept {
                    return std::task::Poll::Ready(());
                }
                std::thread::sleep(duration);
                slept = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .boxed()
        }
    }

    pub(crate) fn thread_registry() -> Arc<LockRegistry> {
        Arc::new(LockRegistry::new(Arc::new(ThreadClock)))
    }

    fn fake_time_registry(cx: &TestAppContext) -> Arc<LockRegistry> {
        Arc::new(LockRegistry::new(Arc::new(cx.executor())))
    }

    #[gpui::test]
    fn test_two_registries_exclude_each_other_without_blocking(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let other_process = thread_registry();
        let this_process = fake_time_registry(cx);

        let held = block_on(other_process.state_lock(root.path())).unwrap();
        let task = cx.executor().spawn({
            let this_process = this_process.clone();
            let root = root.path().to_path_buf();
            async move { this_process.state_lock(&root).await.map(drop) }
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let mut task = Some(task);
        assert!(task.as_mut().unwrap().now_or_never().is_none());
        assert!(this_process.try_acquire(&held.path).unwrap().is_none());

        drop(held);
        cx.executor().advance_clock(MAX_BACKOFF);
        cx.run_until_parked();
        task.take().unwrap().now_or_never().unwrap().unwrap();
        assert!(
            other_process
                .try_acquire(&state_lock_path(root.path()))
                .unwrap()
                .is_some()
        );
    }

    #[gpui::test]
    fn test_two_tasks_through_one_registry_both_finish(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let registry = fake_time_registry(cx);
        let holders = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));

        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let registry = registry.clone();
                let server_dir = root.path().join("kotlin-lsp");
                let holders = holders.clone();
                let finished = finished.clone();
                let cancel = cancel.clone();
                let executor = cx.executor();
                cx.executor().spawn(async move {
                    let guard = registry
                        .version_lock(&server_dir, "263.1.0", &cancel)
                        .await
                        .unwrap();
                    assert_eq!(holders.fetch_add(1, Ordering::SeqCst), 0);
                    executor.timer(Duration::from_secs(1)).await;
                    holders.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                    finished.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        cx.run_until_parked();
        assert_eq!(holders.load(Ordering::SeqCst), 1);
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        for task in tasks {
            task.now_or_never().unwrap();
        }
    }

    #[gpui::test]
    fn test_state_lock_gives_up_after_its_limit(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let other_process = thread_registry();
        let this_process = Arc::new(
            LockRegistry::new(Arc::new(cx.executor())).with_state_limit(Duration::from_secs(2)),
        );
        let _held = block_on(other_process.state_lock(root.path())).unwrap();

        let task = cx.executor().spawn({
            let root = root.path().to_path_buf();
            async move { this_process.state_lock(&root).await.map(drop) }
        });
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        let error = task.now_or_never().unwrap().unwrap_err();
        assert!(error.downcast_ref::<LockTimedOut>().is_some(), "{error:#}");
    }

    #[gpui::test]
    fn test_version_lock_wait_stops_when_cancelled(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let server_dir = root.path().join("kotlin-lsp");
        let other_process = thread_registry();
        let this_process = fake_time_registry(cx);
        let _held = other_process
            .try_version_lock(&server_dir, "263.1.0")
            .unwrap()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        let task = cx.executor().spawn({
            let this_process = this_process.clone();
            let server_dir = server_dir.clone();
            let cancel = cancel.clone();
            async move {
                this_process
                    .version_lock(&server_dir, "263.1.0", &cancel)
                    .await
                    .map(drop)
            }
        });
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        let mut task = Some(task);
        assert!(task.as_mut().unwrap().now_or_never().is_none());

        cancel.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(MAX_BACKOFF);
        cx.run_until_parked();
        let error = task.take().unwrap().now_or_never().unwrap().unwrap_err();
        assert!(
            error.downcast_ref::<LockWaitCancelled>().is_some(),
            "{error:#}"
        );
        // The cancelled wait freed the registry's mutex: this process can take the slot again.
        let path = version_lock_path(&server_dir, "263.1.0").unwrap();
        let (_, slot) = this_process.slot(&path).unwrap();
        assert!(slot.try_lock_owned().is_some());
    }

    #[test]
    fn test_version_lock_paths_are_dot_files_and_refuse_odd_names() {
        let dir = Path::new("/data/kotlin-lsp");
        assert_eq!(
            version_lock_path(dir, "263.1.0").unwrap(),
            dir.join(".263.1.0.lock")
        );
        assert!(version_lock_path(dir, "../x").is_err());
        assert!(version_lock_path(dir, "").is_err());
    }
}
