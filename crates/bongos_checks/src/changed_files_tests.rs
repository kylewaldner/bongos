use bongos_project_repo::Git;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::changed_files::{ChangedFiles, ChangedFilesError, ChangedSet, changed_files};
use crate::test_support::{commit_everything, git_in, init_git_repo, write_file};

fn git() -> Git {
    Git::locate().unwrap().isolated()
}

/// A repository with `base.txt`, `keep.txt` and an ignore rule committed on `main`, and the
/// branch `task` checked out from there.
fn fixture() -> (TempDir, std::path::PathBuf) {
    let root = TempDir::new().unwrap();
    let tree = root.path().join("work");
    std::fs::create_dir_all(&tree).unwrap();
    init_git_repo(&tree);
    write_file(&tree, "base.txt", "base\n");
    write_file(&tree, "keep.txt", "keep\n");
    write_file(&tree, ".gitignore", "build/\n");
    commit_everything(&tree, "base");
    git_in(&tree, &["checkout", "-q", "-b", "task"]);
    (root, tree)
}

fn changed(tree: &std::path::Path) -> ChangedSet {
    match block_on(changed_files(&git(), tree, Some("main"))).unwrap() {
        ChangedFiles::Changed(set) => set,
        ChangedFiles::NoBaseBranch => panic!("a base branch was given"),
    }
}

#[test]
fn changed_files_lists_committed_uncommitted_and_untracked_but_not_ignored_or_unchanged() {
    let (_root, tree) = fixture();
    write_file(&tree, "committed.kt", "c\n");
    commit_everything(&tree, "committed");
    write_file(&tree, "base.txt", "edited\n");
    write_file(&tree, "untracked.kt", "u\n");
    write_file(&tree, "build/output.bin", "ignored\n");

    let set = changed(&tree);
    assert_eq!(set.paths, ["base.txt", "committed.kt", "untracked.kt"]);
    assert_eq!(set.merge_base, git_in(&tree, &["rev-parse", "main"]));
    assert_eq!(set.as_lines(), "base.txt\ncommitted.kt\nuntracked.kt\n");
}

#[test]
fn changed_files_is_empty_when_nothing_changed() {
    let (_root, tree) = fixture();
    let set = changed(&tree);
    assert!(set.paths.is_empty());
    assert_eq!(set.as_lines(), "");
}

#[test]
fn changed_files_measures_from_the_merge_base_when_the_base_moved() {
    let (_root, tree) = fixture();
    let fork = git_in(&tree, &["rev-parse", "HEAD"]);
    write_file(&tree, "mine.kt", "m\n");
    commit_everything(&tree, "mine");
    git_in(&tree, &["checkout", "-q", "main"]);
    write_file(&tree, "theirs.kt", "t\n");
    commit_everything(&tree, "theirs");
    git_in(&tree, &["checkout", "-q", "task"]);

    let set = changed(&tree);
    assert_eq!(set.merge_base, fork);
    assert_eq!(set.paths, ["mine.kt"]);
}

#[test]
fn changed_files_lists_a_deleted_file_once_even_if_staged_and_edited() {
    let (_root, tree) = fixture();
    std::fs::remove_file(tree.join("keep.txt")).unwrap();
    write_file(&tree, "base.txt", "staged\n");
    git_in(&tree, &["add", "base.txt"]);
    write_file(&tree, "base.txt", "staged and edited\n");
    assert_eq!(changed(&tree).paths, ["base.txt", "keep.txt"]);
}

#[test]
fn changed_files_without_a_base_branch_is_no_base_branch() {
    let (_root, tree) = fixture();
    let result = block_on(changed_files(&git(), &tree, None)).unwrap();
    assert_eq!(result, ChangedFiles::NoBaseBranch);
}

#[test]
fn changed_files_with_a_missing_base_branch_is_an_error() {
    let (_root, tree) = fixture();
    let result = block_on(changed_files(&git(), &tree, Some("nope")));
    assert!(
        matches!(result, Err(ChangedFilesError::BaseBranchMissing { .. })),
        "{result:?}"
    );
    let flag = block_on(changed_files(&git(), &tree, Some("--all")));
    assert!(
        matches!(flag, Err(ChangedFilesError::InvalidBaseBranch { .. })),
        "{flag:?}"
    );
}

#[test]
fn changed_files_outside_a_git_tree_is_an_error() {
    let root = TempDir::new().unwrap();
    let result = block_on(changed_files(&git(), root.path(), Some("main")));
    assert!(
        matches!(result, Err(ChangedFilesError::NotGitTree { .. })),
        "{result:?}"
    );
}

#[test]
fn changed_files_keeps_names_with_spaces_and_newlines_whole() {
    let (_root, tree) = fixture();
    write_file(&tree, "my notes.txt", "n\n");
    write_file(&tree, "dir with space/a b.kt", "k\n");
    write_file(&tree, "line\nbreak.txt", "l\n");
    write_file(&tree, " leading.txt", "s\n");
    git_in(&tree, &["add", "my notes.txt"]);

    let set = changed(&tree);
    assert_eq!(
        set.paths,
        [
            " leading.txt",
            "dir with space/a b.kt",
            "line\nbreak.txt",
            "my notes.txt",
        ]
    );
}
