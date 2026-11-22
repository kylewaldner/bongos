//! The shipped `merge` program node (feature 20, D10): brings the run branch into the task branch,
//! in the task worktree, which no agent or program may touch. It never rebases. Before anything is
//! written it refuses a task worktree with uncommitted changes; a conflict is aborted, leaving both
//! branches and the task worktree as they were.
//!
//! A run with no task merges at its success end with [`merge_project_run`] instead: into the
//! branch its start named, with no checkout at all, never into a branch a worktree holds.

use std::path::{Path, PathBuf};

use crate::worktrees::{Git, GitError, MergeError, checked_out_branch, short_id, worktree_holding};

/// The refusal for a task worktree with uncommitted changes.
pub const DIRTY_TASK_WORKTREE: &str =
    "the task worktree has uncommitted changes; commit or stash them";

/// How the run branch is merged. There is no rebase style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeStyle {
    /// Fast-forward when possible, else a merge commit.
    #[default]
    FfOrMerge,
    /// Fast-forward only; fails when the task branch moved.
    FfOnly,
    /// Always a merge commit.
    MergeCommit,
}

impl MergeStyle {
    /// Parses `ff-or-merge`, `ff-only` or `merge-commit`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ff-or-merge" => Some(Self::FfOrMerge),
            "ff-only" => Some(Self::FfOnly),
            "merge-commit" => Some(Self::MergeCommit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FfOrMerge => "ff-or-merge",
            Self::FfOnly => "ff-only",
            Self::MergeCommit => "merge-commit",
        }
    }
}

/// One merge of a run into its task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequest {
    pub task_worktree: PathBuf,
    pub task_branch: String,
    pub run_branch: String,
    pub run_id: String,
    pub workflow: String,
    /// The name of the todo the run works on.
    pub todo: String,
    pub style: MergeStyle,
}

impl MergeRequest {
    /// `Merge run <short id> (<workflow>) for todo <name>`.
    pub fn commit_message(&self) -> String {
        format!(
            "Merge run {} ({}) for todo {}",
            short_id(&self.run_id),
            self.workflow,
            self.todo
        )
    }
}

/// A merge that happened, with the task branch's new head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merged {
    FastForward { head: String },
    MergeCommit { head: String },
}

/// Why the merge did not happen. In every case the task branch, the run branch and the task
/// worktree are as they were.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MergeRefusal {
    #[error("{DIRTY_TASK_WORKTREE}")]
    DirtyTaskWorktree,
    #[error("the task worktree is on {found}, not the task branch {expected}")]
    WrongBranch { expected: String, found: String },
    #[error("the task branch moved since the run started; ff-only cannot merge the run")]
    NotFastForward,
    /// The files that conflicted; the merge was aborted.
    #[error("conflict in {}", .0.join(", "))]
    Conflict(Vec<String>),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Merges the run branch into the task branch in the task worktree, in `request.style`.
pub async fn merge(git: &Git, request: &MergeRequest) -> Result<Merged, MergeRefusal> {
    let worktree = request.task_worktree.as_path();
    if git.is_dirty(worktree).await? {
        return Err(MergeRefusal::DirtyTaskWorktree);
    }
    let current = git
        .run(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    if current != request.task_branch {
        return Err(MergeRefusal::WrongBranch {
            expected: request.task_branch.clone(),
            found: if current.is_empty() {
                "a detached HEAD".to_string()
            } else {
                current
            },
        });
    }
    let fast_forward = can_fast_forward(git, worktree, &request.run_branch).await?;
    match (request.style, fast_forward) {
        (MergeStyle::FfOrMerge | MergeStyle::FfOnly, true) => {
            git.run(
                worktree,
                &[
                    "merge",
                    "--ff-only",
                    "--end-of-options",
                    &request.run_branch,
                ],
            )
            .await?;
            Ok(Merged::FastForward {
                head: head(git, worktree).await?,
            })
        }
        (MergeStyle::FfOnly, false) => Err(MergeRefusal::NotFastForward),
        (MergeStyle::FfOrMerge | MergeStyle::MergeCommit, _) => {
            let message = request.commit_message();
            match git
                .merge_or_abort(
                    worktree,
                    &[
                        "--no-ff",
                        "-m",
                        &message,
                        "--end-of-options",
                        &request.run_branch,
                    ],
                )
                .await
            {
                Ok(()) => Ok(Merged::MergeCommit {
                    head: head(git, worktree).await?,
                }),
                Err(MergeError::Conflict(files)) => Err(MergeRefusal::Conflict(files)),
                Err(MergeError::Git(error)) => Err(error.into()),
            }
        }
    }
}

/// The success-end merge of a run with no task: its run branch into `merge_into`, read from the
/// run's own tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectMergeRequest {
    pub run_tree: PathBuf,
    pub run_branch: String,
    pub merge_into: String,
    pub run_id: String,
    pub workflow: String,
}

impl ProjectMergeRequest {
    /// `Merge run <short id> (<workflow>)`.
    pub fn commit_message(&self) -> String {
        format!("Merge run {} ({})", short_id(&self.run_id), self.workflow)
    }
}

/// A project run's merge that needs nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectMerged {
    /// The run branch was merged; `head` is the new head of `merge_into`.
    Merged { head: String },
    /// `merge_into` already holds every commit of the run branch: nothing was merged.
    AlreadyIn,
}

/// Why a project run's merge did not happen. In every case `merge_into`, the run branch and every
/// checkout are as they were, and the run tree is still on the run branch.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectMergeRefusal {
    /// Another worktree, a person's checkout included, has the target checked out.
    #[error("branch {branch} is checked out at {}; merge it there", .path.display())]
    CheckedOut { branch: String, path: PathBuf },
    #[error("there is no branch {0} to merge the run into")]
    NoBranch(String),
    #[error("the run tree has uncommitted changes; commit them first")]
    DirtyRunTree,
    #[error("the run tree is on {found}, not the run branch {expected}")]
    WrongBranch { expected: String, found: String },
    #[error("conflict in {}", .0.join(", "))]
    Conflict(Vec<String>),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// How many times the merge is made again when `merge_into` moved while it was being made.
