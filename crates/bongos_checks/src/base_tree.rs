//! The trees a base run uses: a second tree a lane lends, checked out at the base commit, or a
//! temporary tree added under the scratch folder and removed when the run ends. A group's
//! closing checks use a lent tree too, with a recorded tree object written out in it.
//!
//! Nothing here ever runs `git checkout --force`, `git clean` or a removal in the task's own
//! tree. A lent tree is used only when it is apart from every protected tree (not the same
//! directory, not inside one and not holding one), and a removal only ever deletes a directory
//! directly inside the scratch folder whose name is a UUID, never through a symbolic link.
//!
//! A temporary tree is `<scratch>/<uuid>`, with `<scratch>/<uuid>.owner` naming the process that
//! made it (its pid and start time). At the next load, [`sweep_scratch`] removes every such tree
//! whose owner file names a process that is gone: what a crash left behind. A tree without an
//! owner file is not known to be one of these, so it stays.

use std::path::{Path, PathBuf};

use bongos_project_repo::Git;

use crate::process::StopHandle;
use crate::tree_key::private_index;
use crate::work_tree::ensure_top_level;

/// The suffix of the file beside a temporary tree that names the process that made it.
const OWNER_SUFFIX: &str = ".owner";

/// A temporary tree at a base commit, removed with `git worktree remove --force` by
/// [`TempTree::remove`], or on a thread of its own when dropped without it.
pub(crate) struct TempTree {
    git: Git,
    /// A working tree of the repository the temporary tree was added to.
    repository: PathBuf,
    /// The scratch folder, canonical.
    scratch: PathBuf,
    path: PathBuf,
    /// Where the tree's private index for its key goes.
    index_dir: PathBuf,
    armed: bool,
}

impl TempTree {
    /// Adds a detached worktree of the repository of `repository` at `commit` as
    /// `<scratch>/<uuid>`. Refused when the new tree would not be apart from `protected`.
    pub(crate) async fn create(
        git: &Git,
        repository: &Path,
        scratch: &Path,
        commit: &str,
        index_dir: &Path,
        protected: &[&Path],
    ) -> Result<Self, String> {
        ensure_commit(commit)?;
        std::fs::create_dir_all(scratch)
            .map_err(|error| format!("creating the scratch folder: {error}"))?;
        let scratch = scratch
            .canonicalize()
            .map_err(|error| format!("resolving the scratch folder: {error}"))?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = scratch.join(&id);
        ensure_apart(&path, protected)?;
        write_owner(&scratch, &id)?;
        let tree = Self {
            git: git.clone(),
            repository: repository.to_path_buf(),
            scratch,
            path,
            index_dir: index_dir.to_path_buf(),
            armed: true,
        };
        if let Err(error) = git
            .worktree_add_detached(repository, &tree.path, commit)
            .await
        {
            tree.remove().await;
            return Err(format!("adding a temporary base tree: {error}"));
        }
        Ok(tree)
    }

    /// Writes out the files of the tree object `tree` of the repository at `repository` as the
    /// new folder `<scratch>/<uuid>`: the tree is read into a private index and checked out
    /// from it with `checkout-index --all --prefix`. No worktree is registered, no ref moves and
    /// the repository's own index and files are not touched. The folder is not a git working
    /// tree; it is removed like a temporary base tree.
    pub(crate) async fn materialize(
        git: &Git,
        repository: &Path,
        scratch: &Path,
        tree: &str,
        index_dir: &Path,
    ) -> Result<Self, String> {
        ensure_commit(tree).map_err(|_| format!("{tree:?} is not a tree id"))?;
        std::fs::create_dir_all(scratch)
            .map_err(|error| format!("creating the scratch folder: {error}"))?;
        let scratch = scratch
            .canonicalize()
            .map_err(|error| format!("resolving the scratch folder: {error}"))?;
        std::fs::create_dir_all(index_dir)
            .map_err(|error| format!("creating the index folder: {error}"))?;
        let index_dir = std::path::absolute(index_dir)
            .map_err(|error| format!("resolving the index folder: {error}"))?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = scratch.join(&id);
        write_owner(&scratch, &id)?;
        let made = Self {
            git: git.clone(),
            repository: repository.to_path_buf(),
            scratch,
            path,
            index_dir,
            armed: true,
        };
        if let Err(error) = made.write_out(tree).await {
            made.remove().await;
            return Err(format!("writing out the tree {tree}: {error}"));
        }
        Ok(made)
    }

