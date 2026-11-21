//! The check both [`crate::tree_key`] and [`crate::changed_files`] make before they touch a
//! directory: it must be the top of a git working tree.

use std::path::{Path, PathBuf};

use crate::task_git::TaskGit;

/// Why a directory is not usable as a working tree.
pub(crate) enum Problem {
    /// Git says the directory is not inside a working tree, or it cannot be read.
    NotWorkTree(String),
    /// The directory is inside a working tree whose top is the given directory.
    NotTopLevel(PathBuf),
}

/// Succeeds when `directory` is the top-level directory of a git working tree. A subdirectory
/// of a working tree is refused, because a key or a list made from one would cover only part
/// of the tree or name paths relative to the wrong directory.
pub(crate) async fn ensure_top_level(git: &TaskGit, directory: &Path) -> Result<(), Problem> {
    // A tree is the top of its working tree when its git directory is recorded for it; a
    // directory below a tree has no record of its own.
    match git.check_tree(directory).await {
        Ok(()) => Ok(()),
        Err(error) => match TaskGit::top_above(directory) {
            Some(top) => Err(Problem::NotTopLevel(top)),
            None => Err(Problem::NotWorkTree(error.to_string())),
        },
    }
}
