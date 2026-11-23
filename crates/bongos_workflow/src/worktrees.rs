//! Run and branch worktrees (feature 20, D11). A run works in its own worktree on its own branch,
//! made from the head of the task branch; fan-out branches get theirs from the run branch. Nothing
//! here writes the task worktree or the user's checkout: git runs there only to read, and to add
//! worktrees and branches. Only [`crate::builtin_merge`] writes the task branch.
//!
//! Git runs as a subprocess through `util::command::new_command`; the `git` crate has no merge API.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};
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
    pub async fn output(&self, directory: &Path, args: &[&str]) -> Result<Output, GitError> {
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
    pub async fn run(&self, directory: &Path, args: &[&str]) -> Result<String, GitError> {
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
    pub async fn is_dirty(&self, directory: &Path) -> Result<bool, GitError> {
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
    runs_dir_with_home(repository, worktrees_dir, paths::home_dir())
}

/// [`runs_dir`] with the home directory given, so `clear finished` and its tests choose it.
fn runs_dir_with_home(repository: &Path, worktrees_dir: Option<&Path>, home: &Path) -> PathBuf {
    match worktrees_dir {
        Some(dir) => {
            let dir = dir.strip_prefix(".").unwrap_or(dir);
            match dir.strip_prefix("~") {
                Ok(rest) => home.join(rest),
                Err(_) => repository.join(dir),
            }
        }
        None => repository.join(DEFAULT_RUNS_DIR),
    }
}

/// The directory under a run directory that holds the scratch directories of program nodes:
/// `scratch/<node>/<branch>/<pass>-<try>`.
pub const SCRATCH_DIR: &str = "scratch";

/// The directory in a node's scratch directory that `TMPDIR` points at.
pub const SCRATCH_TMP_DIR: &str = "tmp";

/// Refuses a runs directory that `clear finished` must never remove from: a relative one, one with
/// a `..`, the filesystem root, or the home directory or the repository or any directory above
/// either (an empty `workflows.worktrees_dir`, `.` and `./` resolve to the repository).
fn refuse_runs_dir(runs: &Path, repository: &Path, home: &Path) -> Result<(), String> {
    let refused = !runs.is_absolute()
        || runs
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        || runs.parent().is_none()
        || home.starts_with(runs)
        || repository.starts_with(runs);
    if refused {
        Err(format!(
            "the runs directory {} is the filesystem root, the home directory, the repository or \
             above one of them; check workflows.worktrees_dir",
            runs.display()
        ))
    } else {
        Ok(())
    }
}

/// `path` with every symlink resolved, or `path` itself when it does not exist.
fn canonical_or_same(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Checks that `recorded`, as a run.toml recorded it, is exactly the run directory [`create_run`]
/// made for attempt `attempt` of run `run_id` of task `task_slug`: `run_dir(runs_dir(repository,
/// worktrees_dir), ...)`. `repository` is the main checkout of the task's own worktree, never one
/// derived from the record. The runs directory is refused when it is the filesystem root, the home
/// directory, the repository or above one of them, both as configured and with symlinks resolved;
/// the run directory may not be a symlink, and resolved it must sit directly in the resolved runs
/// directory, so neither a `..` nor a symlink leads elsewhere.
pub fn check_run_dir(
    recorded: &RunWorktree,
    repository: &Path,
    worktrees_dir: Option<&Path>,
    home: &Path,
    task_slug: &str,
    run_id: &str,
    attempt: u32,
) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "{} is not the run directory this app made: {why}",
            recorded.run_dir.display()
        ))
    };
    let runs = runs_dir_with_home(repository, worktrees_dir, home);
    refuse_runs_dir(&runs, repository, home)?;
    let expected = run_dir(&runs, task_slug, run_id, attempt);
    let plain = recorded.run_dir.components().all(|component| {
        matches!(
            component,
            Component::RootDir | Component::Prefix(_) | Component::Normal(_)
        )
    });
    if !plain || !recorded.run_dir.is_absolute() {
        return refuse("it has a `.` or `..` or is relative");
    }
    if recorded.run_dir != expected {
        return refuse(&format!("expected {}", expected.display()));
    }
    if recorded.path != run_worktree_path(&recorded.run_dir) {
        return refuse("its run worktree is not in it");
    }
    let Ok(metadata) = std::fs::symlink_metadata(&recorded.run_dir) else {
        // Nothing on disk to remove.
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return refuse("it is a symlink or not a directory");
    }
    let canonical_runs = runs
        .canonicalize()
        .map_err(|error| format!("{}: {error}", runs.display()))?;
    refuse_runs_dir(
        &canonical_runs,
        &canonical_or_same(repository),
        &canonical_or_same(home),
    )?;
    let canonical = recorded
        .run_dir
        .canonicalize()
        .map_err(|error| format!("{}: {error}", recorded.run_dir.display()))?;
    match expected.file_name() {
        Some(name) if canonical == canonical_runs.join(name) => Ok(()),
        _ => refuse("with symlinks resolved it is outside the runs directory"),
    }
}