const PROJECT_MERGE_TRIES: usize = 3;

/// Merges the run branch into `merge_into` without checking anything out: refused, before
/// anything is written, when `merge_into` is missing, when any worktree has it checked out (so no
/// checkout ever changes under a person), when the run tree has uncommitted changes or is not on
/// the run branch. Nothing is merged when `merge_into` already holds the run branch. Otherwise
/// the new head of `merge_into` is the run branch's head (fast-forward) or a merge commit made
/// from `git merge-tree` (a conflict writes nothing), and the branch is moved to it only if it
/// still points where it did, so the run tree stays on its run branch whatever happens, a crash
/// included.
pub async fn merge_project_run(
    git: &Git,
    request: &ProjectMergeRequest,
) -> Result<ProjectMerged, ProjectMergeRefusal> {
    let tree = request.run_tree.as_path();
    let target = format!("refs/heads/{}", request.merge_into);
    let run_ref = format!("refs/heads/{}", request.run_branch);
    let mut tries = 0;
    loop {
        tries += 1;
        let Ok(old) = commit_of(git, tree, &target).await else {
            return Err(ProjectMergeRefusal::NoBranch(request.merge_into.clone()));
        };
        let run_head = commit_of(git, tree, &run_ref).await?;
        if is_ancestor(git, tree, &run_head, &old).await? {
            return Ok(ProjectMerged::AlreadyIn);
        }
        refuse_unsafe(git, request).await?;
        let new = if is_ancestor(git, tree, &old, &run_head).await? {
            run_head.clone()
        } else {
            merge_commit(git, tree, request, &old, &run_head).await?
        };
        // Checked once more right before the write, so a checkout made meanwhile is never moved.
        refuse_unsafe(git, request).await?;
        let moved = git
            .run(
                tree,
                &[
                    "update-ref",
                    "-m",
                    &request.commit_message(),
                    &target,
                    &new,
                    &old,
                ],
            )
            .await;
        match moved {
            Ok(_) => return Ok(ProjectMerged::Merged { head: new }),
            // `merge_into` moved since it was read: made again from its new head.
            Err(_) if tries < PROJECT_MERGE_TRIES => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// The refusals checked before `merge_into` is written: a worktree holding it, a run tree with
/// uncommitted changes or not on the run branch.
async fn refuse_unsafe(
    git: &Git,
    request: &ProjectMergeRequest,
) -> Result<(), ProjectMergeRefusal> {
    let tree = request.run_tree.as_path();
    // The run tree is checked too: one on `merge_into` is refused below as on the wrong branch.
    if let Some(path) = worktree_holding(git, tree, &request.merge_into, tree).await? {
        return Err(ProjectMergeRefusal::CheckedOut {
            branch: request.merge_into.clone(),
            path,
        });
    }
    if git.is_dirty(tree).await? {
        return Err(ProjectMergeRefusal::DirtyRunTree);
    }
    let current = checked_out_branch(git, tree).await.unwrap_or_default();
    if current != request.run_branch {
        return Err(ProjectMergeRefusal::WrongBranch {
            expected: request.run_branch.clone(),
            found: if current.is_empty() {
                "a detached HEAD".to_string()
            } else {
                current
            },
        });
    }
    Ok(())
}

/// A merge commit of `run_head` into `old`, written to the object store only; a conflict writes
/// no commit and names the conflicted files.
async fn merge_commit(
    git: &Git,
    tree: &Path,
    request: &ProjectMergeRequest,
    old: &str,
    run_head: &str,
) -> Result<String, ProjectMergeRefusal> {
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        old,
        run_head,
    ];
    let output = git.output(tree, &args).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let merged_tree = lines.next().unwrap_or_default().trim().to_string();
    match output.status.code() {
        Some(0) if !merged_tree.is_empty() => {}
        Some(1) => {
            let mut files: Vec<String> = lines
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            files.dedup();
            return Err(ProjectMergeRefusal::Conflict(files));
        }
        _ => {
            return Err(GitError {
                args: args.join(" "),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }
            .into());
        }
    }
    let message = request.commit_message();
    Ok(git
        .run(
            tree,
            &[
                "commit-tree",
                &merged_tree,
                "-p",
                old,
                "-p",
                run_head,
                "-m",
                &message,
            ],
        )
        .await?)
}

/// The commit `reference` names.
async fn commit_of(git: &Git, dir: &Path, reference: &str) -> Result<String, GitError> {
    git.run(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await
}

/// Whether `ancestor` is an ancestor of `of` (or the same commit).
async fn is_ancestor(git: &Git, dir: &Path, ancestor: &str, of: &str) -> Result<bool, GitError> {
    let output = git
        .output(dir, &["merge-base", "--is-ancestor", ancestor, of])
        .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError {
            args: format!("merge-base --is-ancestor {ancestor} {of}"),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }),
    }
}

/// Whether HEAD is an ancestor of `branch`, so the merge can fast-forward.
async fn can_fast_forward(git: &Git, worktree: &Path, branch: &str) -> Result<bool, GitError> {
    let output = git
        .output(worktree, &["merge-base", "--is-ancestor", "HEAD", branch])
        .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError {
            args: format!("merge-base --is-ancestor HEAD {branch}"),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }),
    }
}

async fn head(git: &Git, worktree: &Path) -> Result<String, GitError> {
    git.run(worktree, &["rev-parse", "HEAD"]).await
}
