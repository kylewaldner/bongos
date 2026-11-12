//! Run and branch worktrees (feature 20, D11). A run works in its own worktree on its own branch,
//! made from the head of the task branch; fan-out branches get theirs from the run branch. Nothing
//! here writes the task worktree or the user's checkout: git runs there only to read, and to add
//! worktrees and branches. Only [`crate::builtin_merge`] writes the task branch.
//!
//! Git runs as a subprocess through `util::command::new_command`; the `git` crate has no merge API.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use bongos_work_model::TaskFile;
use bongos_workflow_model::record::{EndState, Lifecycle};

use crate::WorkflowsSettings;

/// The refusal for a task that has no worktree or branch yet.
pub const NOT_STARTED: &str = "start the task first (task: start)";

/// The directory, under the repository, that holds run worktrees when `workflows.worktrees_dir`
/// is null. It sits inside `.bongos-worktrees`, which task worktrees already use, and the leading
/// dot cannot collide with a task slug.
pub const DEFAULT_RUNS_DIR: &str = ".bongos-worktrees/.runs";

const MIB: u64 = 1024 * 1024;

/// Runs git. Production uses [`Git::new`] with the user's git; tests add an isolated
/// environment with [`Git::env`].
#[derive(Clone, Debug)]
pub struct Git {
    binary: PathBuf,
    envs: Vec<(OsString, OsString)>,
}

impl Git {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            envs: Vec::new(),
        }
    }

    /// Adds an environment variable to every git this runs.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Runs git with `args` in `directory` and returns its whole output, whatever the exit status.
    /// Replace refs are ignored (`GIT_NO_REPLACE_OBJECTS`): a program node may write `refs/`.
    pub(crate) async fn output(&self, directory: &Path, args: &[&str]) -> Result<Output, GitError> {
        util::command::new_command(&self.binary)
            .current_dir(directory)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args(args.iter().map(OsString::from))
            .envs(self.envs.iter().map(|(key, value)| (key, value)))
            .output()
            .await
            .map_err(|error| GitError {
                args: args.join(" "),
                message: error.to_string(),
            })
    }

    /// Runs git with `args` in `directory` and returns its trimmed output, or its error output as
    /// the error.
    pub(crate) async fn run(&self, directory: &Path, args: &[&str]) -> Result<String, GitError> {
        let output = self.output(directory, args).await?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(GitError {
                args: args.join(" "),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    /// Whether `directory`'s worktree has uncommitted changes, untracked files included.
    pub(crate) async fn is_dirty(&self, directory: &Path) -> Result<bool, GitError> {
        let status = self
            .run(
                directory,
                &["status", "--porcelain", "--untracked-files=normal"],
            )
            .await?;
        Ok(!status.is_empty())
    }

    /// Merges `args` (a `git merge` command line after `merge`) in `directory`. On a conflict it
    /// lists the unmerged files, runs `git merge --abort` and returns them; every branch stays as
    /// it was before the merge.
    pub(crate) async fn merge_or_abort(
        &self,
        directory: &Path,
        args: &[&str],
    ) -> Result<(), MergeError> {
        let mut command = vec!["merge"];
        command.extend_from_slice(args);
        let output = self.output(directory, &command).await?;
        if output.status.success() {
            return Ok(());
        }
        let unmerged = self
            .run(directory, &["diff", "--name-only", "--diff-filter=U"])
            .await
            .unwrap_or_default();
        let files: Vec<String> = unmerged
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        // Abort whenever a merge is in progress, conflict or not, so nothing is left half done.
        let in_progress = self
            .run(directory, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .await
            .is_ok();
        if in_progress || !files.is_empty() {
            self.run(directory, &["merge", "--abort"]).await?;
        }
        if files.is_empty() {
            Err(MergeError::Git(GitError {
                args: command.join(" "),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }))
        } else {
            Err(MergeError::Conflict(files))
        }
    }
}

/// A git command that failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("git {args} failed: {message}")]
pub struct GitError {
    pub args: String,
    pub message: String,
}

/// Why a merge did not happen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    /// The merge conflicted on these files and was aborted.
    #[error("merge conflict in {}", .0.join(", "))]
    Conflict(Vec<String>),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Why a run or branch worktree was not created.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    /// The task has no worktree or branch.
    #[error("{NOT_STARTED}")]
    NotStarted,
    /// The disk holding the run worktrees has less free space than the setting asks.
    #[error(
        "only {free_mb} MB free on the disk holding {}; workflows.min_free_disk_mb asks for {min_mb} MB",
        .location.display()
    )]
    LowDiskSpace {
        location: PathBuf,
        free_mb: u64,
        min_mb: u64,
    },
    #[error("creating {}: {message}", .path.display())]
    Io { path: PathBuf, message: String },
    #[error(transparent)]
    Git(#[from] GitError),
}

/// What a run worktree is made from: the task's slug, worktree and branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskCheckout {
    pub slug: String,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
}

