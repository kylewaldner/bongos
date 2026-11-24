//! The shipped `wait` program node (feature 20, D10): holds a branch until something outside the
//! run happens. It runs `gh` and the declared command as the user, in the task worktree, and writes
//! nothing there. A program runner polls it (D9): [`WaitOutcome::Waiting`] is the "not yet" result
//! the runner reports and sleeps on, with no process and no cost, and the runner's give-up path
//! ends the node with "wait timed out". `elapsed` and `give_up` on the request model that same
//! bound here, so each condition has a [`WaitOutcome::TimedOut`] case.
//!
//! A hung `gh` or command is not bounded here: `wait` sets no timeout of its own, so the program
//! runner's `max_minutes` cap (runner_program.rs) stops it.
//!
//! Conditions (`with.condition`):
//! - `ci_green`: `gh pr checks <pr> --json bucket`. Every bucket `pass` or `skipping` is
//!   satisfied; any `fail` or `cancel` fails the wait at once with the reason (a red check will
//!   not turn green by waiting; a retry step reruns it); any `pending`, or no checks yet, is
//!   waiting. If the JSON cannot be read (an older gh), the exit code decides: 0 satisfied, 8
//!   waiting, 1 failed, anything else a failure naming the error.
//! - `pr_merged`: `gh pr view <pr> --json state`. `MERGED` is satisfied, `OPEN` (or `DRAFT`)
//!   waiting, `CLOSED` a failure.
//! - `command`: the declared argv; exit 0 is satisfied, a nonzero exit is waiting.
//! - `deploy`: satisfied at once when the task's `merge_is_ship` is on (D15); else the declared
//!   command as for `command`; with no command, a gate "Confirm deploy of <pr>?" opens.
//!
//! How a pass starts its process is [`WaitRequest::launch`], decided once when the wait starts
//! (crate::builtin_run). Under the `fallback` mode it runs as the user, as described above. Under
//! any other mode it runs inside the OS sandbox: the task worktree is the working directory and
//! read-only, git's directories are protected, the only writable place is a per-try scratch
//! directory (emptied before each pass) that is also `TMPDIR`, the network reaches only the
//! wait's hosts, and the environment is cleared down to [`WaitRequest::env`], `TMPDIR` and the
//! sandbox's own proxy variables. `gh` is told the repository with `--repo`, since it cannot ask
//! git. `gh`'s GitHub Enterprise tokens reach it only for the task remote's host or `GH_HOST`
//! ([`gh_env`]). The sandbox leaves the D-Bus session bus reachable, so a pass can read the user's
//! keyring (that is how `gh` finds its login). With no sandbox and a person's (or the setting's) word, the same cleared environment and
//! scratch directory are used without the sandbox. A pass whose sandbox cannot be set up fails;
//! it never runs outside the sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::LocalBoxFuture;
use sandbox::{CommandAndArgs, Sandbox, SandboxError, SandboxPolicy, WrappedCommand};

/// What the wait waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitCondition {
    CiGreen,
    PrMerged,
    Deploy,
    Command,
}

impl WaitCondition {
    /// Parses `ci_green`, `pr_merged`, `deploy` or `command`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ci_green" => Some(Self::CiGreen),
            "pr_merged" => Some(Self::PrMerged),
            "deploy" => Some(Self::Deploy),
            "command" => Some(Self::Command),
            _ => None,
        }
    }

    /// Whether the condition asks `gh`.
    pub fn uses_gh(self) -> bool {
        matches!(self, Self::CiGreen | Self::PrMerged)
    }
}

/// How each pass of a wait starts `gh` or the declared command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WaitLaunch {
    /// The `fallback` mode: as the user, with the writer's whole environment, unconfined.
    #[default]
    AsUser,
    /// Inside the OS sandbox with this policy, the environment cleared.
    Sandboxed(SandboxPolicy),
    /// No sandbox, by a person's or the setting's word: the environment cleared and `TMPDIR` the
    /// scratch directory, as in the sandbox.
    Unsandboxed,
}

