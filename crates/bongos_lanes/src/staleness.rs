//! The staleness script of a prepared task's first job (pick rule 8). It runs in the lane's tree
//! after its reset, as `<script> <task id> <prepared commit> <tree path>`, with the lane's
//! environment and a timeout; its first output line, `stale` or `fresh`, decides. Anything else
//! is a failure, and the lane halts with "staleness script failed: <line or exit code>".

use std::path::PathBuf;
use std::time::Duration;

use bongos_lanes_model::{EnvEdit, LaneId, TaskId};
use futures::FutureExt as _;
use gpui::{App, Task};

use crate::env::apply_env;

/// What the staleness script of a job is run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalenessCheck {
    pub lane: LaneId,
    /// The prepared top-level task.
    pub task: TaskId,
    /// The commit its preparation left; empty on the command line when unknown.
    pub prepared_commit: Option<String>,
    /// The rule's `staleness`, a project path.
    pub script: String,
    /// The definitions commit the job runs on.
    pub definition_commit: Option<String>,
    /// The lane's tree, after its reset; `None` for a job in a fresh run worktree.
    pub tree: Option<PathBuf>,
    /// The lane's environment edits.
    pub env: Vec<EnvEdit>,
}

/// What a staleness script said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    /// The preparation is out of date: the rule's refresh workflow runs first.
    Stale,
    Fresh,
}

/// The answer of a script that exited with `code` (`None` when killed by a signal) and printed
/// `stdout`: its first line decides, else the failure text names that line or the exit code.
pub fn staleness_answer(stdout: &str, code: Option<i32>) -> Result<Staleness, String> {
    let first = stdout.lines().next().unwrap_or("").trim();
    match first {
        "stale" => Ok(Staleness::Stale),
        "fresh" => Ok(Staleness::Fresh),
        "" => Err(match code {
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_string(),
        }),
        line => Err(line.to_string()),
    }
}

/// Runs `check`'s script, resolved against the lane's tree when relative, in that tree, with the
/// process environment and the lane's edits; a run past `timeout` is killed and fails.
pub fn run_staleness_script(
    check: StalenessCheck,
    timeout: Duration,
    cx: &App,
) -> Task<Result<Staleness, String>> {
    let Some(tree) = check.tree.clone() else {
        return Task::ready(Err("no lane tree to run it in".to_string()));
    };
    let script = PathBuf::from(&check.script);
    let program = if script.is_absolute() {
        script
    } else {
        tree.join(script)
    };
    let env = apply_env(std::env::vars(), &check.env);
    let mut command = util::command::new_command(&program);
    command
        .arg(&check.task.0)
        .arg(check.prepared_commit.as_deref().unwrap_or(""))
        .arg(&tree)
        .current_dir(&tree)
        .env_clear()
        .envs(env)
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    let timer = cx.background_executor().timer(timeout);
    cx.background_executor().spawn(async move {
        let output = command.output().fuse();
        let timer = timer.fuse();
        futures::pin_mut!(output, timer);
        futures::select_biased! {
            output = output => {
                let output = output
                    .map_err(|error| format!("{} cannot run: {error}", program.display()))?;
                staleness_answer(&String::from_utf8_lossy(&output.stdout), output.status.code())
            }
            _ = timer => Err(format!("timed out after {} seconds", timeout.as_secs())),
        }
    })
}