    async fn write_out(&self, tree: &str) -> Result<(), String> {
        std::fs::create_dir(&self.path).map_err(|error| error.to_string())?;
        let index = private_index(&self.path, &self.index_dir);
        let prefix = self
            .path
            .to_str()
            .ok_or_else(|| "the scratch folder's path is not UTF-8".to_string())?;
        let prefix = format!("--prefix={prefix}/");
        self.git
            .run_on_index(&self.repository, &index, &["read-tree", tree], None)
            .await
            .map_err(|error| error.to_string())?;
        self.git
            .run_on_index(
                &self.repository,
                &index,
                &["checkout-index", "--all", "--force", &prefix],
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the tree, its owner file and its private index.
    pub(crate) async fn remove(mut self) {
        self.armed = false;
        remove_temp_tree(
            &self.git,
            Some(&self.repository),
            &self.scratch,
            &self.path,
            &self.index_dir,
        )
        .await;
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let git = self.git.clone();
        let repository = self.repository.clone();
        let scratch = self.scratch.clone();
        let path = self.path.clone();
        let index_dir = self.index_dir.clone();
        let spawned = std::thread::Builder::new()
            .name("checks-scratch".to_string())
            .spawn(move || {
                futures::executor::block_on(remove_temp_tree(
                    &git,
                    Some(&repository),
                    &scratch,
                    &path,
                    &index_dir,
                ));
            });
        if let Err(error) = spawned {
            log::error!(
                "checks: removing the temporary tree {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Removes the temporary tree at `path` with git (from `repository`, else from inside the tree
/// itself), then whatever git left of it, then its owner file and its private index; the owner
/// file and the index go too when the tree was never made. Never touches anything but a
/// UUID-named directory directly inside `scratch`, and never prunes the repository's other
/// worktrees: a registration a failed removal leaves is only marked prunable.
async fn remove_temp_tree(
    git: &Git,
    repository: Option<&Path>,
    scratch: &Path,
    path: &Path,
    index_dir: &Path,
) {
    let absent = std::fs::symlink_metadata(path).is_err();
    if !is_scratch_name(scratch, path) || !(absent || is_scratch_tree(scratch, path)) {
        log::error!(
            "checks: refusing to remove {}, which is not a temporary tree",
            path.display()
        );
        return;
    }
    if !absent {
        if path.join(".git").is_file()
            && let Some(text) = path.to_str()
        {
            let directory = repository.unwrap_or(path);
            if let Err(error) = git
                .run(directory, &["worktree", "remove", "--force", text])
                .await
            {
                log::warn!("checks: git removing {}: {error}", path.display());
            }
        }
        if std::fs::symlink_metadata(path).is_ok()
            && let Err(error) = std::fs::remove_dir_all(path)
        {
            log::error!("checks: removing {}: {error}", path.display());
        }
    }
    if let Some(owner) = owner_file(scratch, path) {
        std::fs::remove_file(owner).ok();
    }
    std::fs::remove_file(private_index(path, index_dir)).ok();
}

/// Whether `path` is directly inside `scratch` and named by a UUID.
fn is_scratch_name(scratch: &Path, path: &Path) -> bool {
    path.parent() == Some(scratch)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok())
}

/// Whether `path` is a directory (not a symbolic link) directly inside `scratch` named by a UUID.
fn is_scratch_tree(scratch: &Path, path: &Path) -> bool {
    is_scratch_name(scratch, path)
        && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

fn owner_file(scratch: &Path, path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(scratch.join(format!("{name}{OWNER_SUFFIX}")))
}

/// Writes `<scratch>/<id>.owner`: this process's pid and start time.
fn write_owner(scratch: &Path, id: &str) -> Result<(), String> {
    let pid = std::process::id();
    let start = bongos_workflow::process::start_time(pid).unwrap_or(0);
    std::fs::write(
        scratch.join(format!("{id}{OWNER_SUFFIX}")),
        format!("{pid} {start}\n"),
    )
    .map_err(|error| format!("writing the owner of a temporary tree: {error}"))
}

/// What an owner file says of the process that made its tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The same pid runs with the same start time.
    Alive,
    /// It names a process that is gone.
    Gone,
    /// The file is missing, unreadable or names no process.
    Unknown,
}

fn owner_of(owner: &Path) -> Owner {
    let Ok(text) = std::fs::read_to_string(owner) else {
        return Owner::Unknown;
    };
    let mut fields = text.split_whitespace();
    let (Some(Ok(pid)), Some(Ok(start))) = (
        fields.next().map(str::parse::<u32>),
        fields.next().map(str::parse::<u64>),
    ) else {
        return Owner::Unknown;
    };
    if pid == std::process::id()
        || (start != 0 && bongos_workflow::process::start_time(pid) == Some(start))
    {
        Owner::Alive
    } else {
        Owner::Gone
    }
}

/// Removes every temporary tree in `scratch` whose owner file names a process that is gone,
/// with its owner file, and every owner file left without its tree. A tree without a readable
/// owner file stays. Returns the trees removed.
pub(crate) async fn sweep_scratch(git: &Git, scratch: &Path, index_dir: &Path) -> Vec<PathBuf> {
    let Ok(scratch) = scratch.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&scratch) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    let mut owners = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && let Some(id) = name.strip_suffix(OWNER_SUFFIX)
            && uuid::Uuid::parse_str(id).is_ok()
        {
            owners.push((path.clone(), scratch.join(id)));
            continue;
        }
        if !is_scratch_tree(&scratch, &path) {
            continue;
        }
        if owner_file(&scratch, &path).map(|owner| owner_of(&owner)) != Some(Owner::Gone) {
            continue;
        }
        remove_temp_tree(git, None, &scratch, &path, index_dir).await;
        removed.push(path);
    }
    for (owner, tree) in owners {
        if !tree.exists() && owner_of(&owner) != Owner::Alive {
            std::fs::remove_file(owner).ok();
        }
    }
    removed
}

/// Why a lent tree was not made ready.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LentTreeError {
    /// `stop` was stopped after the checkout: the tree is at the commit but not cleaned.
    Stopped,
    Failed(String),
}

/// Checks out `commit` detached in the lent tree at `path`, dropping its changes, and removes
/// its untracked files that are not ignored (build output stays). Refused when `path` is not
/// apart from `protected` or not the top of a working tree. Once `stop` is stopped, no further
/// git command runs in the tree.
pub(crate) async fn check_out_lent_tree(
    git: &Git,
    path: &Path,
    commit: &str,
    protected: &[&Path],
    stop: &StopHandle,
) -> Result<(), LentTreeError> {
    let failed = LentTreeError::Failed;
    if stop.is_stopped() {
        return Err(LentTreeError::Stopped);
    }
    ensure_commit(commit).map_err(failed)?;
    let canonical = path.canonicalize().map_err(|error| {
        failed(format!(
            "the lent tree {} is not usable: {error}",
            path.display()
        ))
    })?;
    ensure_apart(&canonical, protected).map_err(failed)?;
    ensure_top_level(git, &canonical).await.map_err(|_| {
        failed(format!(
            "the lent tree {} is not the top of a working tree",
            path.display()
        ))
    })?;
    if stop.is_stopped() {
        return Err(LentTreeError::Stopped);
    }
    git.run(&canonical, &["checkout", "--detach", "--force", commit])
        .await
        .map_err(|error| failed(format!("checking out the base in the lent tree: {error}")))?;
    if stop.is_stopped() {
        return Err(LentTreeError::Stopped);
    }
    git.run(&canonical, &["clean", "-f", "-d"])
        .await
        .map_err(|error| failed(format!("cleaning the lent tree: {error}")))?;
    Ok(())
}

/// Writes the files of the tree object `tree` out in the lent tree at `path`: the tree is read
/// into the lent tree's own index and every file of it checked out over what is there, then the
/// untracked files that are not ignored are removed (build output stays). `HEAD` does not move.
/// Refused when `path` is not apart from `protected`, not the top of a working tree, or its
/// repository does not hold the tree. Every git command runs in the canonical `path` with its
/// arguments passed as they are; nothing outside the lent tree is written.
pub(crate) async fn check_out_tree_in_lent_tree(
    git: &Git,
    path: &Path,
    tree: &str,
    protected: &[&Path],
) -> Result<(), String> {
    ensure_commit(tree).map_err(|_| format!("{tree:?} is not a tree id"))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("the lent tree {} is not usable: {error}", path.display()))?;
    ensure_apart(&canonical, protected)?;
    ensure_top_level(git, &canonical).await.map_err(|_| {
        format!(
            "the lent tree {} is not the top of a working tree",
            path.display()
        )
    })?;
    let kind = git
        .run(&canonical, &["cat-file", "-t", tree])
        .await
        .map_err(|error| format!("the lent tree's repository lacks the tree {tree}: {error}"))?;
    if kind != "tree" {
        return Err(format!(
            "{tree} is not a tree in the lent tree's repository"
        ));
    }
    git.run(&canonical, &["read-tree", tree])
        .await
        .map_err(|error| format!("reading the tree into the lent tree: {error}"))?;
    git.run(&canonical, &["checkout-index", "--force", "--all"])
        .await
        .map_err(|error| format!("writing out the tree in the lent tree: {error}"))?;
    git.run(&canonical, &["clean", "-f", "-d"])
        .await
        .map_err(|error| format!("cleaning the lent tree: {error}"))?;
    Ok(())
}

/// Refuses a full commit id that is not hex, so no argument can read as an option.
fn ensure_commit(commit: &str) -> Result<(), String> {
    if commit.len() >= 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("{commit:?} is not a commit id"))
    }
}

/// Succeeds when the canonical `path` is not one of `protected`, not inside one and does not
/// hold one. A protected path that does not exist protects nothing.
pub(crate) fn ensure_apart(path: &Path, protected: &[&Path]) -> Result<(), String> {
    for tree in protected {
        let Ok(tree) = tree.canonicalize() else {
            continue;
        };
        if path.starts_with(&tree) || tree.starts_with(path) {
            return Err(format!(
                "{} overlaps the task's tree {}; the task's tree is never used for a base run",
                path.display(),
                tree.display()
            ));
        }
    }
    Ok(())
}
