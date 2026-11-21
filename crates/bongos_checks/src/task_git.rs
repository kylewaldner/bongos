//! Host git for the trees a check reads: a task's tree, a leased second tree and a temporary base
//! tree. A job may write any of them, `.git` included, so git must never discover its repository
//! from one. [`TaskGit`] runs every command through [`SafeGit`] with the git directory recorded for
//! the tree, an explicit work tree, and the protections for an untrusted tree (no hook, no filter,
//! text conversion or fsmonitor, no nested repository).
//!
//! A tree's git directory is recorded once, the first time bongos meets the tree and before a
//! check reads it: [`TaskGit::record_linked`] accepts only a git directory that git itself put
//! directly under `<common>/worktrees/` (see [`linked_git_dir_from_record`]) and whose common
//! directory lies outside the tree. A temporary base tree is recorded when it is made. After
//! that the tree's `.git` is never read again, so rewriting it changes nothing. A tree that
//! cannot be recorded is refused.
//!
//! The project repository is not job-written: a command in that directory runs through [`Git`]
//! as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use bongos_confine::safe_git::{SafeGit, linked_git_dir_from_record};
use bongos_project_repo::{Git, GitError, ProjectRepo};

/// The recorded git directory of each tree, by canonical tree path.
static RECORDS: LazyLock<Mutex<HashMap<PathBuf, PathBuf>>> = LazyLock::new(Default::default);

/// Where a command runs.
enum Target {
    /// The project repository.
    Project,
    /// A tree (canonical) with its recorded git directory.
    Tree { tree: PathBuf, git_dir: PathBuf },
}

/// Git for the trees of one project; see the module documentation.
#[derive(Clone, Debug)]
pub struct TaskGit {
    git: Git,
    /// The project repository, whose own commands keep the plain [`Git`] path.
    repository: PathBuf,
    /// The primary checkouts of the code repositories, a person's own and not job-written: they
    /// keep the plain path too.
    primaries: Vec<PathBuf>,
}

impl TaskGit {
    pub fn new(git: Git, repository: PathBuf) -> Self {
        Self {
            git,
            repository,
            primaries: Vec::new(),
        }
    }

    pub fn of(project: &ProjectRepo) -> Self {
        Self::new(project.git().clone(), project.location().repository.clone())
    }

    /// Also runs plain git in the primary checkout `directory` of a code repository.
    pub fn with_primaries(mut self, directories: impl IntoIterator<Item = PathBuf>) -> Self {
        self.primaries.extend(directories);
        self
    }

    /// The plain runner, for the project repository only.
    pub fn project(&self) -> &Git {
        &self.git
    }

    /// Records `git_dir` as the git directory of the tree at `tree`, replacing any record.
    pub fn record(tree: &Path, git_dir: &Path) -> std::io::Result<()> {
        let tree = tree.canonicalize()?;
        let git_dir = git_dir.canonicalize()?;
        RECORDS.lock().unwrap().insert(tree, git_dir);
        Ok(())
    }

    /// Records the git directory of the linked worktree at `tree`, validated as the module
    /// documentation says, and returns it. A tree already recorded keeps its record.
    pub fn record_linked(tree: &Path) -> Result<PathBuf, GitError> {
        let refused = |message: String| GitError::Refused { message };
        let tree = tree
            .canonicalize()
            .map_err(|error| refused(format!("{} is not usable: {error}", tree.display())))?;
        if let Some(git_dir) = RECORDS.lock().unwrap().get(&tree) {
            return Ok(git_dir.clone());
        }
        let git_dir = match validated_git_dir(&tree) {
            Ok(git_dir) => git_dir,
            // Tests build trees with plain `git init`; production trees are linked worktrees.
            #[cfg(test)]
            Err(_) if tree.join(".git").is_dir() => tree.join(".git"),
            Err(message) => return Err(refused(message)),
        };
        RECORDS.lock().unwrap().insert(tree, git_dir.clone());
        Ok(git_dir)
    }

    /// The top of the recorded tree or tree-looking directory that holds `directory`, for the
    /// error that says `directory` is not at the top. Reports a path only; runs nothing.
    pub fn top_above(directory: &Path) -> Option<PathBuf> {
        let start = directory.canonicalize().ok()?;
        let recorded = RECORDS.lock().unwrap();
        start
            .ancestors()
            .skip(1)
            .find(|ancestor| recorded.contains_key(*ancestor) || ancestor.join(".git").exists())
            .map(Path::to_path_buf)
    }

    /// Whether `directory` is a tree that is recorded or can be.
    pub fn check_tree(&self, directory: &Path) -> Result<(), GitError> {
        self.target(directory).map(|_| ())
    }

