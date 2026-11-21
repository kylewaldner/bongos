use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use bongos_confine::safe_git::linked_git_dir_from_record;
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

    /// Task git trusting the linked worktrees of `main` only.
    fn git(&self) -> TaskGit {
        TaskGit::new(Git::locate().unwrap().isolated(), PathBuf::new())
            .with_trusted_repositories([self.main.clone()])
    }

    fn key(&self, tree: &Path) -> Result<TreeKey, TreeError> {
        block_on(tree_key(&self.git(), tree, &self.indexes))
    }

    /// A repository `name` of its own, with one commit, whose configuration runs `script` as its
    /// fsmonitor.
    fn hostile_repository(&self, name: &str, script: &Path) -> PathBuf {
        let hostile = self.root.join(name);
        std::fs::create_dir_all(&hostile).unwrap();
        init_git_repo(&hostile);
        write_file(&hostile, "h.txt", "h\n");
        commit_everything(&hostile, "hostile");
        git_in(
            &hostile,
            &["config", "core.fsmonitor", script.to_str().unwrap()],
        );
        hostile
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
        let git_dir = self.hostile_repository("hostile", script).join(".git");
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
fn a_nested_repository_is_keyed_without_its_configuration_running() {
    let world = World::new();
    let marker = world.root.join("nested-ran");
    let filter = world.script("nested-filter", &marker);
    let inner = world.task.join("vendor/inner");
    std::fs::create_dir_all(&inner).unwrap();
    init_git_repo(&inner);
    write_file(&inner, ".gitattributes", "*.txt filter=y\n");
    write_file(&inner, "x.txt", "x\n");
    commit_everything(&inner, "inner");
    let inner_head = git_in(&inner, &["rev-parse", "HEAD"]);
    let benign_config = std::fs::read(inner.join(".git/config")).unwrap();
    git_in(
        &inner,
        &["config", "filter.y.clean", filter.to_str().unwrap()],
    );
    git_in(
        &inner,
        &["config", "core.fsmonitor", filter.to_str().unwrap()],
    );

    // Every key after the first meets the nested repository as a gitlink already staged.
    let mut keys = Vec::new();
    for edit in ["y\n", "z\n"] {
        write_file(&inner, "x.txt", edit);
        keys.push(world.key(&world.task).unwrap());
    }
    assert!(
        !marker.exists(),
        "the nested repository's configuration ran"
    );
    assert_eq!(keys[0], keys[1], "the gitlink names the nested HEAD only");
    let entry = git_in(&world.main, &["ls-tree", keys[0].as_str(), "vendor/inner"]);
    assert!(
        entry.starts_with(&format!("160000 commit {inner_head}\t")),
        "{entry}"
    );
    std::fs::write(inner.join(".git/config"), &benign_config).unwrap();
    assert_eq!(world.key(&world.task).unwrap(), keys[0]);

    // Plain git checks a staged gitlink with a status inside it, which runs the nested filter.
    git_in(
        &inner,
        &["config", "filter.y.clean", filter.to_str().unwrap()],
    );
    git_in(&world.task, &["add", "-A"]);
    write_file(&inner, "x.txt", "w\n");
    git_in(&world.task, &["add", "-A"]);
    assert!(marker.exists(), "the nested filter is live");
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
        &world.git(),
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

#[test]
fn a_linked_worktree_of_an_untrusted_repository_is_refused() {
    let world = World::new();
    let marker = world.root.join("planted-ran");
    let script = world.script("fsmonitor", &marker);
    // A job that can write outside its tree makes a whole look-alike: a repository of its own
    // whose `worktrees/planted` holds `gitdir` and `commondir` files as git writes them.
    let hostile = world.hostile_repository("hostile", &script);
    let planted = world.root.join("planted");
    git_in(
        &hostile,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            planted.to_str().unwrap(),
        ],
    );
    std::fs::remove_file(&marker).ok();
    let common = hostile.join(".git").canonicalize().unwrap();
    assert!(
        linked_git_dir_from_record(&planted, &common).is_ok(),
        "the layout passes when its own common directory is taken as trusted"
    );
    git_in(&planted, &["status", "--porcelain"]);
    assert!(marker.exists(), "the planted fsmonitor is live");
    std::fs::remove_file(&marker).unwrap();

    let key = world.key(&planted);
    assert!(matches!(key, Err(TreeError::NotGitTree { .. })), "{key:?}");
    let checked_out = block_on(check_out_lent_tree(
        &world.git(),
        &planted,
        &git_in(&hostile, &["rev-parse", "HEAD"]),
        &[],
        &StopHandle::default(),
    ));
    assert!(
        matches!(checked_out, Err(LentTreeError::Failed(_))),
        "{checked_out:?}"
    );
    assert!(!marker.exists(), "the planted configuration ran");
}

#[test]
fn a_record_whose_git_directory_moved_to_another_tree_is_not_used() {
    let world = World::new();
    world.key(&world.task).unwrap();
    // The tree goes and git hands its git directory's name to a new tree elsewhere; then a
    // directory at the old path points at that git directory.
    git_in(
        &world.main,
        &[
            "worktree",
            "remove",
            "--force",
            world.task.to_str().unwrap(),
        ],
    );
    let elsewhere = world.root.join("elsewhere/task");
    git_in(
        &world.main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            elsewhere.to_str().unwrap(),
        ],
    );
    let reused = world.main.join(".git/worktrees/task");
    assert!(
        std::fs::read_to_string(reused.join("gitdir"))
            .unwrap()
            .contains("elsewhere"),
        "git reused the name"
    );
    std::fs::create_dir_all(&world.task).unwrap();
    std::fs::write(
        world.task.join(".git"),
        format!("gitdir: {}\n", reused.display()),
    )
    .unwrap();

    let key = world.key(&world.task);
    assert!(matches!(key, Err(TreeError::NotGitTree { .. })), "{key:?}");
    assert_eq!(world.key(&elsewhere).unwrap().as_str(), tree_id(&elsewhere));
}
