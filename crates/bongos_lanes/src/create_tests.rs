//! The create rule: only components below an existing, writable root are made.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::create::{CreateError, check_root, create_below_existing, lexical};

/// Every path below `dir`, relative to it, sorted.
pub(crate) fn listing(dir: &Path) -> Vec<PathBuf> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(base).unwrap().to_path_buf());
            if path.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Whether a file can be made in `dir` (a privileged user ignores the mode bits).
pub(crate) fn writable(dir: &Path) -> bool {
    let probe = dir.join("probe");
    let made = std::fs::write(&probe, "").is_ok();
    std::fs::remove_file(&probe).ok();
    made
}

pub(crate) fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn lexical_normalizes_dots_and_trailing_separators() {
    assert_eq!(lexical(Path::new("/x/target/")), PathBuf::from("/x/target"));
    assert_eq!(
        lexical(Path::new("/x/./a/../target")),
        PathBuf::from("/x/target")
    );
    assert_eq!(
        lexical(Path::new("/x//target/.")),
        PathBuf::from("/x/target")
    );
    assert_eq!(lexical(Path::new("/../x")), PathBuf::from("/x"));
    assert_eq!(lexical(Path::new("a/../../b")), PathBuf::from("../b"));
}

#[test]
fn only_components_below_the_root_are_made() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("drive");
    std::fs::create_dir(&root).unwrap();
    create_below_existing(&root.join("lanes/build-1/target"), &root).unwrap();
    assert_eq!(
        listing(&root),
        [
            PathBuf::from("lanes"),
            PathBuf::from("lanes/build-1"),
            PathBuf::from("lanes/build-1/target"),
        ],
        "no probe file is left behind"
    );
    // Again: everything exists, nothing changes.
    create_below_existing(&root.join("lanes/build-1/target"), &root).unwrap();
    assert_eq!(listing(&root).len(), 3);
}

#[test]
fn a_missing_root_makes_nothing() {
    let temp = TempDir::new().unwrap();
    let before = listing(temp.path());
    let root = temp.path().join("unmounted");
    let error = create_below_existing(&root.join("target"), &root).unwrap_err();
    assert_eq!(error, CreateError::RootMissing { root: root.clone() });
    assert_eq!(error.to_string(), format!("{} is missing", root.display()));
    assert_eq!(listing(temp.path()), before);
}

#[test]
fn a_root_that_is_a_file_makes_nothing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("file");
    std::fs::write(&root, "").unwrap();
    let before = listing(temp.path());
    let error = create_below_existing(&root.join("target"), &root).unwrap_err();
    assert_eq!(error, CreateError::RootNotDirectory { root });
    assert_eq!(listing(temp.path()), before);
}

#[test]
fn a_path_outside_its_root_is_refused() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("drive");
    std::fs::create_dir(&root).unwrap();
    let outside = temp.path().join("drive/../elsewhere/target");
    let error = create_below_existing(&outside, &root).unwrap_err();
    assert!(matches!(error, CreateError::NotInside { .. }), "{error:?}");
    let sibling = temp.path().join("drive-2/target");
    let error = create_below_existing(&sibling, &root).unwrap_err();
    assert!(matches!(error, CreateError::NotInside { .. }), "{error:?}");
    assert_eq!(listing(temp.path()), [PathBuf::from("drive")]);
}

#[test]
fn an_unwritable_root_makes_nothing_and_leaves_no_probe() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("drive");
    std::fs::create_dir(&root).unwrap();
    set_mode(&root, 0o555);
    if writable(&root) {
        set_mode(&root, 0o755);
        eprintln!("skipped: the mode bits do not stop this user from writing");
        return;
    }
    let error = create_below_existing(&root.join("target"), &root).unwrap_err();
    set_mode(&root, 0o755);
    assert!(
        matches!(&error, CreateError::RootNotWritable { root: found, .. } if *found == root),
        "{error:?}"
    );
    assert!(error.to_string().contains("is not writable"), "{error}");
    assert_eq!(listing(&root), Vec::<PathBuf>::new());
}

#[test]
fn the_root_check_removes_its_probe_file() {
    let temp = TempDir::new().unwrap();
    check_root(temp.path()).unwrap();
    assert_eq!(listing(temp.path()), Vec::<PathBuf>::new());
}
