//! The staleness script of a prepared task's first job (pick rule 8). It is run from a pinned,
//! read-only copy of the definitions the job runs on (master's when it names none), in the lane's
//! tree after its reset, as `<script> <task id> <prepared commit> <tree path>`, with the lane's
//! environment and a timeout; its first output line, `stale` or `fresh`, decides. Anything else
//! is a failure, and the lane halts with "staleness script failed: <line or exit code>".

use std::any::Any;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use bongos_lanes_model::{EnvEdit, LaneId, TaskId};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
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

/// A read-only copy of a commit's files (`ProjectRepo::materialize`), kept while its guard lives.
pub struct PinnedTree {
    /// The root of the copy: `definitions/` is inside it.
    pub root: PathBuf,
    /// Removes the copy when dropped.
    pub guard: Box<dyn Any + Send>,
}

/// The program a staleness script is run as: `script`, a path under `definitions/`, in the pinned
/// copy rooted at `root`. A path that leaves `definitions/` is refused.
pub fn staleness_program(root: &Path, script: &str) -> Result<PathBuf, String> {
    let relative = Path::new(script);
    let inside = relative
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if script.is_empty() || !inside {
        return Err(format!("{script} is not a path inside {DEFINITIONS_DIR}/"));
    }
    Ok(root.join(DEFINITIONS_DIR).join(relative))
}

/// Runs `check`'s script from the pinned copy `pinned` of its definitions, in the lane's tree,
/// with the process environment and the lane's edits; a run past `timeout` is killed and fails.
/// The copy is kept until the script ended.
pub fn run_staleness_script(
    check: StalenessCheck,
    pinned: PinnedTree,
    timeout: Duration,
    cx: &App,
) -> Task<Result<Staleness, String>> {
    let Some(tree) = check.tree.clone() else {
        return Task::ready(Err("no lane tree to run it in".to_string()));
    };
    let program = match staleness_program(&pinned.root, &check.script) {
        Ok(program) => program,
        Err(error) => return Task::ready(Err(error)),
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
        let _pinned = pinned;
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