/// Checks that `recorded`'s branch, as a run.toml recorded it, is exactly the run branch
/// [`create_run`] made for attempt `attempt` of run `run_id` from the task branch `task_branch`
/// ([`run_branch_name`]), so `clear finished` never deletes a branch the app did not make.
pub fn check_run_branch(
    recorded: &RunWorktree,
    task_branch: Option<&str>,
    run_id: &str,
    attempt: u32,
) -> Result<(), String> {
    let Some(task_branch) = task_branch else {
        return Err(format!(
            "the task has no branch, so {} is not known to be the run branch this app made",
            recorded.branch
        ));
    };
    let expected = run_branch_name(task_branch, run_id, attempt);
    if recorded.branch == expected {
        Ok(())
    } else {
        Err(format!(
            "branch {} is not the run branch this app made: expected {expected}",
            recorded.branch
        ))
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

/// The branch checked out in `dir`; `None` for a detached HEAD or when git cannot tell.
pub async fn checked_out_branch(git: &Git, dir: &Path) -> Option<String> {
    git.run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .filter(|branch| !branch.is_empty())
}

/// The worktree of the repository holding `dir`, other than `own`, that has `branch` checked
/// out (a person's checkout or the main checkout included), from `git worktree list`.
pub async fn worktree_holding(
    git: &Git,
    dir: &Path,
    branch: &str,
    own: &Path,
) -> Result<Option<PathBuf>, GitError> {
    let listing = git.run(dir, &["worktree", "list", "--porcelain"]).await?;
    let wanted = format!("refs/heads/{branch}");
    let own = canonical_or_same(own);
    let mut path: Option<PathBuf> = None;
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if line.strip_prefix("branch ") == Some(wanted.as_str())
            && let Some(path) = &path
            && canonical_or_same(path) != own
        {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
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

/// The run worktree of attempt `attempt` of run `run_id` in a lane's tree at `path`, which the
/// lane already reset onto the run branch ([`run_branch_name`] of the task branch): the tree is
/// used as it is, and the run's own directory (its fan-out worktrees and scratch) is made where a
/// fresh run's would be. Refuses a task with no branch, a tree that is missing or has another
/// branch checked out, and a disk with less than `workflows.min_free_disk_mb` free.
pub async fn lane_run(
    git: &Git,
    task: &TaskCheckout,
    run_id: &str,
    attempt: u32,
    path: &Path,
    settings: &WorkflowsSettings,
    free_space: &dyn Fn(&Path) -> Option<u64>,
) -> Result<RunWorktree, WorktreeError> {
    let Some(task_branch) = &task.branch else {
        return Err(WorktreeError::NotStarted);
    };
    let refused = |message: String| WorktreeError::Io {
        path: path.to_path_buf(),
        message,
    };
    if !path.is_dir() {
        return Err(refused("the lane's tree is missing".to_string()));
    }
    let branch = run_branch_name(task_branch, run_id, attempt);
    match checked_out_branch(git, path).await {
        Some(checked_out) if checked_out == branch => {}
        Some(checked_out) => {
            return Err(refused(format!(
                "the lane's tree has {checked_out} checked out, not the run branch {branch}"
            )));
        }
        None => {
            return Err(refused(format!(
                "the lane's tree has no branch checked out, not the run branch {branch}"
            )));
        }
    }
    check_free_space(path, settings.min_free_disk_mb, free_space)?;
    let repository = repository_root(git, path).await?;
    let runs = runs_dir(&repository, settings.worktrees_dir.as_deref());
    let run_dir = run_dir(&runs, &task.slug, run_id, attempt);
    std::fs::create_dir_all(&run_dir).map_err(|error| WorktreeError::Io {
        path: run_dir.clone(),
        message: error.to_string(),
    })?;
    Ok(RunWorktree {
        run_dir,
        path: path.to_path_buf(),
        branch,
    })
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
    /// The attempt's run worktree as its record says; its fan-out worktrees are found beside it.
    /// Checked against [`check_run_dir`] before anything in it is removed.
    pub worktree: RunWorktree,
    /// The attempt number, which the run directory's name carries.
    pub attempt: u32,
    /// The task the run is on, with its own worktree: the repository comes from there.
    pub checkout: TaskCheckout,
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
    /// The short sha of saved work at the head of the branch (a commit whose subject starts with
    /// [`WORK_LEFT_BY`]): the worktree goes, but the branch is kept unless the plan's
    /// `delete_saved` is set.
    pub saved: Option<String>,
}

/// The subject prefix of the commit that saves the work a job left in its tree.
pub const WORK_LEFT_BY: &str = "work left by ";

/// Where saved work goes when the branch of its run cannot hold it.
pub const SAVED_WORK_REFS: &str = "refs/bongos/saved/";

/// A branch or a [`SAVED_WORK_REFS`] ref whose head is saved work, which `clear finished` lists
/// and keeps unless the person confirms deleting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedWork {
    pub repository: PathBuf,
    /// The full ref name.
    pub reference: String,
    pub sha: String,
}

impl SavedWork {
    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(12)]
    }

    /// The branch name, or the full ref name of a saved ref.
    pub fn name(&self) -> &str {
        self.reference
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.reference)
    }
}

