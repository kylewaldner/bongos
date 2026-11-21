//! The files a task changed, as scripts receive them: everything that differs from the merge
//! base with the base branch, committed or not, plus the untracked files that are not ignored.

use std::path::{Path, PathBuf};

use bongos_project_repo::GitError;

use crate::task_git::TaskGit;
use crate::work_tree::{Problem, ensure_top_level};

/// The outcome of [`changed_files`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangedFiles {
    /// No base branch was given, so no list can be made. The caller hands scripts `all` with
    /// the reason "no base branch".
    NoBaseBranch,
    Changed(ChangedSet),
}

/// The changed files and the commit they are measured from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedSet {
    /// The merge base of `HEAD` and the base branch, resolved once for this list, so the caller
    /// can record exactly what the list was measured against.
    pub merge_base: String,
    /// Repository-relative, `/`-separated, sorted, without duplicates. Includes files the
    /// change deleted.
    pub paths: Vec<String>,
}

impl ChangedSet {
    /// The list as text, one path per line, each line ending in a newline; empty for no files.
    pub fn as_lines(&self) -> String {
        self.paths.iter().map(|path| format!("{path}\n")).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChangedFilesError {
    #[error("{path} is not a git working tree: {detail}")]
    NotGitTree { path: PathBuf, detail: String },
    #[error("{path} is inside the git working tree {top}, not at its top; pass the top directory")]
    NotTopLevel { path: PathBuf, top: PathBuf },
    #[error("the base branch {branch:?} does not name a commit")]
    BaseBranchMissing { branch: String },
    #[error("the base branch {branch:?} is not a valid ref name")]
    InvalidBaseBranch { branch: String },
    #[error("HEAD and {branch:?} have no merge base: {detail}")]
    NoMergeBase { branch: String, detail: String },
    #[error("a changed path is not valid UTF-8: {path:?}")]
    NonUtf8Path { path: Vec<u8> },
    #[error(transparent)]
    Git(#[from] GitError),
}

/// The files changed in the working copy at `tree` since it left `base_branch`.
///
/// With a base branch, the merge base of `HEAD` and the branch is resolved once, then the list
/// is the union of `git diff --name-only -z <merge-base>` (committed and uncommitted changes to
/// tracked files, renames shown as a delete and an add) and
/// `git ls-files --others --exclude-standard -z` (untracked files that .gitignore does not
/// exclude). Paths are read from NUL-separated output, so a name with a space or a newline
/// arrives whole. A path that is not UTF-8 is an error rather than a lossy name that would point
/// at no file. A missing base branch is `BaseBranchMissing`, never a guess. `tree` must be the
/// top directory of the working tree; a subdirectory of one is `NotTopLevel`, because the paths
/// are relative to the top.
pub async fn changed_files(
    git: &TaskGit,
    tree: &Path,
    base_branch: Option<&str>,
) -> Result<ChangedFiles, ChangedFilesError> {
    let Some(branch) = base_branch else {
        return Ok(ChangedFiles::NoBaseBranch);
    };
    let merge_base = merge_base(git, tree, branch).await?;

    let tracked = git
        .run_raw(
            tree,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                &merge_base,
                "--",
            ],
        )
        .await?;
    let untracked = git
        .run_raw(tree, &["ls-files", "--others", "--exclude-standard", "-z"])
        .await?;
    let mut paths = Vec::new();
    for listing in [tracked, untracked] {
        for raw in listing
            .split(|byte| *byte == 0)
            .filter(|raw| !raw.is_empty())
        {
            let path = std::str::from_utf8(raw)
                .map_err(|_| ChangedFilesError::NonUtf8Path { path: raw.to_vec() })?;
            paths.push(path.to_string());
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(ChangedFiles::Changed(ChangedSet { merge_base, paths }))
}

/// The merge base of `HEAD` of the working copy at `tree` and `branch`, resolved now. A missing
/// branch is `BaseBranchMissing`, never a guess; `tree` must be the top of its working tree.
pub(crate) async fn merge_base(
    git: &TaskGit,
    tree: &Path,
    branch: &str,
) -> Result<String, ChangedFilesError> {
    if branch.is_empty() || branch.starts_with('-') || branch.contains('\0') {
        return Err(ChangedFilesError::InvalidBaseBranch {
            branch: branch.to_string(),
        });
    }
    ensure_top_level(git, tree)
        .await
        .map_err(|problem| match problem {
            Problem::NotWorkTree(detail) => ChangedFilesError::NotGitTree {
                path: tree.to_path_buf(),
                detail,
            },
            Problem::NotTopLevel(top) => ChangedFilesError::NotTopLevel {
                path: tree.to_path_buf(),
                top,
            },
        })?;

    let target = format!("{branch}^{{commit}}");
    let base = match git
        .run(
            tree,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &target,
            ],
        )
        .await
    {
        Ok(id) => id,
        Err(GitError::Failed { .. }) => {
            return Err(ChangedFilesError::BaseBranchMissing {
                branch: branch.to_string(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    match git.merge_base(tree, "HEAD", &base).await {
        Ok(id) if !id.is_empty() => Ok(id),
        Ok(_) => Err(ChangedFilesError::NoMergeBase {
            branch: branch.to_string(),
            detail: "git printed nothing".to_string(),
        }),
        Err(GitError::Failed { stderr, .. }) => Err(ChangedFilesError::NoMergeBase {
            branch: branch.to_string(),
            detail: stderr,
        }),
        Err(error) => Err(error.into()),
    }
}
