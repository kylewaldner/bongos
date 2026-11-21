use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use bongos_project_repo::Git;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::base_tree::{LentTreeError, check_out_lent_tree};
use crate::process::StopHandle;
use crate::task_git::TaskGit;
use crate::test_support::{
    FakeSecond, FakeTrees, commit_everything, git_in, init_git_repo, write_file,
};
use crate::tree_key::{TreeError, TreeKey, tree_key};
use crate::trees::{SecondTree, TaskTrees as _};

fn git() -> TaskGit {
    TaskGit::new(Git::locate().unwrap().isolated(), PathBuf::new())
}

/// A repository `main` with one commit, a linked worktree `task` of it, and an index folder.
struct World {
    _root: TempDir,
    root: PathBuf,
    main: PathBuf,
    task: PathBuf,
    indexes: PathBuf,
}

impl World {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        init_git_repo(&main);
        write_file(&main, "a.txt", "one\n");
        write_file(&main, "src/lib.kt", "fun f() {}\n");
        commit_everything(&main, "first");
        let task = root.join("task");
        git_in(
            &main,
            &["worktree", "add", "-q", "--detach", task.to_str().unwrap()],
        );
        Self {
            indexes: root.join("indexes"),
            _root: temp,
            root,
            main,
            task,
        }
    }

    fn key(&self, tree: &Path) -> Result<TreeKey, TreeError> {
        block_on(tree_key(&git(), tree, &self.indexes))
    }

    /// A script that creates `marker` and fails, as an fsmonitor or a filter would.
    fn script(&self, name: &str, marker: &Path) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A repository whose configuration runs `script` as its fsmonitor, and points `tree`'s
    /// `.git` at it.
    fn point_at_hostile_repository(&self, tree: &Path, script: &Path) -> PathBuf {
        let hostile = self.root.join("hostile");
        std::fs::create_dir_all(&hostile).unwrap();
        init_git_repo(&hostile);
        write_file(&hostile, "h.txt", "h\n");
        commit_everything(&hostile, "hostile");
        git_in(
            &hostile,
            &["config", "core.fsmonitor", script.to_str().unwrap()],
        );
        let git_dir = hostile.join(".git");
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        git_dir
    }
}

fn tree_id(directory: &Path) -> String {
    git_in(directory, &["rev-parse", "HEAD^{tree}"])
}

#[test]
fn the_key_through_the_recorded_git_directory_is_the_git_tree_id() {
    let world = World::new();
    assert_eq!(
        world.key(&world.task).unwrap().as_str(),
        tree_id(&world.task)
    );

    write_file(&world.task, "src/new.kt", "fun g() {}\n");
    write_file(&world.task, "a.txt", "two\n");
    let key = world.key(&world.task).unwrap();
    commit_everything(&world.task, "second");
    assert_eq!(key.as_str(), tree_id(&world.task));
}

#[test]
fn a_rewritten_git_file_runs_nothing_and_the_key_stays() {
    let world = World::new();
    let clean = world.key(&world.task).unwrap();
    let marker = world.root.join("fsmonitor-ran");
    let script = world.script("fsmonitor", &marker);
    let hostile = world.point_at_hostile_repository(&world.task, &script);

    // Plain git in the tree reads the planted configuration.
    git_in(&world.task, &["status", "--porcelain"]);
    assert!(
        marker.exists(),
        "the hostile repository's fsmonitor is live"
    );
    std::fs::remove_file(&marker).unwrap();

    let key = world.key(&world.task).unwrap();
    assert_eq!(key, clean);
    assert!(!marker.exists(), "the planted fsmonitor ran");
    assert!(hostile.join("HEAD").exists());
}

#[test]
fn a_filter_in_the_repository_configuration_is_not_run_by_the_key() {
    let world = World::new();
    write_file(&world.main, ".gitattributes", "*.txt filter=x\n");
    commit_everything(&world.main, "attributes");
    git_in(&world.task, &["checkout", "-q", "--detach", "main"]);
    let marker = world.root.join("filter-ran");
    let script = world.script("filter", &marker);
    git_in(
        &world.main,
        &["config", "filter.x.clean", script.to_str().unwrap()],
    );

    // Plain git applies the filter to the file.
    git_in(&world.task, &["hash-object", "a.txt"]);
    assert!(marker.exists(), "the configured filter is live");
    std::fs::remove_file(&marker).unwrap();

    let key = world.key(&world.task).unwrap();
    assert!(!marker.exists(), "the filter ran during the key");
    assert_eq!(key.as_str(), tree_id(&world.task));
}

#[test]
fn a_nested_repository_is_refused_and_its_configuration_never_read() {
    let world = World::new();
    let marker = world.root.join("nested-ran");
    let script = world.script("nested-fsmonitor", &marker);
    let inner = world.task.join("vendor/inner");
    std::fs::create_dir_all(&inner).unwrap();
    init_git_repo(&inner);
    write_file(&inner, "x.txt", "x\n");
    commit_everything(&inner, "inner");
    git_in(
        &inner,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );

    let error = world.key(&world.task).unwrap_err();
    assert!(
        matches!(error, TreeError::Git(_)) && error.to_string().contains("nested repository"),
        "{error}"
    );
    assert!(
        !marker.exists(),
        "the nested repository's configuration ran"
    );
}

#[test]
fn a_lease_whose_git_file_names_another_directory_is_refused_before_any_checkout() {
    let world = World::new();
    let commit = git_in(&world.main, &["rev-parse", "HEAD"]);
    write_file(&world.main, "a.txt", "newer\n");
    commit_everything(&world.main, "second");
    let lease_tree = world.root.join("lease");
    git_in(
        &world.main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            lease_tree.to_str().unwrap(),
        ],
    );
    let marker = world.root.join("hostile-ran");
    let script = world.script("fsmonitor", &marker);
    let hostile = world.point_at_hostile_repository(&lease_tree, &script);
    let hostile_head = git_in(&hostile, &["rev-parse", "HEAD"]);
    let files_before = std::fs::read_to_string(lease_tree.join("a.txt")).unwrap();

    let trees = FakeTrees::default();
    trees.state().second.insert(
        "app".to_string(),
        FakeSecond::Lease {
            path: lease_tree.clone(),
            env: Vec::new(),
        },
    );
    let SecondTree::Lease(lease) = trees.second_tree("task", "app") else {
        panic!("the fake lends the tree");
    };

    let result = block_on(check_out_lent_tree(
        &git(),
        lease.path(),
        &commit,
        &[],
        &StopHandle::default(),
    ));
    assert!(
        matches!(result, Err(LentTreeError::Failed(_))),
        "{result:?}"
    );
    let key = world.key(lease.path());
    assert!(matches!(key, Err(TreeError::NotGitTree { .. })), "{key:?}");

    assert!(
        !marker.exists(),
        "the hostile repository's configuration ran"
    );
    assert_eq!(git_in(&hostile, &["rev-parse", "HEAD"]), hostile_head);
    assert_eq!(
        std::fs::read_to_string(lease_tree.join("a.txt")).unwrap(),
        files_before
    );
}