impl fmt::Display for SavedWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.name(), self.short_sha())
    }
}

/// The branches and [`SAVED_WORK_REFS`] refs of `repository` whose head is saved work.
pub async fn saved_work(git: &Git, repository: &Path) -> Result<Vec<SavedWork>, GitError> {
    let listing = git
        .run(
            repository,
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)%00%(contents:subject)",
                "refs/heads/",
                SAVED_WORK_REFS,
            ],
        )
        .await?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\0');
            let reference = fields.next()?;
            let sha = fields.next()?;
            let subject = fields.next()?;
            subject.starts_with(WORK_LEFT_BY).then(|| SavedWork {
                repository: repository.to_path_buf(),
                reference: reference.to_string(),
                sha: sha.to_string(),
            })
        })
        .collect())
}

/// The worktrees of one run that `clear finished` would remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearRun {
    pub run_id: String,
    pub run_dir: PathBuf,
    /// The main checkout of the task's own worktree, which git removes the worktrees from; `None`
    /// when the run directory was refused, and then nothing is removed.
    pub repository: Option<PathBuf>,
    /// Fan-out worktrees first, then the run worktree, so the run branch goes last.
    pub remove: Vec<ClearItem>,
    /// Worktrees with uncommitted changes, which are kept.
    pub skipped: Vec<ClearItem>,
    /// What [`apply_clear`] checks again right before it removes anything; `None` when the run
    /// was refused, and then nothing is removed.
    pub check: Option<ClearCheck>,
}

/// The record of a run and what it was checked against when the plan was made, so
/// [`apply_clear`] can check it again after the confirmation, right before removing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearCheck {
    pub worktree: RunWorktree,
    pub worktrees_dir: Option<PathBuf>,
    pub home: PathBuf,
    pub task_slug: String,
    pub task_branch: Option<String>,
    pub run_id: String,
    pub attempt: u32,
}

impl ClearCheck {
    fn new(run: &FinishedRun, worktrees_dir: Option<&Path>, home: &Path) -> Self {
        Self {
            worktree: run.worktree.clone(),
            worktrees_dir: worktrees_dir.map(Path::to_path_buf),
            home: home.to_path_buf(),
            task_slug: run.checkout.slug.clone(),
            task_branch: run.checkout.branch.clone(),
            run_id: run.run_id.clone(),
            attempt: run.attempt,
        }
    }

