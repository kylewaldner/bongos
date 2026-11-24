//! Git in a lane tree, without ever trusting the tree.
//!
//! A job running in a lane's persistent tree may have written its `.git` file, its configuration,
//! its hooks, `core.fsmonitor` and `.gitattributes`. So no git runs in the tree by discovery: the
//! tree's linked git directory is looked up in the user's own repository record (the entries
//! below `<common git dir>/worktrees` whose `gitdir` file names the tree's `.git`), a crashed job's
//! private git is recovered first, and every command is built by `SafeGit` for that git directory
//! and the tree as a job tree. Nothing here reads the tree's `.git` for a decision except to check
//! that it still names the directory the record gave.
//!
//! Behavior changes this brings: no hook runs in a lane tree at a job start, reset or merge (every
//! command is a job tree command, `core.hooksPath=/dev/null`), and `status` runs with
//! `--ignore-submodules=all`, so a changed submodule does not make a tree dirty.
//!
//! The recovery of a crashed job's private git ([`recover`]) holds the tree's lock only while it
//! runs, not during the reset commands that follow. That is safe only under this rule: a lane
//! never resets a tree and starts a job on it at once; a job starts on the tree only under the
//! lane's own lease (feature 48 D10), so no other job can take its private git between the
//! recovery and the reset. No locking is added here.

use std::fs::{canonicalize, read_dir, read_to_string, symlink_metadata};
use std::path::{Path, PathBuf};

use bongos_confine::private_git::{RecoverOutcome, recover};
use bongos_confine::safe_git::linked_git_dir_from_record;
use bongos_workflow::worktrees::{At, Git, GitError};

/// The common git directory of the repository whose primary checkout is `repository`, read in
/// the user's own checkout (never in a lane tree).
pub(crate) async fn common_dir(git: &Git, repository: &Path) -> Result<PathBuf, GitError> {
    let text = git
        .run(
            repository,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
    Ok(PathBuf::from(text))
}

/// The linked git directory the repository record (`<common>/worktrees/*`) has for `tree`: the
/// only entry whose `gitdir` file names `<tree>/.git`, compared with links resolved. Reads nothing
/// in the tree but its path, so it works while the tree's own `.git` names a crashed job's private
/// directory. Returns the canonical directory.
pub(crate) fn linked_git_dir_in_record(
    tree: &Path,
    common_dir: &Path,
    repository: &Path,
) -> Result<PathBuf, String> {
    let missing = || {
        format!(
            "{} is not a git worktree of its own and not a worktree of {}: its repository \
             records no linked worktree there; nothing was reset",
            tree.display(),
            repository.display()
        )
    };
    let wanted = canonicalize(tree)
        .map_err(|error| format!("{}: {error}; nothing was reset", tree.display()))?
        .join(".git");
    let entries = match read_dir(common_dir.join("worktrees")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(error) => {
            return Err(format!(
                "{}: {error}; nothing was reset",
                common_dir.join("worktrees").display()
            ));
        }
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !symlink_metadata(&dir).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            continue;
        }
        let Ok(text) = read_to_string(dir.join("gitdir")) else {
            continue;
        };
        // `join` keeps an absolute path as it is; git may also write one relative to `dir`.
        let recorded = dir.join(text.trim_end_matches(['\n', '\r']));
        if recorded.file_name() != Some(std::ffi::OsStr::new(".git")) {
            continue;
        }
        let Some(parent) = recorded
            .parent()
            .and_then(|parent| canonicalize(parent).ok())
        else {
            continue;
        };
        if parent.join(".git") == wanted {
            found.push(dir);
        }
    }
    match found.as_slice() {
        [] => Err(missing()),
        [dir] => canonicalize(dir).map_err(|error| format!("{}: {error}", dir.display())),
        _ => Err(format!(
            "{} is recorded by more than one linked worktree of {}; nothing was reset",
            tree.display(),
            repository.display()
        )),
    }
}

/// A lane tree and its linked git directory, trusted: every command runs through `SafeGit` for
/// that directory and the tree as a job tree.
#[derive(Clone, Debug)]
pub(crate) struct LaneGit {
    git: Git,
    tree: PathBuf,
    git_dir: PathBuf,
}