    fn target(&self, directory: &Path) -> Result<Target, GitError> {
        let canonical = directory
            .canonicalize()
            .map_err(|error| GitError::Refused {
                message: format!("{} is not usable: {error}", directory.display()),
            })?;
        let plain = std::iter::once(&self.repository).chain(&self.primaries);
        if plain
            .into_iter()
            .any(|plain| plain.canonicalize().ok().as_ref() == Some(&canonical))
        {
            return Ok(Target::Project);
        }
        let git_dir = Self::record_linked(&canonical)?;
        Ok(Target::Tree {
            tree: canonical,
            git_dir,
        })
    }

    /// Runs git with `args` in the tree or repository `directory`; its trimmed output.
    pub async fn run(&self, directory: &Path, args: &[&str]) -> Result<String, GitError> {
        let output = self.run_raw(directory, args).await?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    /// [`TaskGit::run`] with the raw, untrimmed output.
    pub async fn run_raw(&self, directory: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.exec(directory, None, &[], args).await
    }

    /// [`TaskGit::run_raw`] against the index file `index` instead of the repository's, with the
    /// configuration `config` set.
    pub async fn run_on_index(
        &self,
        directory: &Path,
        index: &Path,
        config: &[(&str, &str)],
        args: &[&str],
    ) -> Result<Vec<u8>, GitError> {
        self.exec(directory, Some(index), config, args).await
    }

    pub async fn merge_base(&self, directory: &Path, a: &str, b: &str) -> Result<String, GitError> {
        self.run(directory, &["merge-base", a, b]).await
    }

    /// Adds a worktree at `path` with `commit` detached, from the tree or repository `directory`.
    pub async fn worktree_add_detached(
        &self,
        directory: &Path,
        path: &Path,
        commit: &str,
    ) -> Result<(), GitError> {
        let path = utf8(path)?;
        self.run(
            directory,
            &["worktree", "add", "-q", "--detach", path, commit],
        )
        .await?;
        Ok(())
    }

    /// Removes the worktree at `path`, whatever is uncommitted in it.
    pub async fn worktree_remove(&self, directory: &Path, path: &Path) -> Result<(), GitError> {
        let path = utf8(path)?;
        self.run(directory, &["worktree", "remove", "--force", path])
            .await?;
        Ok(())
    }

    async fn exec(
        &self,
        directory: &Path,
        index: Option<&Path>,
        config: &[(&str, &str)],
        args: &[&str],
    ) -> Result<Vec<u8>, GitError> {
        let shown = args.join(" ");
        let (tree, git_dir) = match self.target(directory)? {
            Target::Project => {
                let settings: Vec<String> = config
                    .iter()
                    .flat_map(|(key, value)| ["-c".to_string(), format!("{key}={value}")])
                    .collect();
                let mut all: Vec<&str> = settings.iter().map(String::as_str).collect();
                all.extend_from_slice(args);
                return match index {
                    Some(index) => self.git.run_on_index(directory, index, &all, None).await,
                    None => self.git.run_raw(directory, &all).await,
                };
            }
            Target::Tree { tree, git_dir } => (tree, git_dir),
        };
        let mut safe = SafeGit::new(git_dir, tree)
            .binary(self.git.binary())
            .untrusted_tree();
        if self.git.is_isolated() {
            safe = safe
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
        if let Some(index) = index {
            safe = safe.env("GIT_INDEX_FILE", index);
        }
        for (key, value) in config {
            safe = safe.config(*key, *value);
        }
        let mut command = safe
            .command(args)
            .await
            .map_err(|error| GitError::Refused {
                message: format!("{error:#}"),
            })?;
        let output = command.output().await.map_err(|error| GitError::Spawn {
            args: shown.clone(),
            message: error.to_string(),
        })?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(GitError::Failed {
                args: shown,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

fn utf8(path: &Path) -> Result<&str, GitError> {
    path.to_str().ok_or_else(|| GitError::Refused {
        message: format!("{} is not valid UTF-8", path.display()),
    })
}

/// The git directory of the linked worktree at `tree` (canonical): the one its `.git` file names,
/// when [`linked_git_dir_from_record`] trusts it under the common directory that file's path
/// implies and that directory is outside the tree.
fn validated_git_dir(tree: &Path) -> Result<PathBuf, String> {
    let refuse =
        |reason: &str| format!("{} is not a linked git worktree: {reason}", tree.display());
    let text =
        std::fs::read_to_string(tree.join(".git")).map_err(|error| refuse(&error.to_string()))?;
    let target = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| refuse("its .git names no git directory"))?;
    let worktrees = tree
        .join(target)
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .filter(|parent| parent.file_name().is_some_and(|name| name == "worktrees"))
        .ok_or_else(|| refuse("its git directory is not under worktrees/"))?;
    let common = worktrees
        .parent()
        .ok_or_else(|| refuse("its git directory has no common directory"))?;
    if common.starts_with(tree) {
        return Err(refuse("its common git directory lies inside the tree"));
    }
    linked_git_dir_from_record(tree, common).map_err(|error| format!("{error:#}"))
}