    /// [`check_run_dir`] and [`check_run_branch`] against `repository`.
    pub fn check(&self, repository: &Path) -> Result<(), String> {
        check_run_dir(
            &self.worktree,
            repository,
            self.worktrees_dir.as_deref(),
            &self.home,
            &self.task_slug,
            &self.run_id,
            self.attempt,
        )?;
        check_run_branch(
            &self.worktree,
            self.task_branch.as_deref(),
            &self.run_id,
            self.attempt,
        )
    }

    /// Why `item` is not a worktree and branch of this run: its worktree must sit directly in the
    /// run directory, and its branch must be the run branch or a fan-out branch of it.
    fn refuse_item(&self, item: &ClearItem) -> Option<String> {
        let run_branch = &self.worktree.branch;
        if item.path.parent() != Some(self.worktree.run_dir.as_path()) {
            return Some(format!(
                "{} is not in the run directory {}",
                item.path.display(),
                self.worktree.run_dir.display()
            ));
        }
        let ours = item.branch == *run_branch
            || item
                .branch
                .strip_prefix(run_branch.as_str())
                .is_some_and(|rest| rest.len() > 1 && rest.starts_with('-'));
        (!ours).then(|| {
            format!(
                "branch {} is not the run branch {run_branch} or one of its fan-out branches",
                item.branch
            )
        })
    }
}

/// What `clear finished` would do, listed for the confirmation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearPlan {
    pub runs: Vec<ClearRun>,
    /// The saved work of the runs' repositories on no branch of an item: kept unless
    /// `delete_saved`.
    pub saved: Vec<SavedWork>,
    /// The person confirmed deleting the saved work: the item branches that hold it and `saved`.
    pub delete_saved: bool,
}

impl ClearPlan {
    /// How many branches and refs holding saved work the plan lists.
    pub fn saved_count(&self) -> usize {
        let on_items = self
            .runs
            .iter()
            .flat_map(|run| &run.remove)
            .filter(|item| item.saved.is_some())
            .count();
        on_items + self.saved.len()
    }

    /// The confirmation text: what is removed and what is skipped, then the saved work kept.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        let mut kept = Vec::new();
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
                if let Some(sha) = &item.saved {
                    kept.push(format!("keep {} at {sha}: saved work", item.branch));
                }
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
        lines.extend(kept);
        for saved in &self.saved {
            lines.push(format!("keep {saved}: saved work"));
        }
        lines.join("\n")
    }

    pub fn skipped(&self) -> impl Iterator<Item = &ClearItem> {
        self.runs.iter().flat_map(|run| run.skipped.iter())
    }
}

/// Plans `workflow: clear finished`: the succeeded and cancelled runs that finished more than
/// `keep_finished_days` before `now`, with each of their worktrees to remove, and those with
/// uncommitted changes skipped. A run whose recorded run directory is not exactly the one the app
/// made in the task's own repository ([`check_run_dir`], with `workflows.worktrees_dir` and the
/// home directory `home`) is kept whole, with the reason. Every branch and [`SAVED_WORK_REFS`] ref
/// of the runs' repositories whose head is saved work is listed, on its item or in `saved`, and
/// kept unless the person confirms deleting it (`delete_saved`). Writes nothing.
pub async fn plan_clear(
    git: &Git,
    runs: &[FinishedRun],
    worktrees_dir: Option<&Path>,
    home: &Path,
    keep_finished_days: u64,
    now: SystemTime,
) -> Result<ClearPlan, GitError> {
    let keep = Duration::from_secs(keep_finished_days.saturating_mul(24 * 60 * 60));
    let mut plan = ClearPlan::default();
    // The saved work of each repository, listed once.
    let mut saved: Vec<(PathBuf, Vec<SavedWork>)> = Vec::new();
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
            repository: None,
            remove: Vec::new(),
            skipped: Vec::new(),
            check: None,
        };
        match task_repository(git, run, worktrees_dir, home).await {
            Ok(repository) => {
                clear.repository = Some(repository);
                clear.check = Some(ClearCheck::new(run, worktrees_dir, home));
            }
            Err(reason) => {
                clear.skipped.push(ClearItem {
                    path: run.worktree.path.clone(),
                    branch: run.worktree.branch.clone(),
                    reason: Some(reason),
                    unmerged: None,
                    saved: None,
                });
                plan.runs.push(clear);
                continue;
            }
        }
        let repository = clear.repository.clone().unwrap_or_default();
        let index = match saved.iter().position(|(listed, _)| *listed == repository) {
            Some(index) => index,
            None => {
                saved.push((repository.clone(), saved_work(git, &repository).await?));
                saved.len() - 1
            }
        };
        for mut item in run_worktrees(git, &run.worktree).await {
            let head = format!("refs/heads/{}", item.branch);
            let listed = &mut saved[index].1;
            if let Some(position) = listed.iter().position(|work| work.reference == head) {
                item.saved = Some(listed.remove(position).short_sha().to_string());
            }
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
    plan.saved = saved.into_iter().flat_map(|(_, listed)| listed).collect();
    Ok(plan)
}