/// One pass of a wait.
#[derive(Clone, Debug)]
pub struct WaitRequest {
    pub condition: WaitCondition,
    /// The pull request, as `gh` names one (a number, URL or branch).
    pub pr: String,
    /// The declared command, an argv list.
    pub command: Vec<String>,
    /// The task's `merge_is_ship`.
    pub merge_is_ship: bool,
    /// The directory commands run in: the task worktree.
    pub worktree: PathBuf,
    /// The GitHub CLI binary, `gh` unless a test says otherwise.
    pub gh: PathBuf,
    /// How long the wait has been waiting, and when it gives up; a still-waiting pass at or past
    /// the bound is [`WaitOutcome::TimedOut`].
    pub elapsed: Duration,
    pub give_up: Option<Duration>,
    /// How each pass starts its process.
    pub launch: WaitLaunch,
    /// The variables a confined pass gets, besides `TMPDIR` and the sandbox's proxy variables.
    pub env: BTreeMap<String, String>,
    /// The try's scratch directory: a confined pass's `TMPDIR` and only writable place.
    pub scratch: PathBuf,
    /// The repository `gh` is told with `--repo`, as `owner/name`; set for a confined pass.
    pub repo: Option<String>,
    /// Sets up a sandboxed pass's sandbox and wraps its command: [`wrap_in_sandbox`] unless a
    /// test makes it fail.
    pub sandbox_wrap: SandboxWrap,
}

/// Builds the sandbox for `policy` and wraps `command` in it, returning both: the sandbox owns
/// the network proxy, so it must live until the wrapped process has ended.
pub type SandboxWrap =
    fn(
        SandboxPolicy,
        CommandAndArgs,
    ) -> LocalBoxFuture<'static, Result<(Sandbox, WrappedCommand), SandboxError>>;

/// The real [`SandboxWrap`]: [`Sandbox::new`], then [`Sandbox::wrap`].
pub fn wrap_in_sandbox(
    policy: SandboxPolicy,
    command: CommandAndArgs,
) -> LocalBoxFuture<'static, Result<(Sandbox, WrappedCommand), SandboxError>> {
    async move {
        let mut sandbox = Sandbox::new(policy)?;
        let wrapped = sandbox.wrap(&command).await?;
        Ok((sandbox, wrapped))
    }
    .boxed_local()
}

impl WaitRequest {
    /// Whether a pass starts a process: a `deploy` that ships with the merge, or that opens a
    /// gate, runs nothing.
    pub fn runs_a_process(&self) -> bool {
        self.condition != WaitCondition::Deploy || (!self.merge_is_ship && !self.command.is_empty())
    }
}

/// The `result` value strings a pass reports: `waiting` is what the runner's `polls` setting
/// matches to sleep and poll again, `satisfied` ends the node successfully.
pub const WAITING: &str = "waiting";
pub const SATISFIED: &str = "satisfied";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Satisfied,
    Waiting,
    TimedOut(String),
    /// The wait cannot be satisfied; the node fails with this reason.
    Failed(String),
    /// Opens a gate with this question and waits on Kyle.
    OpenGate(String),
}

impl WaitOutcome {
    /// The node's `result` value: `satisfied` or `waiting`; other outcomes end the node.
    pub fn result_value(&self) -> Option<&'static str> {
        match self {
            Self::Satisfied => Some(SATISFIED),
            Self::Waiting => Some(WAITING),
            _ => None,
        }
    }
}

/// Runs one pass of the wait.
pub async fn wait(request: &WaitRequest) -> WaitOutcome {
    let outcome = match request.condition {
        WaitCondition::CiGreen => ci_green(request).await,
        WaitCondition::PrMerged => pr_merged(request).await,
        WaitCondition::Command => run_declared(request).await,
        WaitCondition::Deploy => {
            if request.merge_is_ship {
                WaitOutcome::Satisfied
            } else if request.command.is_empty() {
                WaitOutcome::OpenGate(format!("Confirm deploy of {}?", request.pr))
            } else {
                run_declared(request).await
            }
        }
    };
    match (outcome, request.give_up) {
        (WaitOutcome::Waiting, Some(give_up)) if request.elapsed >= give_up => {
            WaitOutcome::TimedOut("wait timed out".to_string())
        }
        (outcome, _) => outcome,
    }
}

fn check_pr(request: &WaitRequest) -> Result<(), WaitOutcome> {
    if request.pr.is_empty() || request.pr.starts_with('-') {
        return Err(WaitOutcome::Failed(format!(
            "`{}` is not a pull request",
            request.pr
        )));
    }
    Ok(())
}

