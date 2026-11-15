//! The check both [`crate::tree_key`] and [`crate::changed_files`] make before they touch a
//! directory: it must be the top of a git working tree.

use std::path::{Path, PathBuf};

use bongos_project_repo::{Git, GitError};

/// Why a directory is not usable as a working tree.
pub(crate) enum Problem {
    /// Git says the directory is not inside a working tree, or it cannot be read.
    NotWorkTree(String),
    /// The directory is inside a working tree whose top is the given directory.
    NotTopLevel(PathBuf),
    Git(GitError),
}

/// Succeeds when `directory` is the top-level directory of a git working tree. A subdirectory
/// of a working tree is refused, because a key or a list made from one would cover only part
/// of the tree or name paths relative to the wrong directory.
pub(crate) async fn ensure_top_level(git: &Git, directory: &Path) -> Result<(), Problem> {
    match git
        .run(directory, &["rev-parse", "--is-inside-work-tree"])
        .await
    {
        Ok(answer) if answer == "true" => {}
        Ok(_) => {
            return Err(Problem::NotWorkTree(
                "not inside a working tree".to_string(),
            ));
        }
        Err(GitError::Failed { stderr, .. }) => return Err(Problem::NotWorkTree(stderr)),
        Err(error) => return Err(Problem::Git(error)),
    }
    let top = git
        .run(directory, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(Problem::Git)?;
    let top = PathBuf::from(top);
    let here = directory.canonicalize();
    let there = top.canonicalize();
    match (here, there) {
        (Ok(here), Ok(there)) if here == there => Ok(()),
        _ => Err(Problem::NotTopLevel(top)),
    }
}
