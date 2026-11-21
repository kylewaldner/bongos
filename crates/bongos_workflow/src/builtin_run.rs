//! The shipped `merge` and `wait` program nodes as [`BuiltinNode`]s. Each reads its parameters from
//! the node's `with` and the run's [`RunBinding`], then hands the work to a thread, so `start`,
//! `poll`, `stop` and `recover` (called with the runtime locked) never wait. A thread reports its
//! end through `poll`; a try that was stopped is never reported.
//!
//! `merge` brings the run branch into the task branch in the task worktree
//! ([`builtin_merge::merge`]); `with.style` is `ff-or-merge` (the default), `ff-only` or
//! `merge-commit`. A conflict fails the try naming the files, with nothing left half done.
//! [`BuiltinNode::recover`] aborts a merge a crash left half done in the task worktree.
//!
//! `wait` has no runner to sleep for it, so its thread does the polling (D9): a pass that is
//! [`WaitOutcome::Waiting`] sleeps `with.interval_minutes` (default 5) on the [`WaitClock`] and
//! passes again, until the condition is satisfied, fails, or `with.give_up_minutes` (default 7
//! days) pass, which ends the try with "wait timed out". Its other `with` keys are `condition`,
//! `pr`, `command` (an argv list) and `merge_is_ship`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bongos_workflow_model::{Failure, FailureReason, NodeKey};
use serde_json::{Value, json};

use crate::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::builtin_wait::{self, WaitCondition, WaitOutcome, WaitRequest};
use crate::runtime::{Finish, RunBinding};
use crate::worktrees::{At, Git, task_git_dir};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_GIVE_UP: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The longest a sleeping wait goes without looking at whether it was stopped.
const SLEEP_SLICE: Duration = Duration::from_secs(1);

/// Registers the `merge` and `wait` built-ins in the global registry.
pub fn register(cx: &mut gpui::App) {
    BuiltinNodes::register("merge", Arc::new(MergeNode::new(Git::new("git"))), cx);
    BuiltinNodes::register(
        "wait",
        Arc::new(WaitNode::new(Arc::new(SystemClock::new()), "gh")),
        cx,
    );
}

type TryId = (String, NodeKey, u32);

#[derive(Default)]
struct TryState {
    running: HashSet<TryId>,
    stopped: HashSet<TryId>,
    finished: Vec<Finish>,
}

/// The tries a built-in runs on threads, and the ones that ended.
#[derive(Clone, Default)]
struct Tries(Arc<(Mutex<TryState>, Condvar)>);

impl Tries {
    fn lock(&self) -> std::sync::MutexGuard<'_, TryState> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(&self, id: &TryId) {
        self.lock().running.insert(id.clone());
    }

    fn cancelled(&self, id: &TryId) -> bool {
        self.lock().stopped.contains(id)
    }

    /// A thread ended: its result is reported unless the try was stopped.
    fn end(&self, id: &TryId, finish: Finish) {
        let mut state = self.lock();
        state.running.remove(id);
        if !state.stopped.remove(id) {
            state.finished.push(finish);
        }
        self.0.1.notify_all();
    }

    fn poll(&self) -> Vec<Finish> {
        std::mem::take(&mut self.lock().finished)
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        let mut state = self.lock();
        let matches = |run: &str, key: &NodeKey| {
            tries
                .iter()
                .any(|(stopped_run, stopped_key)| stopped_run == run && stopped_key == key)
        };
        state
            .finished
            .retain(|finish| !matches(&finish.run, &finish.key));
        let ids: Vec<TryId> = state
            .running
            .iter()
            .filter(|(run, key, _)| matches(run, key))
            .cloned()
            .collect();
        state.stopped.extend(ids);
    }

    /// Runs `work` on a thread and reports its result as the end of `id`.
    fn spawn(
        &self,
        id: TryId,
        work: impl FnOnce(&Tries, &TryId) -> Result<Value, Failure> + Send + 'static,
    ) -> Result<(), Failure> {
        self.begin(&id);
        let tries = self.clone();
        let thread_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("workflow-builtin".to_string())
            .spawn(move || {
                let began = Instant::now();
                let outcome = work(&tries, &thread_id);
                let (run, key, try_number) = thread_id.clone();
                let finish = Finish {
                    outcome,
                    elapsed_ms: began.elapsed().as_millis() as u64,
                    ..Finish::ok(run, key, try_number, Value::Null)
                };
                tries.end(&thread_id, finish);
            });
        if let Err(error) = spawned {
            self.lock().running.remove(&id);
            return Err(Failure::new(
                FailureReason::WorktreeUnavailable,
                Some(id.1),
                format!("could not start a thread: {error}"),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn wait_for(&self, count: usize) -> Vec<Finish> {
        let mut state = self.lock();
        while state.finished.len() < count {
            state = self.0.1.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        std::mem::take(&mut state.finished)
    }
}

fn failure(reason: FailureReason, key: &NodeKey, detail: impl Into<String>) -> Failure {
    Failure::new(reason, Some(key.clone()), detail)
}

fn invalid(key: &NodeKey, detail: impl Into<String>) -> Failure {
    failure(FailureReason::ProgramResultInvalid, key, detail)
}

fn binding<'a>(start: &BuiltinStart<'a>) -> Result<&'a RunBinding, Failure> {
    start.binding.ok_or_else(|| {
        failure(
            FailureReason::WorktreeUnavailable,
            start.key,
            "the run has no task worktree: it was not bound to a task",
        )
    })
}

fn node_with<'a>(start: &BuiltinStart<'a>) -> Result<&'a toml::Table, Failure> {
    start
        .definition
        .node(&start.key.node)
        .map(|node| &node.with)
        .ok_or_else(|| {
            invalid(
                start.key,
                format!("no node '{}' in the definition", start.key.node),
            )
        })
}

