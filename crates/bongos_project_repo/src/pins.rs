//! Materialized trees: a read-only copy of a tree's files under `state/pins/`, written from the
//! objects (never from a checkout), for the checks and runs that read files. A [`PinGuard`]
//! removes its copy when dropped; the landing engine keeps it until every check reading it has
//! completed. Copies a crash left behind are removed when the project opens.

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
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            log::error!("removing the pinned tree {}: {error}", self.path.display());
        }
    }
}

impl ProjectRepo {
    /// Writes the files of `treeish` (a commit or a tree) read-only into a fresh folder under
    /// `state/pins/`, scripts with their executable bit.
    pub async fn materialize(&self, treeish: &str) -> Result<PinGuard> {
        let repository = &self.location.repository;
        let tree = self.git.rev_parse(repository, &format!("{treeish}^{{tree}}")).await?;
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
        Ok(guard)
    }

    /// Removes the materialized trees a former process left behind.
    pub(crate) fn sweep_pins(&self) {
        let pins = self.location.state.join(PINS_DIR);
        if pins.exists()
            && let Err(error) = std::fs::remove_dir_all(&pins)
        {
            log::error!("removing {}: {error}", pins.display());
        }
    }
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
