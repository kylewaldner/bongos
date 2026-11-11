//! The shipped `merge` program node (feature 20, D10): brings the run branch into the task branch,
//! in the task worktree, which no agent or program may touch. It never rebases. Before anything is
//! written it refuses a task worktree with uncommitted changes; a conflict is aborted, leaving both
//! branches and the task worktree as they were.

use std::path::{Path, PathBuf};

use crate::worktrees::{Git, GitError, MergeError, short_id};

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
