//! The shipped `wait` program node (feature 20, D10): holds a branch until something outside the
//! run happens. It runs `gh` and the declared command as the user, in the task worktree, and writes
//! nothing there. A program runner polls it (D9): [`WaitOutcome::Waiting`] is the "not yet" result
//! the runner reports and sleeps on, with no process and no cost, and the runner's give-up path
//! ends the node with "wait timed out". `elapsed` and `give_up` on the request model that same
//! bound here, so each condition has a [`WaitOutcome::TimedOut`] case.
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

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

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
}

/// The `polls` value of the node's `result` field while the wait is not yet over.
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

async fn run(binary: &Path, args: &[String], directory: &Path) -> Result<Output, String> {
    util::command::new_command(binary)
        .current_dir(directory)
        .args(args)
        .output()
        .await
        .map_err(|error| format!("could not run {}: {error}", binary.display()))
}

async fn run_declared(request: &WaitRequest) -> WaitOutcome {
    let Some((program, args)) = request.command.split_first() else {
        return WaitOutcome::Failed("the wait declares no command".to_string());
    };
    match run(Path::new(program), args, &request.worktree).await {
        Ok(output) if output.status.success() => WaitOutcome::Satisfied,
        Ok(_) => WaitOutcome::Waiting,
        Err(message) => WaitOutcome::Failed(message),
    }
}

async fn ci_green(request: &WaitRequest) -> WaitOutcome {
    if let Err(outcome) = check_pr(request) {
        return outcome;
    }
    let args = ["pr", "checks", &request.pr, "--json", "bucket"].map(String::from);
    let output = match run(&request.gh, &args, &request.worktree).await {
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
    match output.status.code() {
        Some(0) => WaitOutcome::Satisfied,
        Some(8) => WaitOutcome::Waiting,
        Some(1) => WaitOutcome::Failed(format!("checks failed on {}", request.pr)),
        _ => WaitOutcome::Failed(format!(
            "gh pr checks failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

async fn pr_merged(request: &WaitRequest) -> WaitOutcome {
    if let Err(outcome) = check_pr(request) {
        return outcome;
    }
    let args = ["pr", "view", &request.pr, "--json", "state"].map(String::from);
    let output = match run(&request.gh, &args, &request.worktree).await {
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