/// The `merge` built-in.
pub struct MergeNode {
    git: Git,
    tries: Tries,
    /// The task worktree and the run's git directory each run's merge started in, for [`BuiltinNode::recover`].
    worktrees: Mutex<HashMap<String, (PathBuf, PathBuf)>>,
    recoveries: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl MergeNode {
    pub fn new(git: Git) -> Self {
        Self {
            git,
            tries: Tries::default(),
            worktrees: Mutex::default(),
            recoveries: Mutex::default(),
        }
    }

    /// Waits for `count` tries to end and returns them.
    #[cfg(test)]
    pub(crate) fn wait_finished(&self, count: usize) -> Vec<Finish> {
        self.tries.wait_for(count)
    }

    /// Waits for the recoveries `recover` started.
    #[cfg(test)]
    pub(crate) fn join_recoveries(&self) {
        let handles = std::mem::take(&mut *self.recoveries.lock().unwrap());
        for handle in handles {
            handle.join().unwrap();
        }
    }

    fn request(start: &BuiltinStart<'_>) -> Result<MergeRequest, Failure> {
        let binding = binding(start)?;
        let with = node_with(start)?;
        let style = match with.get("style").and_then(toml::Value::as_str) {
            None => MergeStyle::default(),
            Some(text) => MergeStyle::parse(text).ok_or_else(|| {
                invalid(
                    start.key,
                    format!(
                        "`with.style` must be \"ff-or-merge\", \"ff-only\" or \"merge-commit\", not \"{text}\""
                    ),
                )
            })?,
        };
        let (Some(task_worktree), Some(task_branch)) =
            (&binding.checkout.worktree, &binding.checkout.branch)
        else {
            return Err(failure(
                FailureReason::WorktreeUnavailable,
                start.key,
                "the task has no worktree or branch to merge into",
            ));
        };
        Ok(MergeRequest {
            task_worktree: task_worktree.clone(),
            run_git_dir: binding.worktree.git_dir.clone(),
            task_branch: task_branch.clone(),
            run_branch: binding.worktree.branch.clone(),
            run_id: start.run.to_string(),
            workflow: binding.workflow.clone(),
            todo: binding.task_id.clone(),
            style,
        })
    }
}

/// The merge's outcome as a try's result or failure.
async fn run_merge(git: &Git, request: &MergeRequest, key: &NodeKey) -> Result<Value, Failure> {
    match builtin_merge::merge(git, request).await {
        // Only the declared field: a definition's output schema rejects any other.
        Ok(Merged::FastForward { .. } | Merged::MergeCommit { .. }) => {
            Ok(json!({ "result": "merged" }))
        }
        Err(MergeRefusal::Conflict(files)) => Err(failure(
            FailureReason::MergeConflict,
            key,
            MergeRefusal::Conflict(files).to_string(),
        )),
        Err(refusal) => Err(failure(
            FailureReason::WorktreeUnavailable,
            key,
            refusal.to_string(),
        )),
    }
}

/// Aborts a merge in progress in `worktree`; does nothing when there is none.
async fn abort_half_done(git: &Git, worktree: &Path, run_git_dir: &Path) {
    let Ok(git_dir) = task_git_dir(worktree, run_git_dir) else {
        log::error!(
            "workflows: the task worktree's git directory is not trusted; no merge aborted"
        );
        return;
    };
    let git_dir = git_dir.as_path();
    let at = At {
        git_dir,
        directory: worktree,
        job: false,
    };
    let in_progress = git
        .run_at(at, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .await
        .is_ok();
    if in_progress && let Err(error) = git.run_at(at, &["merge", "--abort"]).await {
        log::error!("workflows: could not abort a half-done merge: {error}");
    }
}

impl BuiltinNode for MergeNode {
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let request = Self::request(&start)?;
        self.worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                start.run.to_string(),
                (request.task_worktree.clone(), request.run_git_dir.clone()),
            );
        let git = self.git.clone();
        let key = start.key.clone();
        let id = (start.run.to_string(), key.clone(), start.try_number);
        self.tries.spawn(id, move |_, _| {
            futures::executor::block_on(run_merge(&git, &request, &key))
        })
    }

    fn poll(&self) -> Vec<Finish> {
        self.tries.poll()
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        self.tries.stop(tries);
    }

