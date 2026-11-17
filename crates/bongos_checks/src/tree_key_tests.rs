use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use bongos_project_repo::Git;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::test_support::{
    HOSTILE_CHILD, commit_everything, git_in, init_git_repo, rerun_with_hostile_git_env, write_file,
};
use crate::tree_key::{TreeError, TreeKey, combine_trees, key_inputs_digest, tree_key};

fn git() -> Git {
    Git::locate().unwrap().isolated()
}

struct Fixture {
    _root: TempDir,
    tree: PathBuf,
    indexes: PathBuf,
}

fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let tree = root.path().join("work");
    std::fs::create_dir_all(&tree).unwrap();
    init_git_repo(&tree);
    write_file(&tree, "src/main.kt", "fun main() {}\n");
    write_file(&tree, "README.md", "hello\n");
    commit_everything(&tree, "first");
    Fixture {
        indexes: root.path().join("indexes"),
        tree,
        _root: root,
    }
}

fn key_of(fixture: &Fixture) -> TreeKey {
    block_on(tree_key(&git(), &fixture.tree, &fixture.indexes)).unwrap()
}

#[test]
fn tree_key_is_the_same_for_the_same_content_on_two_commits() {
    let fixture = fixture();
    let before = key_of(&fixture);
    git_in(
        &fixture.tree,
        &["commit", "-q", "--allow-empty", "-m", "second"],
    );
    assert_eq!(key_of(&fixture), before);
    assert_eq!(
        before.as_str(),
        git_in(&fixture.tree, &["rev-parse", "HEAD^{tree}"])
    );
}

#[test]
fn tree_key_changes_on_an_edit_and_a_revert_restores_it() {
    let fixture = fixture();
    let original = key_of(&fixture);
    write_file(&fixture.tree, "README.md", "changed\n");
    let edited = key_of(&fixture);
    assert_ne!(edited, original);
    write_file(&fixture.tree, "README.md", "hello\n");
    assert_eq!(key_of(&fixture), original);
}

#[test]
fn tree_key_counts_untracked_files_and_skips_ignored_ones() {
    let fixture = fixture();
    write_file(&fixture.tree, ".gitignore", "build/\n");
    commit_everything(&fixture.tree, "ignore build");
    let clean = key_of(&fixture);

    write_file(&fixture.tree, "build/output.bin", "generated\n");
    assert_eq!(key_of(&fixture), clean);

    write_file(&fixture.tree, "notes.txt", "new\n");
    let with_untracked = key_of(&fixture);
    assert_ne!(with_untracked, clean);

    std::fs::remove_file(fixture.tree.join("notes.txt")).unwrap();
    assert_eq!(key_of(&fixture), clean);
}

#[test]
fn tree_key_changes_when_a_rebase_moves_the_base() {
    let fixture = fixture();
    git_in(&fixture.tree, &["checkout", "-q", "-b", "task"]);
    write_file(&fixture.tree, "feature.kt", "val x = 1\n");
    commit_everything(&fixture.tree, "feature");
    let before = key_of(&fixture);

    git_in(&fixture.tree, &["checkout", "-q", "main"]);
    write_file(&fixture.tree, "other.kt", "val y = 2\n");
    commit_everything(&fixture.tree, "base moved");
    git_in(&fixture.tree, &["checkout", "-q", "task"]);
    git_in(&fixture.tree, &["rebase", "-q", "main"]);

    assert_ne!(key_of(&fixture), before);
}

#[test]
fn tree_key_leaves_the_real_index_untouched() {
    let fixture = fixture();
    write_file(&fixture.tree, "untracked.txt", "x\n");
    write_file(&fixture.tree, "README.md", "edited\n");
    let index = fixture.tree.join(".git/index");
    let bytes = std::fs::read(&index).unwrap();
    let modified = std::fs::metadata(&index).unwrap().modified().unwrap();

    key_of(&fixture);
    key_of(&fixture);

    assert_eq!(std::fs::read(&index).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        modified
    );
    // The untracked file is still untracked in the real index.
    assert!(
        git_in(&fixture.tree, &["status", "--porcelain"]).contains("?? untracked.txt"),
        "the file must stay untracked"
    );
    assert_eq!(std::fs::read_dir(&fixture.indexes).unwrap().count(), 1);
}

#[test]
fn tree_key_changes_with_the_executable_bit() {
    let fixture = fixture();
    let before = key_of(&fixture);
    let script = fixture.tree.join("README.md");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(key_of(&fixture), before);
}

#[test]
fn tree_key_of_a_directory_that_is_not_a_git_tree_is_an_error() {
    let root = TempDir::new().unwrap();
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    write_file(&plain, "a.txt", "a\n");
    let result = block_on(tree_key(&git(), &plain, &root.path().join("indexes")));
    assert!(
        matches!(result, Err(TreeError::NotGitTree { .. })),
        "{result:?}"
    );

    let missing = block_on(tree_key(
        &git(),
        &root.path().join("missing"),
        &root.path().join("indexes"),
    ));
    assert!(
        matches!(missing, Err(TreeError::NotGitTree { .. })),
        "{missing:?}"
    );
}