impl TaskCheckout {
    pub fn from_task(slug: impl Into<String>, task: &TaskFile) -> Self {
        Self {
            slug: slug.into(),
            worktree: task.worktree.as_ref().map(PathBuf::from),
            branch: task.branch.clone(),
        }
    }
}

/// A worktree made for a run or one of its fan-out branches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunWorktree {
    /// The directory of the run attempt, `<dir>/<task slug>-<short>-<attempt>`.
    pub run_dir: PathBuf,
    pub path: PathBuf,
    pub branch: String,
}

/// The last 8 hex digits of a run id.
pub fn short_id(run_id: &str) -> String {
    let hex: Vec<char> = run_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|digit| digit.to_ascii_lowercase())
        .collect();
    hex[hex.len().saturating_sub(8)..].iter().collect()
}

/// `<task branch>-run-<short>-<attempt>`.
pub fn run_branch_name(task_branch: &str, run_id: &str, attempt: u32) -> String {
    format!("{task_branch}-run-{}-{attempt}", short_id(run_id))
}

/// `<run branch>-<node>-<index>`: flat, because a ref `a/b` cannot coexist with `a/b/c`.
pub fn fan_out_branch_name(run_branch: &str, node: &str, index: u32) -> String {
    format!("{run_branch}-{node}-{index}")
}

/// The directory holding run worktrees: `workflows.worktrees_dir`, else
/// `<repo>/.bongos-worktrees/.runs`. `~` expands to the home directory and a relative path is
/// resolved against `repository`.
pub fn runs_dir(repository: &Path, worktrees_dir: Option<&Path>) -> PathBuf {
    match worktrees_dir {
        Some(dir) => {
            // A leading `~` is the home directory; a relative path is relative to the repository.
            let expanded = match dir.strip_prefix("~") {
                Ok(rest) => paths::home_dir().join(rest),
                Err(_) => dir.to_path_buf(),
            };
            repository.join(expanded)
        }
        None => repository.join(DEFAULT_RUNS_DIR),
    }
}

/// `<runs dir>/<task slug>-<short>-<attempt>`.
pub fn run_dir(runs_dir: &Path, task_slug: &str, run_id: &str, attempt: u32) -> PathBuf {
    runs_dir.join(format!("{task_slug}-{}-{attempt}", short_id(run_id)))
}

/// `<run dir>/run`.
pub fn run_worktree_path(run_dir: &Path) -> PathBuf {
    run_dir.join("run")
}

/// `<run dir>/b-<node>-<index>`.
pub fn branch_worktree_path(run_dir: &Path, node: &str, index: u32) -> PathBuf {
    run_dir.join(format!("b-{node}-{index}"))
}

/// Free bytes on the disk holding `path`, from `sysinfo`: the mounted disk with the longest mount
/// point that holds the nearest existing ancestor of `path`. `None` when no disk matches.
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let existing = existing.canonicalize().ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| existing.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

