//! The create rule for lane trees and directories: only the components below an existing,
//! writable root are ever made, so a drive that is not mounted is never mistaken for an empty
//! directory and gets nothing made in its place.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CreateError {
    #[error("{} is not inside its root {}", path.display(), root.display())]
    NotInside { path: PathBuf, root: PathBuf },
    #[error("{} is missing", root.display())]
    RootMissing { root: PathBuf },
    #[error("{} is not a directory", root.display())]
    RootNotDirectory { root: PathBuf },
    #[error("{message}")]
    RootNotWritable { root: PathBuf, message: String },
    #[error("{} exists and is not a directory", path.display())]
    NotADirectory { path: PathBuf },
    #[error("creating {}: {message}", path.display())]
    Create { path: PathBuf, message: String },
}

/// `path` with `.` components dropped and each `..` taking off the component before it, worked
/// out on the text alone: nothing has to exist and no link is followed. A trailing separator
/// makes no difference.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// Checks that `root` exists, is a directory and takes a new file (a `create_new` probe that is
/// removed at once).
pub fn check_root(root: &Path) -> Result<(), CreateError> {
    let metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(CreateError::RootMissing {
                root: root.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(CreateError::RootNotWritable {
                root: root.to_path_buf(),
                message: format!("{}: {error}", root.display()),
            });
        }
    };
    if !metadata.is_dir() {
        return Err(CreateError::RootNotDirectory {
            root: root.to_path_buf(),
        });
    }
    bongos_work::probe_writable(root).map_err(|error| CreateError::RootNotWritable {
        root: root.to_path_buf(),
        message: format!("{error:#}"),
    })
}

/// Makes the directories of `path` that are below `root` and missing, one component at a time.
/// `path` must be inside `root` (or be it), and `root` must exist, be a directory and be
/// writable; `root` and everything above it are never made.
pub fn create_below_existing(path: &Path, root: &Path) -> Result<(), CreateError> {
    let path = lexical(path);
    let root = lexical(root);
    let Ok(below) = path.strip_prefix(&root) else {
        return Err(CreateError::NotInside { path, root });
    };
    check_root(&root)?;
    let mut current = root.clone();
    for component in below.components() {
        current.push(component);
        match std::fs::metadata(&current) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => return Err(CreateError::NotADirectory { path: current }),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CreateError::Create {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists && current.is_dir() => {}
            Err(error) => {
                return Err(CreateError::Create {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}
