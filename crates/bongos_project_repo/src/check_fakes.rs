//! Fake landing checks and amending checks for the tests of the landing engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_project_repo_model::RelPath;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::{
    AmendOutcome, AmendingCheck, CheckInput, CheckOutcome, CheckPhase, LandingCheck, Stamp,
};

/// What a fake check saw on one call: the tree id and the files of its root.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub(crate) tree: String,
    pub(crate) root: PathBuf,
    pub(crate) alpha: Option<String>,
}

/// A landing check that records every call and answers with the next queued outcome (then
/// passes). It may block its first call until released.
pub(crate) struct FakeCheck {
    pub(crate) name: &'static str,
    pub(crate) phase: CheckPhase,
    pub(crate) outcomes: Mutex<Vec<CheckOutcome>>,
    pub(crate) seen: Mutex<Vec<Seen>>,
    /// Sent when the first call starts; the call then waits for the receiver.
    pub(crate) gate: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    /// A file written into the working copy while the first call runs, as a person typing.
    pub(crate) typed: Mutex<Option<PathBuf>>,
}

impl FakeCheck {
    pub(crate) fn new(name: &'static str, phase: CheckPhase) -> Self {
        Self {
            name,
            phase,
            outcomes: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
            typed: Mutex::new(None),
        }
    }

    /// Answers the next calls with `outcomes`, in order.
    pub(crate) fn answering(self, outcomes: Vec<CheckOutcome>) -> Self {
        *self.outcomes.lock() = outcomes;
        self
    }

    pub(crate) fn calls(&self) -> usize {
        self.seen.lock().len()
    }
}

impl LandingCheck for FakeCheck {
    fn name(&self) -> &str {
        self.name
    }

    fn phase(&self) -> CheckPhase {
        self.phase
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let alpha =
            std::fs::read_to_string(input.root.join("definitions/tasks/alpha/task.toml")).ok();
        self.seen.lock().push(Seen {
            tree: input.tree.0.clone(),
            root: input.root.to_path_buf(),
            alpha,
        });
        if let Some(typed) = self.typed.lock().take() {
            std::fs::create_dir_all(typed.parent().unwrap()).unwrap();
            std::fs::write(&typed, "typed while checking\n").unwrap();
        }
        let outcome = {
            let mut outcomes = self.outcomes.lock();
            if outcomes.is_empty() {
                CheckOutcome::Pass
            } else {
                outcomes.remove(0)
            }
        };
        let gate = self.gate.lock().take();
        let root = input.root.to_path_buf();
        async move {
            if let Some((started, release)) = gate {
                started.send(()).ok();
                release.await.ok();
            }
            // The materialized tree is still there while the check runs.
            assert!(root.join("definitions/project.toml").is_file());
            outcome
        }
        .boxed()
    }
}

/// What a fake amending check does to the writable checkout; whether it changed anything.
pub(crate) type Edit = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// An amending check that applies `edit` and reports `Amended` when it changed something, at
/// most `times` times, then `Unchanged`.
pub(crate) struct FakeAmender {
    pub(crate) name: &'static str,
    pub(crate) edit: Edit,
    pub(crate) times: Mutex<usize>,
    pub(crate) stamp_paths: Vec<&'static str>,
    /// A stamp that stays the same whatever the tree (the default is the tree id).
    pub(crate) fixed_stamp: Option<&'static str>,
}

impl FakeAmender {
    pub(crate) fn new(name: &'static str, times: usize, edit: Edit) -> Self {
        Self {
            name,
            edit,
            times: Mutex::new(times),
            stamp_paths: Vec::new(),
            fixed_stamp: None,
        }
    }
}

impl AmendingCheck for FakeAmender {
    fn name(&self) -> &str {
        self.name
    }

    fn stamp(&self, input: &CheckInput) -> Stamp {
        match self.fixed_stamp {
            Some(stamp) => Stamp(stamp.to_string()),
            None => Stamp::tree(input.tree.0.clone()),
        }
    }

    fn stamp_paths(&self) -> Vec<RelPath> {
        self.stamp_paths
            .iter()
            .map(|path| RelPath::new(*path).unwrap())
            .collect()
    }

    fn amend(&self, _input: CheckInput, writable: &Path) -> BoxFuture<'static, AmendOutcome> {
        let outcome = {
            let mut times = self.times.lock();
            if *times == 0 || !(self.edit)(writable) {
                AmendOutcome::Unchanged
            } else {
                *times -= 1;
                AmendOutcome::Amended {
                    message: "edit".to_string(),
                }
            }
        };
        async move { outcome }.boxed()
    }
}

/// Replaces `from` with `to` in the file at `relative` under `definitions/` of the checkout.
pub(crate) fn replace_in(relative: &'static str, from: &'static str, to: &'static str) -> Edit {
    Arc::new(move |writable: &Path| {
        let path = writable.join("definitions").join(relative);
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains(to) || !text.contains(from) {
            return false;
        }
        std::fs::write(&path, text.replacen(from, to, 1)).unwrap();
        true
    })
}

/// Writes `text` at `relative` under `definitions/` of the checkout.
pub(crate) fn write_in(relative: &'static str, text: &'static str) -> Edit {
    Arc::new(move |writable: &Path| {
        let path = writable.join("definitions").join(relative);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text) {
            return false;
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        true
    })
}

/// A timer the test fires: every call waits until [`ManualTimer::fire`], or forever.
#[derive(Clone, Default)]
pub(crate) struct ManualTimer {
    waiting: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
}

impl ManualTimer {
    pub(crate) fn timer(&self) -> crate::landing_queue::Timer {
        let waiting = self.waiting.clone();
        Arc::new(move |_| {
            let (fire, fired) = oneshot::channel();
            waiting.lock().push(fire);
            async move {
                if fired.await.is_err() {
                    futures::future::pending::<()>().await;
                }
            }
            .boxed()
        })
    }

    /// Ends every wait started so far; how many there were.
    pub(crate) fn fire(&self) -> usize {
        let waiting = std::mem::take(&mut *self.waiting.lock());
        let count = waiting.len();
        for fire in waiting {
            fire.send(()).ok();
        }
        count
    }
}

/// A timer whose every wait has already ended.
pub(crate) fn elapsed_timer() -> crate::landing_queue::Timer {
    Arc::new(|_| async {}.boxed())
}
