//! The landing queue of one project: FIFO rows in the state store's `landing_queue` table, and
//! the queue's holder, who takes `flock` on `state/landing.lock` so the lock dies with its
//! process. Waiters in this process wake on a [`Signal`].

use std::fs::{File, OpenOptions};
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use parking_lot::Mutex;

use crate::state::StateWriter;

/// Wakes everything that waits on it: a queue that moved, a change that landed or was
/// abandoned, a split master that was resolved.
#[derive(Default)]
pub(crate) struct Signal {
    waiters: Mutex<Vec<oneshot::Sender<()>>>,
}

impl Signal {
    /// A receiver that resolves on the next [`Self::notify`]. Take it before checking the
    /// condition waited for, so a notification between the check and the wait is not lost.
    pub(crate) fn listen(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.waiters.lock().push(sender);
        receiver
    }

    pub(crate) fn notify(&self) {
        for waiter in self.waiters.lock().drain(..) {
            waiter.send(()).ok();
        }
    }
}

/// `state/landing.lock`, held. Dropping it, or the death of the process, releases it.
pub(crate) struct LandingLock {
    _file: File,
}

/// Why the landing lock could not be taken.
pub(crate) enum LockRefused {
    /// Another process holds it.
    HeldBy(Option<u32>),
    Failed(anyhow::Error),
}

impl LandingLock {
    /// Takes the lock at `path` without waiting.
    #[cfg(unix)]
    pub(crate) fn try_take(path: &Path) -> Result<Self, LockRefused> {
        use std::os::fd::AsRawFd as _;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
            .map_err(LockRefused::Failed)?;
        // SAFETY: the descriptor is open for the duration of the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(LockRefused::HeldBy(crate::state::read_holder_pid(
                    &mut file,
                )));
            }
            return Err(LockRefused::Failed(
                anyhow::Error::new(error).context(format!("locking {}", path.display())),
            ));
        }
        let wrote = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| writeln!(file, "{}", std::process::id()))
            .and_then(|()| file.flush());
        wrote
            .with_context(|| format!("writing the pid into {}", path.display()))
            .map_err(LockRefused::Failed)?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    pub(crate) fn try_take(path: &Path) -> Result<Self, LockRefused> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
            .map_err(LockRefused::Failed)?;
        Ok(Self { _file: file })
    }
}

/// The change holding the landing lock in this process, and the lock.
pub(crate) struct Holder {
    pub(crate) change_id: String,
    _lock: LandingLock,
}

impl Holder {
    pub(crate) fn new(change_id: &str, lock: LandingLock) -> Self {
        Self {
            change_id: change_id.to_string(),
            _lock: lock,
        }
    }
}

/// The landing machinery one project shares across the clones of its `ProjectRepo`.
#[derive(Default)]
pub(crate) struct Engine {
    pub(crate) holder: Mutex<Option<Holder>>,
    pub(crate) signal: Signal,
    pub(crate) landing_events:
        Mutex<Vec<futures::channel::mpsc::UnboundedSender<(String, crate::LandingEvent)>>>,
    pub(crate) repo_events: Mutex<Vec<futures::channel::mpsc::UnboundedSender<crate::RepoEvent>>>,
    /// The checks registered in the app, as the last `land` with an app saw them.
    pub(crate) registered: Mutex<crate::landing_checks::CheckRegistry>,
    /// The checks added to this project only.
    pub(crate) local: Mutex<crate::landing_checks::CheckRegistry>,
    pub(crate) options: Mutex<crate::LandOptions>,
    /// The changes a `land` or `check` of this process is working on.
    pub(crate) landing: Mutex<std::collections::HashSet<String>>,
    #[cfg(test)]
    pub(crate) probe: Mutex<Option<crate::landing::Probe>>,
}

impl Engine {
    pub(crate) fn emit(&self, change_id: &str, event: crate::LandingEvent) {
        self.landing_events.lock().retain(|subscriber| {
            subscriber
                .unbounded_send((change_id.to_string(), event.clone()))
                .is_ok()
        });
    }

    pub(crate) fn emit_repo(&self, event: crate::RepoEvent) {
        self.repo_events
            .lock()
            .retain(|subscriber| subscriber.unbounded_send(event.clone()).is_ok());
    }

    /// The change holding the landing lock in this process.
    pub(crate) fn holder_id(&self) -> Option<String> {
        self.holder
            .lock()
            .as_ref()
            .map(|holder| holder.change_id.clone())
    }

    /// Marks `change_id` as being landed or checked by this process; `None` while another
    /// `land` or `check` of it runs. Dropping the guard unmarks it.
    pub(crate) fn start_landing(self: &Arc<Self>, change_id: &str) -> Option<LandingGuard> {
        if !self.landing.lock().insert(change_id.to_string()) {
            return None;
        }
        Some(LandingGuard {
            engine: self.clone(),
            change_id: change_id.to_string(),
        })
    }

    /// Releases the lock if `change_id` holds it.
    pub(crate) fn release(&self, change_id: &str) {
        let mut holder = self.holder.lock();
        if holder
            .as_ref()
            .is_some_and(|held| held.change_id == change_id)
        {
            *holder = None;
        }
    }
}

/// A change a `land` or `check` of this process is working on.
pub(crate) struct LandingGuard {
    engine: Arc<Engine>,
    change_id: String,
}

impl Drop for LandingGuard {
    fn drop(&mut self) {
        self.engine.landing.lock().remove(&self.change_id);
    }
}

/// A change's place at the head of the queue, holding the lock. Dropping it releases the lock,
/// takes the change out of the queue and wakes the next one, unless [`Self::keep`] was called
/// (a split master keeps the lock; a simulated crash leaves everything as a dead process would).
pub(crate) struct QueueSlot {
    engine: Arc<Engine>,
    writer: Arc<StateWriter>,
    change_id: String,
    keep: bool,
}

impl QueueSlot {
    pub(crate) fn new(engine: Arc<Engine>, writer: Arc<StateWriter>, change_id: &str) -> Self {
        Self {
            engine,
            writer,
            change_id: change_id.to_string(),
            keep: false,
        }
    }

    /// Keeps the lock held after the slot is dropped.
    pub(crate) fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        self.engine.release(&self.change_id);
        if let Err(error) = self.writer.transact(|tx| tx.queue_remove(&self.change_id)) {
            log::error!(
                "taking {} out of the landing queue: {error:#}",
                self.change_id
            );
        }
        self.engine.signal.notify();
    }
}