/// The main checkout of the repository of `run`'s task, from the task's own worktree, once the
/// recorded run directory and run branch are checked against it ([`check_run_dir`],
/// [`check_run_branch`]).
async fn task_repository(
    git: &Git,
    run: &FinishedRun,
    worktrees_dir: Option<&Path>,
    home: &Path,
) -> Result<PathBuf, String> {
    let task_worktree = match &run.checkout.worktree {
        Some(worktree) if worktree.is_dir() => worktree,
        _ => return Err("the task worktree is gone, so its repository is unknown".to_string()),
    };
    let repository = repository_root(git, task_worktree)
        .await
        .map_err(|error| format!("git failed: {}", error.message))?;
    ClearCheck::new(run, worktrees_dir, home).check(&repository)?;
    Ok(repository)
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
            saved: None,
        });
    }
    // Only a run worktree still on the run branch is removed with it. When the worktree is gone,
    // its branch name was checked against the record ([`check_run_branch`]) and goes alone.
    let reason = if run.path.is_dir() {
        match git
            .run(&run.path, &["symbolic-ref", "--short", "HEAD"])
            .await
        {
            Ok(head) if head == run.branch => None,
            Ok(head) => Some(format!("on branch {head}, not {}", run.branch)),
            Err(_) => Some("HEAD is detached or unreadable".to_string()),
        }
    } else {
        None
    };
    items.push(ClearItem {
        path: run.path.clone(),
        branch: run.branch.clone(),
        reason,
        unmerged: None,
        saved: None,
    });
    items
}

/// What `clear finished` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearReport {
    pub removed: Vec<ClearItem>,
    /// Kept: uncommitted changes, or git refused, with the reason.
    pub skipped: Vec<(ClearItem, String)>,
    /// The branches and refs holding saved work that were kept, named with their short sha.
    pub kept_saved: Vec<String>,
    /// The branches and refs holding saved work that the person confirmed deleting.
    pub deleted_saved: Vec<String>,
}

impl fmt::Display for ClearReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "removed {} worktrees", self.removed.len())?;
        for (item, reason) in &self.skipped {
            write!(f, "\nskipped {}: {reason}", item.path.display())?;
        }
        for saved in &self.kept_saved {
            write!(f, "\nkept saved work {saved}")?;
        }
        for saved in &self.deleted_saved {
            write!(f, "\ndeleted saved work {saved}")?;
        }
        Ok(())
    }
}