/// Runs `binary` with `args` in the task worktree, as [`WaitRequest::launch`] says.
async fn run(request: &WaitRequest, binary: &Path, args: &[String]) -> Result<Output, String> {
    let could_not_run =
        |error: std::io::Error| format!("could not run {}: {error}", binary.display());
    match &request.launch {
        WaitLaunch::AsUser => util::command::new_command(binary)
            .current_dir(&request.worktree)
            .args(args)
            .output()
            .await
            .map_err(could_not_run),
        WaitLaunch::Unsandboxed => {
            let env = confined_env(request)?;
            run_cleared(binary.as_os_str(), args, &env, &request.worktree)
                .await
                .map_err(could_not_run)
        }
        WaitLaunch::Sandboxed(policy) => {
            let env = confined_env(request)?;
            let unavailable = |error: sandbox::SandboxError| {
                format!(
                    "could not set up the sandbox for {}: {error}",
                    binary.display()
                )
            };
            let command = CommandAndArgs {
                program: binary.to_string_lossy().into_owned(),
                args: args.to_vec(),
                env: env.into_iter().collect(),
                cwd: Some(request.worktree.clone()),
            };
            let (sandbox, wrapped) = (request.sandbox_wrap)(policy.clone(), command)
                .await
                .map_err(unavailable)?;
            // The sandbox owns the network proxy, so it lives until the process has ended. Its
            // environment is what the pass gets: the one passed in plus the proxy's variables.
            let output = run_cleared(
                wrapped.program.as_ref(),
                &wrapped.args,
                &wrapped.env,
                &request.worktree,
            )
            .await
            .map_err(could_not_run);
            sandbox.drop_on_current_thread();
            output
        }
    }
}

/// Empties the scratch directory for this pass, and returns the confined environment:
/// [`WaitRequest::env`] with `TMPDIR` the scratch directory.
fn confined_env(request: &WaitRequest) -> Result<BTreeMap<String, String>, String> {
    let scratch = &request.scratch;
    let failed =
        |error: std::io::Error| format!("could not prepare {}: {error}", scratch.display());
    match std::fs::remove_dir_all(scratch) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(failed(error)),
        _ => {}
    }
    std::fs::create_dir_all(scratch).map_err(failed)?;
    let mut env = request.env.clone();
    env.insert("TMPDIR".to_string(), scratch.to_string_lossy().into_owned());
    Ok(env)
}

/// Runs `program` with exactly `env`, nothing of the writer's own environment.
async fn run_cleared<K, V>(
    program: &std::ffi::OsStr,
    args: &[String],
    env: impl IntoIterator<Item = (K, V)>,
    directory: &Path,
) -> std::io::Result<Output>
where
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    util::command::new_command(program)
        .current_dir(directory)
        .args(args)
        .env_clear()
        .envs(env)
        .output()
        .await
}

/// `args` for `gh`, with `--repo` when the request names the repository.
fn gh_args(request: &WaitRequest, args: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    if let Some(repo) = &request.repo {
        args.extend(["--repo".to_string(), repo.clone()]);
    }
    args
}

/// `gh`'s tokens for GitHub Enterprise Server hosts: `gh` sends them to whatever host it is asked
/// about that is not github.com, so [`gh_env`] passes them only to a host the task already trusts.
pub const ENTERPRISE_TOKENS: &[&str] = &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"];

/// The environment a confined `gh` pass gets: `env` (the writer's variables picked for the wait)
/// without [`ENTERPRISE_TOKENS`] unless the host `gh` will talk to is the task remote's host or
/// `GH_HOST`. That host is a URL `pr`'s host; for a number or branch it is `GH_HOST`, else
/// github.com. A URL `pr` whose host cannot be read never gets the tokens.
pub fn gh_env(
    mut env: BTreeMap<String, String>,
    pr: &str,
    remote: Option<&str>,
) -> BTreeMap<String, String> {
    let gh_host_var = env
        .get("GH_HOST")
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty());
    let pr = pr.trim();
    let target = if pr.contains("://") {
        crate::dispatch_program::remote_hosts(pr).and_then(|hosts| hosts.into_iter().next())
    } else {
        Some(
            gh_host_var
                .clone()
                .unwrap_or_else(|| "github.com".to_string()),
        )
    };
    let remote_host = remote
        .and_then(crate::dispatch_program::remote_hosts)
        .and_then(|hosts| hosts.into_iter().next());
    let trusted = target.is_some_and(|target| {
        Some(&target) == remote_host.as_ref() || Some(&target) == gh_host_var.as_ref()
    });
    if !trusted {
        for name in ENTERPRISE_TOKENS {
            env.remove(*name);
        }
    }
    env
}