/// Refuses when the disk holding `location` has less than `min_free_disk_mb` free. `free_space`
/// returns free bytes ([`disk_free_bytes`] in the app); when it cannot tell, the check passes.
pub fn check_free_space(
    location: &Path,
    min_free_disk_mb: u64,
    free_space: &dyn Fn(&Path) -> Option<u64>,
) -> Result<(), WorktreeError> {
    match free_space(location) {
        Some(free) if free < min_free_disk_mb.saturating_mul(MIB) => {
            Err(WorktreeError::LowDiskSpace {
                location: location.to_path_buf(),
                free_mb: free / MIB,
                min_mb: min_free_disk_mb,
            })
        }
        Some(_) => Ok(()),
        None => {
            log::warn!(
                "cannot tell the free space on the disk holding {}",
                location.display()
            );
            Ok(())
        }
    }
}

/// The main checkout of the repository `worktree` belongs to.
pub async fn repository_root(git: &Git, worktree: &Path) -> Result<PathBuf, GitError> {
    let common = git
        .run(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
    let common = PathBuf::from(common);
    Ok(match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf).unwrap_or(common),
        _ => common,
    })
}

/// Creates the run worktree of attempt `attempt` of run `run_id`: a new branch
/// ([`run_branch_name`]) from the head of the task branch, checked out at
/// `<runs dir>/<task slug>-<short>-<attempt>/run`. Refuses a task with no worktree, and a disk
/// with less than `workflows.min_free_disk_mb` free. The task worktree is only read.
pub async fn create_run(
    git: &Git,
    task: &TaskCheckout,
    run_id: &str,
    attempt: u32,
    settings: &WorkflowsSettings,
    free_space: &dyn Fn(&Path) -> Option<u64>,
) -> Result<RunWorktree, WorktreeError> {
    let (Some(task_worktree), Some(task_branch)) = (&task.worktree, &task.branch) else {
        return Err(WorktreeError::NotStarted);
    };
    if !task_worktree.is_dir() {
        return Err(WorktreeError::NotStarted);
    }
    let repository = repository_root(git, task_worktree).await?;
    let runs = runs_dir(&repository, settings.worktrees_dir.as_deref());
    check_free_space(&runs, settings.min_free_disk_mb, free_space)?;
    let head = git
        .run(
            task_worktree,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{task_branch}^{{commit}}"),
            ],
        )
        .await?;
    let run_dir = run_dir(&runs, &task.slug, run_id, attempt);
    let worktree = RunWorktree {
        path: run_worktree_path(&run_dir),
        branch: run_branch_name(task_branch, run_id, attempt),
        run_dir,
    };
    add_worktree(git, task_worktree, &worktree, &head).await?;
    Ok(worktree)
}

/// Creates the worktree of fan-out branch `index` of node `node` from the head of the run branch,
/// at `<run dir>/b-<node>-<index>`.
pub async fn create_branch(
    git: &Git,
    run: &RunWorktree,
    node: &str,
    index: u32,
) -> Result<RunWorktree, WorktreeError> {
    let head = git
        .run(
            &run.path,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}^{{commit}}", run.branch),
            ],
        )
        .await?;
    let worktree = RunWorktree {
        run_dir: run.run_dir.clone(),
        path: branch_worktree_path(&run.run_dir, node, index),
        branch: fan_out_branch_name(&run.branch, node, index),
    };
    add_worktree(git, &run.path, &worktree, &head).await?;
    Ok(worktree)
}

async fn add_worktree(
    git: &Git,
    from: &Path,
    worktree: &RunWorktree,
    head: &str,
) -> Result<(), WorktreeError> {
    std::fs::create_dir_all(&worktree.run_dir).map_err(|error| WorktreeError::Io {
        path: worktree.run_dir.clone(),
        message: error.to_string(),
    })?;
    let path = worktree.path.to_string_lossy();
    git.run(
        from,
        &[
            "worktree",
            "add",
            "-b",
            &worktree.branch,
            "--end-of-options",
            &path,
            head,
        ],
    )
    .await?;
    Ok(())
}

