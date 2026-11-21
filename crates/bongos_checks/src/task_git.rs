//! Host git for the trees a check reads: a task's tree, a leased second tree and a temporary base
//! tree. A job may write any of them, `.git` included, so git must never discover its repository
//! from one. [`TaskGit`] runs every command through [`SafeGit`] with the git directory recorded for
//! the tree, an explicit work tree, and the protections for an untrusted tree (no hook, no filter,
//! text conversion or fsmonitor, no nested repository unless the commands stay out of it).
//!
//! A tree's git directory is trusted only as a linked worktree of a trusted repository: the
//! primary checkouts of the code repositories, a person's own and never job-written, whose common
//! git directories plain git reports. The git directory must sit directly under
//! `<trusted common>/worktrees/`, name this tree's `.git` in its `gitdir` file and name the
//! trusted common directory in its `commondir` file (see [`linked_git_dir_from_record`]). A tree
//! that no trusted repository vouches for is refused, so a look-alike `worktrees/` a job planted
//! anywhere else is never used.
//!
//! The git directory is recorded, in memory, when bongos meets the tree: a temporary base tree
//! right after `git worktree add` made it; a task's tree, a leased tree and the tree a temporary
//! tree is added from at their first command, since the engine is only handed those after they
//! exist. A record is used only while its git directory's `gitdir` file still names the tree's
//! `.git` (it does not when the tree was removed and made again); otherwise the tree is
//! validated again. After that the tree's `.git` is not read again, so rewriting it changes
//! nothing. A temporary base tree's record goes with the tree.
//!
//! The project repository and the primary checkouts are not job-written: a command there runs
//! through [`Git`] as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use bongos_confine::safe_git::{SafeGit, linked_git_dir_from_record};
use bongos_project_repo::{Git, GitError, ProjectRepo};

use crate::task_source::TaskChecks;

/// A tree's recorded git directory.
#[derive(Clone)]
struct Record {
    git_dir: PathBuf,
    /// Whether it is a linked worktree's git directory (its `gitdir` file is checked before use);
    /// false only for the plain repositories tests make.
    linked: bool,
}

/// The recorded git directory of each tree, by canonical tree path.
static RECORDS: LazyLock<Mutex<HashMap<PathBuf, Record>>> = LazyLock::new(Default::default);

/// Where a command runs.
enum Target {
    /// The project repository or a primary checkout.
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
    /// Primary checkouts of code repositories that keep the plain path too, and whose linked
    /// worktrees are trusted.
    primaries: Vec<PathBuf>,
    /// Primary checkouts of code repositories whose linked worktrees are trusted; commands in
    /// them still go through [`SafeGit`].
    trusted: Vec<PathBuf>,
    /// Whether commands may run in a tree holding a nested repository; see
    /// [`TaskGit::allowing_nested_repositories`].
    allow_nested: bool,
}

impl TaskGit {
    pub fn new(git: Git, repository: PathBuf) -> Self {
        Self {
            git,
            repository,
            primaries: Vec::new(),
            trusted: Vec::new(),
            allow_nested: false,
        }
    }

    pub fn of(project: &ProjectRepo) -> Self {
        Self::new(project.git().clone(), project.location().repository.clone())
    }

    /// Git for the trees of `task`: its project's, trusting the linked worktrees of the code
    /// repositories the task knows.
    pub(crate) fn of_task(task: &TaskChecks) -> Self {
        Self::of(&task.project).with_trusted_repositories(task.code_repositories.iter().cloned())
    }

    /// Also runs plain git in the primary checkout `directory` of a code repository, and trusts
    /// its linked worktrees.
    pub fn with_primaries(mut self, directories: impl IntoIterator<Item = PathBuf>) -> Self {
        self.primaries.extend(directories);
        self
    }

    /// Trusts the linked worktrees of the primary checkouts `directories` of code repositories.
    pub fn with_trusted_repositories(
        mut self,
        directories: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        self.trusted.extend(directories);
        self
    }

    /// Runs commands in a tree even while it holds a nested repository (a submodule, a vendored
    /// clone), for callers whose commands never start git inside one; see
    /// [`SafeGit::allow_nested_repositories`].
    pub(crate) fn allowing_nested_repositories(&self) -> Self {
        Self {
            allow_nested: true,
            ..self.clone()
        }
    }