/// The host and `owner/name` of a pull request URL such as `https://github.com/o/r/pull/42`.
pub fn pr_url(pr: &str) -> Option<(String, String)> {
    let pr = pr.trim();
    let rest = pr
        .strip_prefix("https://")
        .or_else(|| pr.strip_prefix("http://"))?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    let host = host.to_ascii_lowercase();
    let path = path.split(['?', '#']).next()?.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        [owner, name, "pull", number, ..]
            if !host.is_empty()
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some((host, owner_name(&format!("{owner}/{name}"))?))
        }
        _ => None,
    }
}

/// The `owner/name` of a GitHub remote URL (`https://github.com/o/r.git`, `git@github.com:o/r`,
/// `ssh://git@ssh.github.com:443/o/r.git`); `None` for any other host or shape.
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let hosts = crate::dispatch_program::remote_hosts(url)?;
    if !matches!(
        hosts.first()?.as_str(),
        "github.com" | "www.github.com" | "ssh.github.com"
    ) {
        return None;
    }
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':')?.1,
    };
    owner_name(path.split(['?', '#']).next()?)
}

fn owner_name(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let part = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (part(owner) && part(name)).then(|| format!("{owner}/{name}"))
}

async fn run_declared(request: &WaitRequest) -> WaitOutcome {
    let Some((program, args)) = request.command.split_first() else {
        return WaitOutcome::Failed("the wait declares no command".to_string());
    };
    match run(request, Path::new(program), args).await {
        Ok(output) if output.status.success() => WaitOutcome::Satisfied,
        Ok(_) => WaitOutcome::Waiting,
        Err(message) => WaitOutcome::Failed(message),
    }
}

async fn ci_green(request: &WaitRequest) -> WaitOutcome {
    if let Err(outcome) = check_pr(request) {
        return outcome;
    }
    let args = gh_args(request, &["pr", "checks", &request.pr, "--json", "bucket"]);
    let output = match run(request, &request.gh, &args).await {
        Ok(output) => output,
        Err(message) => return WaitOutcome::Failed(message),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(serde_json::Value::Array(checks)) = serde_json::from_str(&stdout) {
        let buckets: Vec<&str> = checks
            .iter()
            .map(|check| check["bucket"].as_str().unwrap_or("pending"))
            .collect();
        return if buckets.iter().any(|b| matches!(*b, "fail" | "cancel")) {
            WaitOutcome::Failed(format!("checks failed on {}", request.pr))
        } else if buckets.is_empty() || buckets.contains(&"pending") {
            WaitOutcome::Waiting
        } else {
            WaitOutcome::Satisfied
        };
    }
    // With no checks yet, gh prints "no checks reported" and exits 1: CI has not started.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.to_lowercase().contains("no checks reported")
        || stdout.to_lowercase().contains("no checks reported")
    {
        return WaitOutcome::Waiting;
    }
    match output.status.code() {
        Some(0) => WaitOutcome::Satisfied,
        Some(8) => WaitOutcome::Waiting,
        Some(1) => WaitOutcome::Failed(format!("checks failed on {}", request.pr)),
        _ => WaitOutcome::Failed(format!("gh pr checks failed: {}", stderr.trim())),
    }
}

async fn pr_merged(request: &WaitRequest) -> WaitOutcome {
    if let Err(outcome) = check_pr(request) {
        return outcome;
    }
    let args = gh_args(request, &["pr", "view", &request.pr, "--json", "state"]);
    let output = match run(request, &request.gh, &args).await {
        Ok(output) => output,
        Err(message) => return WaitOutcome::Failed(message),
    };
    let state = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|value| value["state"].as_str().map(str::to_string));
    match state.as_deref() {
        Some("MERGED") => WaitOutcome::Satisfied,
        Some("OPEN" | "DRAFT") => WaitOutcome::Waiting,
        Some("CLOSED") => WaitOutcome::Failed(format!("{} was closed without merging", request.pr)),
        _ => WaitOutcome::Failed(format!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}
