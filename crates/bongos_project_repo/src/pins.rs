//! Materialized trees: a read-only copy of a tree's files under `state/pins/`, written from the
//! objects (never from a checkout), for the checks and runs that read files. A [`PinGuard`]
//! removes its copy when dropped; the landing engine keeps it until every check reading it has
//! completed. Copies a crash left behind are removed when the project opens. On Unix the folders
//! are read-only too, so a script reading the copy cannot add, remove or replace a file in it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use bongos_project_repo_model::layout::PINS_DIR;

use crate::create::ProjectRepo;

/// A materialized tree, removed on drop.
#[derive(Debug)]
pub struct PinGuard {
    path: PathBuf,
}

impl PinGuard {
    /// The root of the tree: `definitions/` is inside it.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        if let Err(error) = remove_read_only_tree(&self.path) {
            log::error!("removing the pinned tree {}: {error}", self.path.display());
        }
    }
}

impl ProjectRepo {
    /// Writes the files of `treeish` (a commit or a tree) read-only into a fresh folder under
    /// `state/pins/`, scripts with their executable bit.
    pub async fn materialize(&self, treeish: &str) -> Result<PinGuard> {
        let repository = &self.location.repository;
        let tree = self
            .git
            .rev_parse(repository, &format!("{treeish}^{{tree}}"))
            .await?;
        let entries: Vec<_> = self
            .git
            .ls_tree_r(repository, &tree)
            .await?
            .into_iter()
            .filter(|entry| entry.kind == "blob" && entry.mode != "120000")
            .collect();
        let objects: Vec<String> = entries.iter().map(|entry| entry.object.clone()).collect();
        let blobs = self.git.cat_file_batch(repository, &objects).await?;
        let short = &tree[..tree.len().min(12)];
        let path = self
            .location
            .state
            .join(PINS_DIR)
            .join(format!("{short}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        // From here the guard removes what was written, whatever fails.
        let guard = PinGuard { path };
        for (entry, blob) in entries.iter().zip(blobs) {
            let Some(bytes) = blob else {
                continue;
            };
            let file = guard.path.join(&entry.path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&file, bytes).with_context(|| format!("writing {}", file.display()))?;
            set_read_only(&file, entry.mode == "100755")?;
        }
        set_folders(&guard.path, FolderMode::ReadOnly)?;
        Ok(guard)
    }

    /// Removes the materialized trees a former process left behind.
    pub(crate) fn sweep_pins(&self) {
        let pins = self.location.state.join(PINS_DIR);
        if pins.exists()
            && let Err(error) = remove_read_only_tree(&pins)
        {
            log::error!("removing {}: {error}", pins.display());
        }
    }
}

/// Removes a tree whose folders [`set_folders`] made read-only. When some folder cannot be made
/// writable, the rest of the tree is still removed.
fn remove_read_only_tree(path: &Path) -> Result<()> {
    if let Err(error) = set_folders(path, FolderMode::Writable) {
        log::warn!("making {} writable: {error:#}", path.display());
    }
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderMode {
    ReadOnly,
    Writable,
}

/// Sets the mode of every folder under `root` (itself included). Symbolic links are not
/// followed. A writable folder is made so before its contents are visited, a read-only one after.
#[cfg(unix)]
fn set_folders(root: &Path, mode: FolderMode) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let bits = match mode {
        FolderMode::ReadOnly => 0o555,
        FolderMode::Writable => 0o755,
    };
    let set = |path: &Path| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .with_context(|| format!("setting the mode of {}", path.display()))
    };
    let metadata =
        std::fs::symlink_metadata(root).with_context(|| format!("reading {}", root.display()))?;
    if !metadata.is_dir() {
        return Ok(());
    }
    if mode == FolderMode::Writable {
        set(root)?;
    }
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        if entry.file_type()?.is_dir() {
            set_folders(&entry.path(), mode)?;
        }
    }
    if mode == FolderMode::ReadOnly {
        set(root)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_folders(_root: &Path, _mode: FolderMode) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_read_only(file: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o555 } else { 0o444 };
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("making {} read-only", file.display()))
}

#[cfg(not(unix))]
fn set_read_only(file: &Path, _executable: bool) -> Result<()> {
    let mut permissions = std::fs::metadata(file)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(file, permissions)
        .with_context(|| format!("making {} read-only", file.display()))
}