#[test]
fn tree_key_is_an_error_when_a_file_cannot_be_read() {
    let fixture = fixture();
    let secret = fixture.tree.join("secret.txt");
    write_file(&fixture.tree, "secret.txt", "hidden\n");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&secret).is_ok() {
        // Running as a user that ignores file modes (root): nothing to test.
        return;
    }
    let result = block_on(tree_key(&git(), &fixture.tree, &fixture.indexes));
    assert!(matches!(result, Err(TreeError::Git(_))), "{result:?}");
}

#[test]
fn tree_key_inputs_digest_changes_with_bytes_and_tells_missing_from_empty() {
    let root = TempDir::new().unwrap();
    let lock = root.path().join("lock");
    let pin = root.path().join("pin");
    let absent = root.path().join("absent");
    std::fs::write(&lock, "one").unwrap();
    std::fs::write(&pin, "").unwrap();

    let first = key_inputs_digest(&[lock.clone(), pin.clone()]).unwrap();
    assert_eq!(
        first,
        key_inputs_digest(&[lock.clone(), pin.clone()]).unwrap()
    );

    std::fs::write(&lock, "two").unwrap();
    let changed = key_inputs_digest(&[lock.clone(), pin.clone()]).unwrap();
    assert_ne!(changed, first);

    let empty = key_inputs_digest(std::slice::from_ref(&pin)).unwrap();
    let missing = key_inputs_digest(&[absent]).unwrap();
    assert_ne!(empty, missing);
    assert_ne!(
        key_inputs_digest(&[lock.clone(), pin.clone()]).unwrap(),
        key_inputs_digest(&[pin, lock]).unwrap(),
        "the given order is part of the digest"
    );
    // A directory is an error, not "missing".
    assert!(key_inputs_digest(&[root.path().to_path_buf()]).is_err());
    // No inputs is a valid digest.
    assert_eq!(key_inputs_digest(&[]).unwrap().len(), 64);
}

#[test]
fn tree_key_combined_over_two_repositories_ignores_order_and_follows_each_tree() {
    let app = TreeKey("a".repeat(40));
    let lib = TreeKey("b".repeat(40));
    let forward = combine_trees(&[("app", app.clone()), ("lib", lib.clone())]);
    let backward = combine_trees(&[("lib", lib.clone()), ("app", app.clone())]);
    assert_eq!(forward, backward);
    assert_eq!(forward.as_str().len(), 64);

    let app_changed = combine_trees(&[("app", TreeKey("c".repeat(40))), ("lib", lib.clone())]);
    let lib_changed = combine_trees(&[("app", app.clone()), ("lib", TreeKey("c".repeat(40)))]);
    assert_ne!(app_changed, forward);
    assert_ne!(lib_changed, forward);
    assert_ne!(app_changed, lib_changed);

    // The same trees under swapped names are another key.
    assert_ne!(
        combine_trees(&[("app", lib), ("lib", app.clone())]),
        forward
    );
    assert_ne!(combine_trees(&[("app", app)]), forward);
}

#[test]
fn tree_key_ignores_the_executable_bit_setting_of_the_repository() {
    let fixture = fixture();
    git_in(&fixture.tree, &["config", "core.fileMode", "false"]);
    let before = key_of(&fixture);
    let script = fixture.tree.join("README.md");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(key_of(&fixture), before);
}

#[test]
fn tree_key_of_a_subdirectory_of_a_repository_is_an_error() {
    let fixture = fixture();
    let result = block_on(tree_key(
        &git(),
        &fixture.tree.join("src"),
        &fixture.indexes,
    ));
    assert!(
        matches!(result, Err(TreeError::NotTopLevel { .. })),
        "{result:?}"
    );
}

#[test]
fn tree_key_is_not_redirected_by_inherited_git_variables() {
    if std::env::var_os(HOSTILE_CHILD).is_some() {
        // The child: git variables point at another repository; the key must still be the
        // tree under test.
        let tree = PathBuf::from(std::env::var_os("TREE_KEY_TEST_TREE").unwrap());
        let indexes = PathBuf::from(std::env::var_os("TREE_KEY_TEST_INDEXES").unwrap());
        let key = block_on(tree_key(&git(), &tree, &indexes)).unwrap();
        assert_eq!(
            key.as_str(),
            std::env::var("TREE_KEY_TEST_EXPECTED").unwrap()
        );
        return;
    }
    let fixture = fixture();
    write_file(&fixture.tree, "untracked.txt", "x\n");
    let expected = key_of(&fixture);
    let decoy = fixture._root.path().join("decoy");
    std::fs::create_dir_all(&decoy).unwrap();
    init_git_repo(&decoy);
    write_file(&decoy, "other.txt", "other\n");
    commit_everything(&decoy, "decoy");
    rerun_with_hostile_git_env(
        module_path!(),
        "tree_key_is_not_redirected_by_inherited_git_variables",
        &decoy,
        &[
            ("TREE_KEY_TEST_TREE", fixture.tree.display().to_string()),
            (
                "TREE_KEY_TEST_INDEXES",
                fixture.indexes.display().to_string(),
            ),
            ("TREE_KEY_TEST_EXPECTED", expected.as_str().to_string()),
        ],
    );
}
