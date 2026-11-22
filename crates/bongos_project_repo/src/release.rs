//! Moving a project's writer to another process: [`ProjectRepo::release_writer`] lets it go
//! after the work in flight here is done, and [`ProjectRepo::retake_writer`] takes it back when
//! the other process never took it.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::FutureExt as _;

use crate::create::ProjectRepo;
use crate::state::OpenError;

impl ProjectRepo {
    /// Lets this process's writer go so another process can take it. In order: the snapshot
    /// timer stops (a snapshot or push in progress ends first); the landing queue's current
    /// step ends (the change at the head of the queue in this process lets the landing lock go,
    /// waiting at most `queue_wait`; a step still running then is completed by the next writer's
    /// startup recovery, which never needs this process); the transaction in progress commits;
    /// then every later write is refused and `host.lock` is unlocked. The landing lock is let go
    /// as well. Reads keep working. The project's sink is not changed here: the caller points
    /// it at the next writer.
    pub async fn release_writer(&self, queue_wait: Duration) -> Result<()> {
        let writer = self
            .state
            .as_ref()
            .and_then(|state| state.opened_writer())
            .cloned()
            .context("this process does not hold the project's writer")?;
        // Released before stopped, so the timer never reads a stop for good here.
        self.engine.backup.released.store(true, Ordering::SeqCst);
        self.stop_backups();
        drop(self.engine.backup.branch_lock.lock().await);
        drop(self.engine.backup.push_lock.lock().await);

        let timer = self.engine.timer();
        let mut deadline = timer(queue_wait).fuse();
        loop {
            let changed = self.engine.signal.listen();
            if self.engine.holder.lock().is_none() {
                break;
            }
            futures::select! {
                _ = changed.fuse() => {}
                () = deadline => {
                    log::warn!(
                        "{}: a landing still held the queue after {}s; the next writer completes it",
                        self.location.id,
                        queue_wait.as_secs()
                    );
                    break;
                }
            }
        }

        writer.release();
        // The landing lock dies with the step that held it; a step still running here can no
        // longer write, so it is the next writer's to complete.
        self.engine.holder.lock().take();
        self.engine.signal.notify();
        Ok(())
    }

    /// Takes the writer back after [`Self::release_writer`], when the process it was given to
    /// never took it: the lock again (`HeldBy` while another process holds it), and the snapshot
    /// timer's work allowed again. Writes on this process's writer work again.
    pub fn retake_writer(&self) -> Result<(), OpenError> {
        let writer = self
            .state
            .as_ref()
            .and_then(|state| state.opened_writer())
            .cloned()
            .context("this process never opened the project's writer")?;
        writer.retake()?;
        self.engine.backup.stopped.store(false, Ordering::SeqCst);
        self.engine.backup.released.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Sends this project's writes to this process's writer again, after a sink was set.
    pub fn clear_sink(&self) {
        *self.sink.write() = None;
    }
}