impl LaneGit {
    /// Resolves `tree`'s git directory from the record of the repository whose primary checkout
    /// is `repository`, recovers a crashed job's private git, and checks that the tree's `.git`
    /// names the recorded directory again. Fails, having changed nothing in the tree, when the tree
    /// is not in the record, when a running job holds it (the lane waits), or when recovery fails.
    ///
    /// `fresh` is for a tree this process has just added with `git worktree add`, where no job has
    /// run yet: git itself is asked for its git directory once, and it must be the recorded one.
    pub(crate) async fn open(
        git: &Git,
        tree: &Path,
        repository: &Path,
        fresh: bool,
    ) -> Result<Self, String> {
        let common = common_dir(git, repository)
            .await
            .map_err(|error| error.to_string())?;
        if let (Ok(real), Ok(primary)) = (canonicalize(tree), canonicalize(repository))
            && real == primary
        {
            return Err(format!(
                "{} is a primary checkout, not a lane tree; nothing was reset",
                tree.display()
            ));
        }
        let git_dir = linked_git_dir_in_record(tree, &common, repository)?;
        if fresh {
            let named = fresh_git_dir(git, tree).await?;
            if named != git_dir {
                return Err(format!(
                    "the new lane tree {} has the git directory {}, not the recorded {}; \
                     nothing was reset",
                    tree.display(),
                    named.display(),
                    git_dir.display()
                ));
            }
        }
        match recover(&git_dir) {
            Ok(RecoverOutcome::Recovered | RecoverOutcome::NoMarker) => {}
            Ok(RecoverOutcome::InUse) => {
                return Err(format!(
                    "the lane tree {} is in use by a running job (its git is private to that \
                     job), so the lane waits for it; nothing was changed",
                    tree.display()
                ));
            }
            Err(error) => return Err(format!("{error:#}")),
        }
        let recorded = linked_git_dir_from_record(tree, &common)
            .map_err(|error| format!("{error:#}; nothing was changed"))?;
        if recorded != git_dir {
            return Err(format!(
                "the .git of {} names {}, not {}; nothing was reset",
                tree.display(),
                recorded.display(),
                git_dir.display()
            ));
        }
        Ok(Self {
            git: git.clone(),
            tree: tree.to_path_buf(),
            git_dir,
        })
    }

    /// The git that runs the commands.
    pub(crate) fn git(&self) -> &Git {
        &self.git
    }

    pub(crate) fn tree(&self) -> &Path {
        &self.tree
    }

    /// The linked git directory, canonical.
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The same tree and git directory with `git` (another environment, say) running the commands.
    pub(crate) fn with_git(&self, git: Git) -> Self {
        Self {
            git,
            tree: self.tree.clone(),
            git_dir: self.git_dir.clone(),
        }
    }

    fn at(&self) -> At<'_> {
        At {
            git_dir: &self.git_dir,
            directory: &self.tree,
            job: true,
        }
    }

    /// Runs git with `args` (starting with the subcommand) in the tree; see [`Git::run_at`].
    pub(crate) async fn run(&self, args: &[&str]) -> Result<String, GitError> {
        self.git.run_at(self.at(), args).await
    }

    /// [`LaneGit::run`] returning the whole output, whatever the exit status.
    pub(crate) async fn output(&self, args: &[&str]) -> Result<std::process::Output, GitError> {
        self.git.output_at(self.at(), args).await
    }

    /// Whether the tree has uncommitted changes, untracked files included.
    pub(crate) async fn is_dirty(&self) -> Result<bool, GitError> {
        self.git.is_dirty(self.at()).await
    }
}

/// The git directory of the tree just added at `tree`, read with git itself before any job ran
/// there, with links resolved.
async fn fresh_git_dir(git: &Git, tree: &Path) -> Result<PathBuf, String> {
    let text = git
        .run(tree, &["rev-parse", "--path-format=absolute", "--git-dir"])
        .await
        .map_err(|error| error.to_string())?;
    canonicalize(&text).map_err(|error| format!("{text}: {error}"))
}