/// What a join did: the branches it merged, in branch order, and the conflict that stopped it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinOutcome {
    pub merged: Vec<u32>,
    /// The branch whose merge conflicted, and the files. Earlier branches stay merged; this one and
    /// the later ones are not, and every branch is left as it was.
    pub conflict: Option<(u32, Vec<String>)>,
}

/// The join of fan-out node `node`: merges each finished branch into the run branch, in the run
/// worktree, with `git merge --no-ff -m "Join <node> branch <i>"`, in branch order whatever order
/// `finished` lists them in, so the result does not depend on finish order. A conflict aborts that
/// merge and stops the join.
pub async fn join(
    git: &Git,
    run: &RunWorktree,
    node: &str,
    finished: &[u32],
) -> Result<JoinOutcome, GitError> {
    let mut order = finished.to_vec();
    order.sort_unstable();
    order.dedup();
    let mut outcome = JoinOutcome::default();
    for index in order {
        let branch = fan_out_branch_name(&run.branch, node, index);
        let message = format!("Join {node} branch {index}");
        match git
            .merge_or_abort(
                &run.path,
                &["--no-ff", "-m", &message, "--end-of-options", &branch],
            )
            .await
        {
            Ok(()) => outcome.merged.push(index),
            Err(MergeError::Conflict(files)) => {
                outcome.conflict = Some((index, files));
                break;
            }
            Err(MergeError::Git(error)) => return Err(error),
        }
    }
    Ok(outcome)
}

/// A finished run attempt that `workflow: clear finished` may remove.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedRun {
    pub run_id: String,
    pub lifecycle: Lifecycle,
    /// When the attempt ended or was cancelled.
    pub finished: SystemTime,
    /// The attempt's run worktree; its fan-out worktrees are found beside it.
    pub worktree: RunWorktree,
}

/// One worktree and branch to remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearItem {
    pub path: PathBuf,
    pub branch: String,
    /// Why the item is kept, for a skipped item.
    pub reason: Option<String>,
    /// For a fan-out branch, the commits on it that are not on the run branch, when known.
    pub unmerged: Option<u32>,
}

/// The worktrees of one run that `clear finished` would remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearRun {
    pub run_id: String,
    pub run_dir: PathBuf,
    /// Fan-out worktrees first, then the run worktree, so the run branch goes last.
    pub remove: Vec<ClearItem>,
    /// Worktrees with uncommitted changes, which are kept.
    pub skipped: Vec<ClearItem>,
}

/// What `clear finished` would do, listed for the confirmation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearPlan {
    pub runs: Vec<ClearRun>,
}

impl ClearPlan {
    /// The confirmation text: what is removed and what is skipped.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        for run in &self.runs {
            for item in &run.remove {
                let unmerged = match item.unmerged {
                    Some(count) if count > 0 => format!(", {count} unmerged commits"),
                    _ => String::new(),
                };
                lines.push(format!(
                    "remove {} ({}{unmerged})",
                    item.path.display(),
                    item.branch
                ));
            }
            for item in &run.skipped {
                lines.push(format!(
                    "skip {} ({}): {}",
                    item.path.display(),
                    item.branch,
                    item.reason.as_deref().unwrap_or("uncommitted changes")
                ));
            }
        }
        lines.join("\n")
    }

    pub fn skipped(&self) -> impl Iterator<Item = &ClearItem> {
        self.runs.iter().flat_map(|run| run.skipped.iter())
    }
}