    fn recover(&self, run: &str, _keys: &[NodeKey]) {
        let Some((worktree, git_dir)) = self
            .worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .cloned()
        else {
            return;
        };
        let git = self.git.clone();
        let handle = std::thread::Builder::new()
            .name("workflow-merge-recover".to_string())
            .spawn(move || futures::executor::block_on(abort_half_done(&git, &worktree, &git_dir)));
        if let Ok(handle) = handle {
            self.recoveries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(handle);
        }
    }
}

/// Time for a `wait`: tests use a fake one so nothing really sleeps.
pub trait WaitClock: Send + Sync + 'static {
    /// The time since some fixed start.
    fn now(&self) -> Duration;
    /// Lets `duration` pass.
    fn sleep(&self, duration: Duration);
}

/// The real clock.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitClock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The `wait` built-in.
pub struct WaitNode {
    clock: Arc<dyn WaitClock>,
    gh: PathBuf,
    tries: Tries,
}

impl WaitNode {
    pub fn new(clock: Arc<dyn WaitClock>, gh: impl Into<PathBuf>) -> Self {
        Self {
            clock,
            gh: gh.into(),
            tries: Tries::default(),
        }
    }

    /// Waits for `count` tries to end and returns them.
    #[cfg(test)]
    pub(crate) fn wait_finished(&self, count: usize) -> Vec<Finish> {
        self.tries.wait_for(count)
    }

    /// The first pass's request, and the poll interval.
    fn request(&self, start: &BuiltinStart<'_>) -> Result<(WaitRequest, Duration), Failure> {
        let binding = binding(start)?;
        let with = node_with(start)?;
        let key = start.key;
        let condition = with
            .get("condition")
            .and_then(toml::Value::as_str)
            .and_then(WaitCondition::parse)
            .ok_or_else(|| {
                invalid(
                    key,
                    "`with.condition` must be \"ci_green\", \"pr_merged\", \"deploy\" or \"command\"",
                )
            })?;
        let command = match with.get("command") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .ok_or_else(|| invalid(key, "`with.command` must be a list of text"))?,
        };
        let minutes = |name: &str, default: Duration| -> Result<Duration, Failure> {
            match with.get(name) {
                None => Ok(default),
                Some(value) => value
                    .as_integer()
                    .and_then(|minutes| u64::try_from(minutes).ok())
                    .map(|minutes| Duration::from_secs(minutes * 60))
                    .ok_or_else(|| invalid(key, format!("`with.{name}` must be whole minutes"))),
            }
        };
        let interval = minutes("interval_minutes", DEFAULT_INTERVAL)?.max(Duration::from_secs(60));
        let give_up = minutes("give_up_minutes", DEFAULT_GIVE_UP)?;
        let worktree = binding.checkout.worktree.clone().ok_or_else(|| {
            failure(
                FailureReason::WorktreeUnavailable,
                key,
                "the task has no worktree to wait in",
            )
        })?;
        Ok((
            WaitRequest {
                condition,
                pr: with
                    .get("pr")
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                command,
                merge_is_ship: with
                    .get("merge_is_ship")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false),
                worktree,
                gh: self.gh.clone(),
                elapsed: Duration::ZERO,
                give_up: Some(give_up),
            },
            interval,
        ))
    }
}

/// Passes until the condition is satisfied, fails or gives up; `None` when stopped meanwhile.
fn poll_wait(
    clock: &dyn WaitClock,
    tries: &Tries,
    id: &TryId,
    mut request: WaitRequest,
    interval: Duration,
) -> Result<Value, Failure> {
    let key = &id.1;
    let began = clock.now();
    loop {
        if tries.cancelled(id) {
            return Ok(Value::Null);
        }
        request.elapsed = clock.now().saturating_sub(began);
        match futures::executor::block_on(builtin_wait::wait(&request)) {
            WaitOutcome::Satisfied => return Ok(json!({ "result": builtin_wait::SATISFIED })),
            WaitOutcome::TimedOut(message) => {
                return Err(failure(FailureReason::WaitTimedOut, key, message));
            }
            WaitOutcome::Failed(message) => return Err(invalid(key, message)),
            WaitOutcome::OpenGate(question) => {
                return Err(invalid(
                    key,
                    format!("a built-in wait cannot open a gate: {question}"),
                ));
            }
            WaitOutcome::Waiting => {}
        }
        // Sleep to the next pass, but not past the give-up time, and in slices so a stop is seen.
        let give_up = request.give_up.unwrap_or(DEFAULT_GIVE_UP);
        let due = (clock.now() + interval).min(began + give_up);
        loop {
            let now = clock.now();
            if now >= due || tries.cancelled(id) {
                break;
            }
            clock.sleep((due - now).min(SLEEP_SLICE));
        }
    }
}

impl BuiltinNode for WaitNode {
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let (request, interval) = self.request(&start)?;
        let clock = self.clock.clone();
        let id = (start.run.to_string(), start.key.clone(), start.try_number);
        self.tries.spawn(id, move |tries, id| {
            poll_wait(clock.as_ref(), tries, id, request, interval)
        })
    }

    fn poll(&self) -> Vec<Finish> {
        self.tries.poll()
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        self.tries.stop(tries);
    }
}