    /// The plain runner, for the project repository only.
    pub fn project(&self) -> &Git {
        &self.git
    }

    /// The canonical common git directories of the trusted repositories that plain git reports;
    /// a repository it fails in is left out.
    async fn trusted_commons(&self) -> Vec<PathBuf> {
        let mut commons = Vec::new();
        for primary in self.primaries.iter().chain(&self.trusted) {
            let Ok(common) = self
                .git
                .run(primary, &["rev-parse", "--git-common-dir"])
                .await
            else {
                continue;
            };
            // Git reports it relative to the directory it ran in, or absolute.
            if let Ok(common) = primary.join(common).canonicalize()
                && !commons.contains(&common)
            {
                commons.push(common);
            }
        }
        commons
    }

    /// The git directory of the linked worktree at `tree`: its record when it still holds,
    /// else one validated against the trusted repositories as the module documentation says,
    /// which is recorded.
    pub async fn record_linked(&self, tree: &Path) -> Result<PathBuf, GitError> {
        let refused = |message: String| GitError::Refused { message };
        let tree = tree
            .canonicalize()
            .map_err(|error| refused(format!("{} is not usable: {error}", tree.display())))?;
        let recorded = RECORDS.lock().unwrap().get(&tree).cloned();
        if let Some(record) = recorded {
            if !record.linked || names_tree(&record.git_dir, &tree) {
                return Ok(record.git_dir);
            }
            RECORDS.lock().unwrap().remove(&tree);
        }
        let commons = self.trusted_commons().await;
        let record = match validated_git_dir(&tree, &commons) {
            Ok(git_dir) => Record {
                git_dir,
                linked: true,
            },
            // Tests build trees with plain `git init`; production trees are linked worktrees.
            #[cfg(test)]
            Err(_) if tree.join(".git").is_dir() => Record {
                git_dir: tree.join(".git"),
                linked: false,
            },
            Err(message) => return Err(refused(message)),
        };
        RECORDS.lock().unwrap().insert(tree, record.clone());
        Ok(record.git_dir)
    }

    /// Drops the record of the tree at `tree`, which is gone or about to be.
    pub(crate) fn forget(tree: &Path) {
        let mut records = RECORDS.lock().unwrap();
        records.remove(tree);
        if let Ok(canonical) = tree.canonicalize() {
            records.remove(&canonical);
        }
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
    pub async fn check_tree(&self, directory: &Path) -> Result<(), GitError> {
        self.target(directory).await.map(|_| ())
    }

    async fn target(&self, directory: &Path) -> Result<Target, GitError> {
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
        let git_dir = self.record_linked(&canonical).await?;
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
        let (tree, git_dir) = match self.target(directory).await? {
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
        if self.allow_nested {
            safe = safe.allow_nested_repositories();
        }
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

/// Whether the `gitdir` file of the linked git directory `git_dir` still names the `.git` of
/// `tree` (canonical). The `.git` itself is not followed: only its parent is resolved.
fn names_tree(git_dir: &Path, tree: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) else {
        return false;
    };
    let named = git_dir.join(text.trim_end_matches(['\n', '\r']));
    named.file_name().is_some_and(|name| name == ".git")
        && named
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent == tree)
}

/// The git directory of the linked worktree at `tree` (canonical) under one of the trusted
/// common directories `commons` (canonical), when [`linked_git_dir_from_record`] trusts it there.
fn validated_git_dir(tree: &Path, commons: &[PathBuf]) -> Result<PathBuf, String> {
    let mut reasons = Vec::new();
    for common in commons {
        if common.starts_with(tree) {
            continue;
        }
        match linked_git_dir_from_record(tree, common) {
            Ok(git_dir) => return Ok(git_dir),
            Err(error) => reasons.push(format!("{error:#}")),
        }
    }
    if reasons.is_empty() {
        reasons.push("no trusted repository is known for it".to_string());
    }
    Err(format!(
        "{} is not a linked worktree of a trusted repository: {}",
        tree.display(),
        reasons.join("; ")
    ))
}