/// Applies a confirmed [`ClearPlan`]: `git worktree remove` (never forced) and `git branch -D`
/// for each worktree, from `repository`, then, once all its worktrees are gone, the run directory
/// with [`remove_run_dir`], which never deletes recursively. A run directory that is not empty
/// after that is kept and reported. Each run is checked again first ([`ClearCheck`]), since the
/// disk may have changed since the plan: a run that no longer passes, or that has no check, is
/// kept whole, and an item whose worktree is not in the run directory or whose branch is not the
/// run branch or one of its fan-out branches is kept. A branch holding saved work and the plan's
/// `saved` are kept unless `delete_saved`; each saved entry is deleted in its own repository.
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
        let checked = match &run.check {
            Some(check) if check.worktree.run_dir != run.run_dir => Err(format!(
                "{} is not the run directory that was checked",
                run.run_dir.display()
            )),
            Some(check) => check.check(repository).map(|()| check),
            None => Err("the run was not checked, so nothing in it is removed".to_string()),
        };
        let check = match checked {
            Ok(check) => check,
            Err(reason) => {
                for item in &run.remove {
                    report.skipped.push((item.clone(), reason.clone()));
                }
                continue;
            }
        };
        for item in &run.remove {
            if let Some(reason) = check.refuse_item(item) {
                all_removed = false;
                report.skipped.push((item.clone(), reason));
                continue;
            }
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
            let label = item
                .saved
                .as_ref()
                .map(|sha| format!("{} at {sha}", item.branch));
            match label {
                Some(label) if !plan.delete_saved => report.kept_saved.push(label),
                _ => match git
                    .run(
                        repository,
                        &["branch", "-D", "--end-of-options", &item.branch],
                    )
                    .await
                {
                    Ok(_) => report.deleted_saved.extend(label),
                    Err(error) => log::warn!("removing branch {}: {error}", item.branch),
                },
            }
            report.removed.push(item.clone());
        }
        if all_removed
            && std::fs::symlink_metadata(&run.run_dir).is_ok()
            && let Err(error) = remove_run_dir(&run.run_dir)
        {
            log::warn!("removing {}: {error}", run.run_dir.display());
            report.skipped.push((
                ClearItem {
                    path: run.run_dir.clone(),
                    branch: String::new(),
                    reason: None,
                    unmerged: None,
                    saved: None,
                },
                format!("kept the run directory: {error}"),
            ));
        }
    }
    for saved in &plan.saved {
        if !plan.delete_saved {
            report.kept_saved.push(saved.to_string());
            continue;
        }
        // `branch -D` refuses a branch a worktree has checked out; the old sha guards a saved ref
        // that moved since the plan.
        let deleting = match saved.reference.strip_prefix("refs/heads/") {
            Some(branch) => {
                git.run(
                    &saved.repository,
                    &["branch", "-D", "--end-of-options", branch],
                )
                .await
            }
            None => {
                git.run(
                    &saved.repository,
                    &["update-ref", "-d", &saved.reference, &saved.sha],
                )
                .await
            }
        };
        match deleting {
            Ok(_) => report.deleted_saved.push(saved.to_string()),
            Err(error) => {
                log::warn!("deleting {}: {error}", saved.reference);
                report
                    .kept_saved
                    .push(format!("{saved} ({})", error.message));
            }
        }
    }
    report
}

/// Removes a run directory whose worktrees git has removed, without ever deleting recursively:
/// only what the app itself leaves there goes, then the directory with the non-recursive
/// `remove_dir`, which fails when anything else is left. What the app leaves, beside the
/// worktrees, is the scratch tree of its program nodes:
/// `scratch/<node>/<branch>/<pass>-<try>/` with the file `result.json` and the directory `tmp/`.
/// Only a regular file of that name is deleted and only directories that are not symlinks are
/// entered; every directory is removed with `remove_dir`, so one with anything else in it stays.
pub fn remove_run_dir(run_dir: &Path) -> std::io::Result<()> {
    fn real_dir(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
    }
    fn subdirs(path: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs
    }
    // Failures below leave a directory that is not empty, which the final `remove_dir` reports.
    let scratch = run_dir.join(SCRATCH_DIR);
    if real_dir(&scratch) {
        for node in subdirs(&scratch) {
            for branch in subdirs(&node) {
                for attempt in subdirs(&branch) {
                    let result = attempt.join(crate::runner_program::SCRATCH_RESULT_FILE);
                    if std::fs::symlink_metadata(&result)
                        .is_ok_and(|metadata| metadata.file_type().is_file())
                    {
                        std::fs::remove_file(&result).ok();
                    }
                    std::fs::remove_dir(attempt.join(SCRATCH_TMP_DIR)).ok();
                    std::fs::remove_dir(&attempt).ok();
                }
                std::fs::remove_dir(&branch).ok();
            }
            std::fs::remove_dir(&node).ok();
        }
        std::fs::remove_dir(&scratch).ok();
    }
    std::fs::remove_dir(run_dir)
}