/// Plans `workflow: clear finished`: the succeeded and cancelled runs that finished more than
/// `keep_finished_days` before `now`, with each of their worktrees to remove, and those with
/// uncommitted changes skipped. Writes nothing.
pub async fn plan_clear(
    git: &Git,
    runs: &[FinishedRun],
    keep_finished_days: u64,
    now: SystemTime,
) -> Result<ClearPlan, GitError> {
    let keep = Duration::from_secs(keep_finished_days.saturating_mul(24 * 60 * 60));
    let mut plan = ClearPlan::default();
    for run in runs {
        let finished = matches!(
            run.lifecycle,
            Lifecycle::Ended(EndState::Succeeded) | Lifecycle::Cancelled
        );
        let old_enough = now.duration_since(run.finished).is_ok_and(|age| age > keep);
        if !finished || !old_enough {
            continue;
        }
        let mut clear = ClearRun {
            run_id: run.run_id.clone(),
            run_dir: run.worktree.run_dir.clone(),
            remove: Vec::new(),
            skipped: Vec::new(),
        };
        for mut item in run_worktrees(git, &run.worktree).await {
            if item.reason.is_none() && item.path.is_dir() {
                match git.is_dirty(&item.path).await {
                    Ok(false) => {}
                    Ok(true) => item.reason = Some("uncommitted changes".to_string()),
                    Err(error) => item.reason = Some(format!("git failed: {}", error.message)),
                }
            }
            if item.reason.is_some() {
                clear.skipped.push(item);
            } else {
                clear.remove.push(item);
            }
        }
        plan.runs.push(clear);
    }
    Ok(plan)
}

/// The run worktree and its fan-out worktrees (`b-*` beside it), with their branches.
async fn run_worktrees(git: &Git, run: &RunWorktree) -> Vec<ClearItem> {
    let mut items = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&run.run_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with("b-"))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let branch = format!(
            "{}-{}",
            run.branch,
            name.strip_prefix("b-").unwrap_or(&name)
        );
        // Only a worktree still on its own fan-out branch is removed with that branch.
        let reason = match git.run(&path, &["symbolic-ref", "--short", "HEAD"]).await {
            Ok(head) if head == branch => None,
            Ok(head) => Some(format!("on branch {head}, not {branch}")),
            Err(_) => Some("HEAD is detached or unreadable".to_string()),
        };
        let unmerged = if reason.is_none() {
            let range = format!("{}..{branch}", run.branch);
            git.run(&path, &["rev-list", "--count", "--end-of-options", &range])
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        items.push(ClearItem {
            path,
            branch,
            reason,
            unmerged,
        });
    }
    items.push(ClearItem {
        path: run.path.clone(),
        branch: run.branch.clone(),
        reason: None,
        unmerged: None,
    });
    items
}

/// What `clear finished` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearReport {
    pub removed: Vec<ClearItem>,
    /// Kept: uncommitted changes, or git refused, with the reason.
    pub skipped: Vec<(ClearItem, String)>,
}

impl fmt::Display for ClearReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "removed {} worktrees", self.removed.len())?;
        for (item, reason) in &self.skipped {
            write!(f, "\nskipped {}: {reason}", item.path.display())?;
        }
        Ok(())
    }
}

/// Applies a confirmed [`ClearPlan`]: `git worktree remove` (never forced) and `git branch -D`
/// for each worktree, from `repository`, then the run directory once all its worktrees are gone.
pub async fn apply_clear(git: &Git, repository: &Path, plan: &ClearPlan) -> ClearReport {
    let mut report = ClearReport::default();
    for run in &plan.runs {
        let mut all_removed = run.skipped.is_empty();
        for item in &run.skipped {
            report.skipped.push((
                item.clone(),
                item.reason
                    .clone()
                    .unwrap_or_else(|| "uncommitted changes".to_string()),
            ));
        }
        for item in &run.remove {
            if item.path.exists() {
                let path = item.path.to_string_lossy();
                if let Err(error) = git
                    .run(
                        repository,
                        &["worktree", "remove", "--end-of-options", &path],
                    )
                    .await
                {
                    all_removed = false;
                    report.skipped.push((item.clone(), error.message));
                    continue;
                }
            }
            if let Err(error) = git
                .run(
                    repository,
                    &["branch", "-D", "--end-of-options", &item.branch],
                )
                .await
            {
                log::warn!("removing branch {}: {error}", item.branch);
            }
            report.removed.push(item.clone());
        }
        if all_removed
            && run.run_dir.exists()
            && let Err(error) = std::fs::remove_dir_all(&run.run_dir)
        {
            log::warn!("removing {}: {error}", run.run_dir.display());
        }
    }
    report
}
