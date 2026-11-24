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
//! `pr`, `command` (an argv list), `merge_is_ship` and `env`: the names of the writer's variables
//! the wait's process gets besides its own (a name that is not set is skipped).
//!
//! Outside the `fallback` mode, how the passes start is decided once, in `start` before the poll
//! thread: the wait's sandbox policy (see [`builtin_wait`]) and the sandbox probe for it, then
//! [`unsandboxed::decide`] over the run's permit, as a program node decides. A start that needs
//! a person's answer returns `SandboxRequired` and one that is refused `SandboxRefused`, so the
//! runtime asks or refuses before anything runs, and one answer covers the run's program nodes
//! and waits alike.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bongos_workflow_model::unsandboxed::{self, Mode};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};
use sandbox::{GitSandboxPolicy, Sandbox, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
use serde_json::{Value, json};

use crate::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::builtin_wait::{self, SandboxWrap, WaitCondition, WaitLaunch, WaitOutcome, WaitRequest};
use crate::dispatch_program::{
    NO_ORIGIN, REMOTE_ENTRY, expand_remote, remote_hosts, scratch_dir, task_remote_url,
};
use crate::runner_program::{SandboxProbe, os_sandbox_probe};
use crate::runtime::{Finish, RunBinding};
use crate::worktrees::Git;

/// Every confined wait gets these of the writer's variables, when set.
const BASE_VARS: &[&str] = &["PATH", "HOME", "LANG"];

/// A `gh` condition also gets `gh`'s own variables for its account, host and configuration
/// directory. The installed `gh` was checked under the sandbox: it reads its keyring token with
/// none of the D-Bus variables, through the session bus the sandbox leaves reachable (see the
/// `sandbox` crate's docs). The enterprise tokens are dropped again in [`WaitNode::policy`] unless
/// `gh`'s host is the remote's or `GH_HOST` ([`builtin_wait::gh_env`]).
const GH_VARS: &[&str] = &[
    "GH_CONFIG_DIR",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_HOST",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// The refusal when a `gh` condition has no host to reach.
pub const NO_GH_HOST: &str =
    "cannot tell which host gh needs; declare permissions.network on the wait node";

/// The refusal when a `gh` condition cannot name its repository.
pub const NO_GH_REPO: &str = "cannot tell which repository gh needs";

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
    /// How many tries ended, polled or not.
    #[cfg(test)]
    ended: usize,
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
        #[cfg(test)]
        {
            state.ended += 1;
        }
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

    /// Blocks until `count` tries ended in all, polled or not; panics after a minute, so a try
    /// that never started fails its test instead of hanging it.
    #[cfg(test)]
    fn wait_ended(&self, count: usize) {
        let (state, timeout) = self
            .0
            .1
            .wait_timeout_while(self.lock(), Duration::from_secs(60), |state| {
                state.ended < count
            })
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !timeout.timed_out(),
            "{} of {count} tries ended",
            state.ended
        );
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
    /// The task worktree each run's merge started in, for [`BuiltinNode::recover`].
    worktrees: Mutex<HashMap<String, PathBuf>>,
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
async fn abort_half_done(git: &Git, worktree: &Path) {
    let in_progress = git
        .run(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .await
        .is_ok();
    if in_progress && let Err(error) = git.run(worktree, &["merge", "--abort"]).await {
        log::error!("workflows: could not abort a half-done merge: {error}");
    }
}

impl BuiltinNode for MergeNode {
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let request = Self::request(&start)?;
        self.worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(start.run.to_string(), request.task_worktree.clone());
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
        let Some(worktree) = self
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
            .spawn(move || futures::executor::block_on(abort_half_done(&git, &worktree)));
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

/// Reads one of the writer's environment variables.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The `wait` built-in.
pub struct WaitNode {
    clock: Arc<dyn WaitClock>,
    gh: PathBuf,
    tries: Tries,
    /// The sandbox probe, [`os_sandbox_probe`] unless a test says otherwise.
    probe: SandboxProbe,
    /// Git, for the task remote and the worktree's git directories.
    git: Git,
    /// The writer's environment, the process's own unless a test says otherwise.
    vars: EnvLookup,
    /// How a sandboxed pass sets up its sandbox, [`builtin_wait::wrap_in_sandbox`] unless a test
    /// says otherwise.
    sandbox_wrap: SandboxWrap,
}

impl WaitNode {
    pub fn new(clock: Arc<dyn WaitClock>, gh: impl Into<PathBuf>) -> Self {
        Self {
            clock,
            gh: gh.into(),
            tries: Tries::default(),
            probe: os_sandbox_probe(),
            git: Git::new("git"),
            vars: Arc::new(|name| std::env::var(name).ok()),
            sandbox_wrap: builtin_wait::wrap_in_sandbox,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sandbox_wrap(mut self, sandbox_wrap: SandboxWrap) -> Self {
        self.sandbox_wrap = sandbox_wrap;
        self
    }

    pub fn with_probe(mut self, probe: SandboxProbe) -> Self {
        self.probe = probe;
        self
    }

    pub fn with_git(mut self, git: Git) -> Self {
        self.git = git;
        self
    }

    pub fn with_vars(mut self, vars: EnvLookup) -> Self {
        self.vars = vars;
        self
    }

    /// Waits for `count` tries to end and returns them.
    #[cfg(test)]
    pub(crate) fn wait_finished(&self, count: usize) -> Vec<Finish> {
        self.tries.wait_for(count)
    }

    /// Waits until `count` tries ended in all, leaving them for [`BuiltinNode::poll`].
    #[cfg(test)]
    pub(crate) fn wait_ended(&self, count: usize) {
        self.tries.wait_ended(count);
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
        let names = match with.get("env") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .filter(|name| is_variable_name(name))
                                .map(str::to_string)
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| invalid(key, "`with.env` must be a list of variable names"))?,
        };
        let mut env = BTreeMap::new();
        let gh_vars = if condition.uses_gh() { GH_VARS } else { &[] };
        for name in BASE_VARS
            .iter()
            .chain(gh_vars)
            .copied()
            .chain(names.iter().map(String::as_str))
        {
            // A name that is not set is skipped, never passed empty.
            if let Some(value) = (self.vars)(name) {
                env.insert(name.to_string(), value);
            }
        }
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
                scratch: scratch_dir(binding, key, start.try_number),
                worktree,
                gh: self.gh.clone(),
                elapsed: Duration::ZERO,
                give_up: Some(give_up),
                launch: WaitLaunch::AsUser,
                env,
                repo: None,
                sandbox_wrap: self.sandbox_wrap,
            },
            interval,
        ))
    }

    /// Decides how the passes start, once, before the poll thread. Under `fallback` (and for a
    /// wait that runs nothing) they run as the user, as before. Otherwise this builds the wait's
    /// sandbox policy, probes it, and [`unsandboxed::decide`]s over the probe's answer, the run's
    /// permit and the mode: sandboxed, unsandboxed by a person's or the setting's word,
    /// `SandboxRequired` (the runtime asks) or `SandboxRefused`.
    fn confine(&self, start: &BuiltinStart<'_>, request: &mut WaitRequest) -> Result<(), Failure> {
        if start.mode == Mode::Fallback || !request.runs_a_process() {
            return Ok(());
        }
        let policy = self.policy(start, request)?;
        let availability = (self.probe)(&policy, &request.worktree).and_then(|()| {
            Sandbox::new(policy.clone())
                .map(drop)
                .map_err(|error| error.to_string())
        });
        let refusal = |reason, detail| Err(failure(reason, start.key, detail));
        request.launch = match unsandboxed::decide(&availability, &start.permit, start.mode) {
            unsandboxed::Launch::Sandboxed => WaitLaunch::Sandboxed(policy),
            unsandboxed::Launch::Unsandboxed { note } => {
                log::warn!(
                    "workflows: wait {} of run {} runs without the OS sandbox: {note}",
                    start.key,
                    start.run
                );
                WaitLaunch::Unsandboxed
            }
            unsandboxed::Launch::Ask { message } => {
                return refusal(FailureReason::SandboxRequired, message);
            }
            unsandboxed::Launch::Refuse { message } => {
                return refusal(FailureReason::SandboxRefused, message);
            }
        };
        Ok(())
    }

    /// The wait's sandbox policy: the scratch directory writable, the rest read-only, the
    /// worktree's git directories protected, and the network per condition. A `gh` condition
    /// reaches the task remote's hosts, a URL `pr`'s host and any declared ones, and is told its
    /// repository (`--repo`, set here); `command` and `deploy` reach the declared hosts only.
    fn policy(
        &self,
        start: &BuiltinStart<'_>,
        request: &mut WaitRequest,
    ) -> Result<SandboxPolicy, Failure> {
        let key = start.key;
        let binding = binding(start)?;
        let denied = |detail: &str| failure(FailureReason::PermissionDenied, key, detail);
        let declared = start
            .definition
            .node(&key.node)
            .and_then(|node| node.permissions.as_ref())
            .map(|permissions| permissions.network.clone())
            .unwrap_or_default();
        let wants_remote = declared.iter().any(|entry| entry == REMOTE_ENTRY);
        let gh = request.condition.uses_gh();
        let remote = (wants_remote || gh)
            .then(|| task_remote_url(&self.git, binding))
            .flatten();
        let mut hosts = if wants_remote {
            let remote = remote
                .as_deref()
                .and_then(remote_hosts)
                .ok_or_else(|| denied(NO_ORIGIN))?;
            expand_remote(&declared, &remote)
        } else {
            declared
        };
        if gh {
            let from_pr = builtin_wait::pr_url(&request.pr);
            let github = remote.as_deref().and_then(builtin_wait::github_repo);
            if github.is_some() {
                hosts.extend(["github.com".to_string(), "*.github.com".to_string()]);
            } else if let Some(remote) = remote.as_deref().and_then(remote_hosts) {
                hosts.extend(remote);
            }
            if let Some((host, _)) = &from_pr {
                hosts.extend([host.clone(), format!("*.{host}")]);
            }
            if hosts.is_empty() {
                return Err(denied(NO_GH_HOST));
            }
            let repo = from_pr.map(|(_, repo)| repo).or(github);
            request.repo = Some(repo.ok_or_else(|| denied(NO_GH_REPO))?);
            request.env = builtin_wait::gh_env(
                std::mem::take(&mut request.env),
                &request.pr,
                remote.as_deref(),
            );
        }
        let mut seen = HashSet::new();
        hosts.retain(|host| seen.insert(host.clone()));
        let mut git_dirs: Vec<PathBuf> = futures::executor::block_on(self.git.run(
            &request.worktree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        ))
        .map(|output| output.lines().map(PathBuf::from).collect())
        .unwrap_or_default();
        git_dirs.dedup();
        Ok(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![request.scratch.clone()],
            },
            network: if hosts.is_empty() {
                SandboxNetPolicy::Blocked
            } else {
                SandboxNetPolicy::Restricted {
                    allowed_domains: hosts,
                }
            },
            git: GitSandboxPolicy::Denied { git_dirs },
        })
    }
}

/// A shell-style variable name: letters, digits and `_`, not starting with a digit.
fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
        let (mut request, interval) = self.request(&start)?;
        self.confine(&start, &mut request)?;
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
