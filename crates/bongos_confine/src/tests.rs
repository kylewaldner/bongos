use std::{
    fs,
    path::{Path, PathBuf},
};

use sandbox::{GitSandboxPolicy, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};

use crate::{
    ConfineError, Confinement, HostGit, JobDeclaration, JobKind, JobTree, Platform, RULE,
    RootExclusions, SandboxAvailability, TreeAccess, TreeGit, annotate, promise, switch_off_notes,
};

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        Self { _temp: temp, base }
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.base.join(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.base.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// A main worktree at `repo` and a linked worktree `name` of it at `worktree`.
    fn add_worktree(&self, name: &str, worktree: &Path) -> PathBuf {
        let git_dir = self.dir(&format!("repo/.git/worktrees/{name}"));
        fs::write(
            git_dir.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        fs::create_dir_all(worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        git_dir
    }
}

fn declaration(
    scratch: PathBuf,
    trees: Vec<JobTree>,
    declared_dirs: Vec<PathBuf>,
) -> JobDeclaration {
    JobDeclaration {
        kind: JobKind::Program,
        trees,
        scratch,
        declared_dirs,
        extra_writable: Vec::new(),
        network: SandboxNetPolicy::Blocked,
    }
}

fn tree(path: PathBuf, access: TreeAccess, git: TreeGit) -> JobTree {
    JobTree { path, access, git }
}

#[test]
fn builds_an_allowed_linked_worktree_job() {
    let fixture = Fixture::new();
    fixture.dir("repo/.git");
    let worktree = fixture.base.join("trees/run");
    let linked_git_dir = fixture.add_worktree("run", &worktree);
    let snapshot = fixture.dir("snapshot");
    let scratch = fixture.dir("scratch");
    let lane = fixture.dir("lane");
    let host = HostGit {
        git_dirs: vec![linked_git_dir, fixture.base.join("repo/.git")],
        records_dir: Some(fixture.dir("records")),
    };

    let confinement = Confinement::build(
        &declaration(
            scratch.clone(),
            vec![
                tree(worktree.clone(), TreeAccess::Writable, TreeGit::Private),
                tree(snapshot.clone(), TreeAccess::ReadOnly, TreeGit::None),
            ],
            vec![lane.clone()],
        ),
        &host,
    )
    .unwrap();

    assert_eq!(
        confinement.writable_roots(),
        [scratch.clone(), worktree.clone(), lane.clone()]
    );
    assert!(!confinement.writable_roots().contains(&snapshot));
    assert_eq!(confinement.masks(), [worktree.join(".git")]);
    assert!(confinement.exclusions().is_empty());
    assert!(!confinement.requires_sandbox());
    assert_eq!(
        confinement.sandbox_policy(),
        SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![scratch, worktree.clone(), lane],
            },
            network: SandboxNetPolicy::Blocked,
            git: GitSandboxPolicy::Denied {
                git_dirs: Vec::new()
            },
            read_only_masks: vec![worktree.join(".git")],
        }
    );
}

#[test]
fn no_trees_and_no_declared_dirs_gives_scratch_alone() {
    let fixture = Fixture::new();
    let scratch = fixture.dir("scratch");

    let confinement = Confinement::build(
        &declaration(scratch.clone(), Vec::new(), Vec::new()),
        &HostGit::default(),
    )
    .unwrap();

    assert_eq!(confinement.writable_roots(), std::slice::from_ref(&scratch));
    assert!(confinement.masks().is_empty());
    let policy = confinement.sandbox_policy();
    assert_eq!(
        policy.fs,
        SandboxFsPolicy::Restricted {
            writable_paths: vec![scratch]
        }
    );
    assert!(policy.read_only_masks.is_empty());
}

#[test]
fn refuses_each_unsafe_declaration() {
    let fixture = Fixture::new();
    let common = fixture.dir("repo/.git");
    let worktree = fixture.base.join("trees/run");
    let linked_git_dir = fixture.add_worktree("run", &worktree);
    let scratch = fixture.dir("scratch");
    let records = fixture.dir("scratch/records");
    let host = HostGit {
        git_dirs: vec![linked_git_dir.clone(), common.clone()],
        records_dir: None,
    };
    let build = |scratch: PathBuf, trees: Vec<JobTree>, dirs: Vec<PathBuf>, host: &HostGit| {
        Confinement::build(&declaration(scratch, trees, dirs), host).unwrap_err()
    };

    assert_eq!(
        build(
            PathBuf::from("relative/scratch"),
            Vec::new(),
            Vec::new(),
            &host
        ),
        ConfineError::RelativePath {
            path: PathBuf::from("relative/scratch")
        }
    );
    assert_eq!(
        build(scratch.clone(), Vec::new(), vec![PathBuf::from("/")], &host),
        ConfineError::FilesystemRoot {
            path: PathBuf::from("/")
        }
    );
    assert_eq!(
        build(
            scratch.clone(),
            Vec::new(),
            vec![fixture.base.clone()],
            &host
        ),
        ConfineError::GitDirectory {
            root: fixture.base.clone(),
            git_dir: linked_git_dir.clone(),
        }
    );
    let inside_git = fixture.dir("repo/.git/worktrees/run/logs");
    assert_eq!(
        build(scratch.clone(), Vec::new(), vec![inside_git.clone()], &host),
        ConfineError::GitDirectory {
            root: inside_git,
            git_dir: linked_git_dir.clone(),
        }
    );
    assert_eq!(
        build(
            scratch.clone(),
            vec![tree(
                worktree.clone(),
                TreeAccess::ReadOnly,
                TreeGit::Private
            )],
            Vec::new(),
            &host
        ),
        ConfineError::ReadOnlyTreeWithPrivateGit { tree: worktree }
    );
    let missing = fixture.base.join("missing");
    assert!(matches!(
        build(scratch.clone(), Vec::new(), vec![missing.clone()], &host),
        ConfineError::Unresolved { path, .. } if path == missing
    ));
    assert_eq!(
        build(
            scratch.clone(),
            vec![tree(
                fixture.base.join("repo"),
                TreeAccess::Writable,
                TreeGit::Private
            )],
            Vec::new(),
            &host
        ),
        ConfineError::MainWorktreeWithPrivateGit {
            tree: fixture.base.join("repo")
        }
    );
    let with_records = HostGit {
        records_dir: Some(records.clone()),
        ..host.clone()
    };
    assert_eq!(
        build(scratch.clone(), Vec::new(), Vec::new(), &with_records),
        ConfineError::RecordsDirectory {
            root: scratch.clone(),
            records_dir: records,
        }
    );
    let outer_records = fixture.dir("records");
    let inside_records = fixture.dir("records/lane");
    let records_outside_scratch = HostGit {
        records_dir: Some(outer_records.clone()),
        ..host.clone()
    };
    assert_eq!(
        build(
            scratch.clone(),
            Vec::new(),
            vec![inside_records.clone()],
            &records_outside_scratch
        ),
        ConfineError::RecordsDirectory {
            root: inside_records,
            records_dir: outer_records,
        }
    );
    let lane = fixture.dir("lane");
    let inside_lane = fixture.dir("lane/snapshot");
    assert_eq!(
        build(
            scratch.clone(),
            vec![tree(lane.clone(), TreeAccess::ReadOnly, TreeGit::None)],
            vec![lane.clone()],
            &host
        ),
        ConfineError::ReadOnlyTreeInsideWritable {
            tree: lane.clone(),
            root: lane.clone(),
        }
    );
    assert_eq!(
        build(
            scratch.clone(),
            vec![tree(
                inside_lane.clone(),
                TreeAccess::ReadOnly,
                TreeGit::None
            )],
            vec![lane.clone()],
            &host
        ),
        ConfineError::ReadOnlyTreeInsideWritable {
            tree: inside_lane,
            root: lane,
        }
    );
    let inside_scratch = fixture.dir("scratch/copy");
    assert_eq!(
        build(
            scratch.clone(),
            vec![tree(
                inside_scratch.clone(),
                TreeAccess::ReadOnly,
                TreeGit::ReadOnly
            )],
            Vec::new(),
            &host
        ),
        ConfineError::ReadOnlyTreeInsideWritable {
            tree: inside_scratch,
            root: scratch.clone(),
        }
    );

    #[cfg(unix)]
    {
        let link = fixture.base.join("lane-link");
        std::os::unix::fs::symlink(&common, &link).unwrap();
        assert_eq!(
            build(scratch, Vec::new(), vec![link], &host),
            ConfineError::GitDirectory {
                root: common,
                git_dir: linked_git_dir,
            }
        );
    }
}

#[test]
fn main_worktree_masks_its_git_nested_worktree_and_nested_clone() {
    let fixture = Fixture::new();
    let repo = fixture.dir("repo");
    fixture.file("repo/.git/HEAD", "ref: refs/heads/main\n");
    let nested_worktree = repo.join(".bongos-worktrees/task");
    let nested_git_dir = fixture.add_worktree("task", &nested_worktree);
    fixture.file("repo/vendor/lib/.git/config", "[core]\n");
    fixture.file("repo/vendor/lib/src/main.rs", "");
    fixture.file(
        "repo/.bongos-worktrees/task/inner/.git",
        "gitdir: elsewhere\n",
    );
    fixture.file("outside/clone/.git/config", "[core]\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(fixture.base.join("outside"), repo.join("linked")).unwrap();
    let scratch = fixture.dir("scratch");

    let confinement = Confinement::build(
        &declaration(
            scratch,
            vec![tree(repo.clone(), TreeAccess::Writable, TreeGit::ReadOnly)],
            Vec::new(),
        ),
        &HostGit {
            git_dirs: vec![repo.join(".git"), nested_git_dir],
            records_dir: None,
        },
    )
    .unwrap();

    let expected = vec![
        repo.join(".git"),
        nested_worktree,
        repo.join("vendor/lib/.git"),
    ];
    assert_eq!(confinement.masks(), expected.as_slice());
    assert_eq!(confinement.sandbox_policy().read_only_masks, expected);
    assert!(confinement.requires_sandbox());
    assert!(confinement.writable_roots().contains(&repo));
}

#[test]
fn main_worktree_skips_a_stale_gitdir_entry_and_lists_exclusions() {
    let fixture = Fixture::new();
    let repo = fixture.dir("repo");
    fixture.file("repo/.git/HEAD", "ref: refs/heads/main\n");
    fixture.file(
        "repo/.git/worktrees/gone/gitdir",
        &format!("{}\n", repo.join(".bongos-worktrees/gone/.git").display()),
    );
    fixture.dir("repo/.git/worktrees/incomplete");
    fixture.file("repo/sub/.git", "gitdir: ../.git/modules/sub\n");
    let scratch = fixture.dir("scratch");

    let confinement = Confinement::build(
        &declaration(
            scratch,
            vec![tree(repo.clone(), TreeAccess::Writable, TreeGit::ReadOnly)],
            Vec::new(),
        ),
        &HostGit {
            git_dirs: vec![repo.join(".git")],
            records_dir: None,
        },
    )
    .unwrap();

    assert_eq!(
        confinement.masks(),
        [repo.join(".git"), repo.join("sub/.git")]
    );
    assert!(confinement.masks().iter().all(|mask| mask.exists()));
    assert_eq!(
        confinement.exclusions(),
        [RootExclusions {
            root: repo.clone(),
            excluded: vec![repo.join(".git")],
        }]
    );
}

#[test]
fn nested_clone_git_is_masked_but_not_excluded() {
    let fixture = Fixture::new();
    let repo = fixture.dir("repo");
    fixture.file("repo/.git/HEAD", "ref: refs/heads/main\n");
    let direct_worktree = repo.join("wt");
    let direct_git_dir = fixture.add_worktree("wt", &direct_worktree);
    let deep_worktree = repo.join(".bongos-worktrees/task");
    let deep_git_dir = fixture.add_worktree("task", &deep_worktree);
    fixture.file("repo/vendor/lib/.git/config", "[core]\n");
    let scratch = fixture.dir("scratch");

    let confinement = Confinement::build(
        &declaration(
            scratch,
            vec![tree(repo.clone(), TreeAccess::Writable, TreeGit::ReadOnly)],
            Vec::new(),
        ),
        &HostGit {
            git_dirs: vec![repo.join(".git"), direct_git_dir, deep_git_dir],
            records_dir: None,
        },
    )
    .unwrap();

    assert_eq!(
        confinement.masks(),
        [
            repo.join(".git"),
            deep_worktree.clone(),
            direct_worktree.clone(),
            repo.join("vendor/lib/.git"),
        ]
    );
    assert_eq!(
        confinement.exclusions(),
        [RootExclusions {
            root: repo.clone(),
            excluded: vec![repo.join(".git"), direct_worktree],
        }]
    );
    let excluded = &confinement.exclusions()[0].excluded;
    assert!(!excluded.contains(&repo.join("vendor/lib/.git")));
    assert!(!excluded.contains(&deep_worktree));
}

#[cfg(unix)]
#[test]
fn refuses_a_tree_whose_git_is_a_symlink() {
    let fixture = Fixture::new();
    let repo = fixture.dir("repo");
    let real_git = fixture.dir("elsewhere/.git");
    std::os::unix::fs::symlink(&real_git, repo.join(".git")).unwrap();
    let scratch = fixture.dir("scratch");

    assert_eq!(
        Confinement::build(
            &declaration(
                scratch,
                vec![tree(repo.clone(), TreeAccess::Writable, TreeGit::ReadOnly)],
                Vec::new(),
            ),
            &HostGit::default(),
        )
        .unwrap_err(),
        ConfineError::MaskSymlink {
            path: repo.join(".git")
        }
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_main_worktree_with_an_unreadable_directory() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.dir("repo");
    fixture.file("repo/.git/HEAD", "ref: refs/heads/main\n");
    let locked = fixture.dir("repo/locked");
    let scratch = fixture.dir("scratch");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // Root (or a capability that bypasses permissions) can still read the directory; the walk
    // then succeeds and this case cannot be produced, so the test checks nothing there.
    let readable = fs::read_dir(&locked).is_ok();

    let result = Confinement::build(
        &declaration(
            scratch,
            vec![tree(repo, TreeAccess::Writable, TreeGit::ReadOnly)],
            Vec::new(),
        ),
        &HostGit::default(),
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    if !readable {
        assert!(matches!(
            result,
            Err(ConfineError::Walk { path, .. }) if path == locked
        ));
    }
}

#[test]
fn annotates_both_os_refusal_messages() {
    let roots = [PathBuf::from("/work/tree"), PathBuf::from("/work/scratch")];
    let annotation = |path: &str| Some(format!("bongos: {RULE}: {path}"));

    assert_eq!(
        annotate::refusal(
            "touch: cannot touch '/home/u/x': Read-only file system",
            &roots
        ),
        annotation("/home/u/x")
    );
    assert_eq!(
        annotate::refusal(
            "mkdir: cannot create directory ‘/etc/y’: Operation not permitted",
            &roots
        ),
        annotation("/etc/y")
    );
    assert_eq!(
        annotate::refusal(
            "PermissionError: [Errno 1] Operation not permitted: '/work/other/z'",
            &roots
        ),
        annotation("/work/other/z")
    );
    assert_eq!(
        annotate::refusal("sh: /usr/local/w: Read-only file system", &roots),
        annotation("/usr/local/w")
    );
    assert_eq!(
        annotate::refusal("sh: can't create '/opt/v': Read-only file system", &roots),
        annotation("/opt/v")
    );
    assert_eq!(
        annotate::refusal(
            "OSError: [Errno 30] Read-only file system: '/work/tree/../escape'",
            &roots
        ),
        annotation("/work/escape")
    );
}

#[test]
fn leaves_allowed_paths_and_other_lines_alone() {
    let roots = [PathBuf::from("/work/tree"), PathBuf::from("/work/scratch")];

    assert_eq!(
        annotate::refusal(
            "touch: cannot touch '/work/tree/a': Read-only file system",
            &roots
        ),
        None
    );
    assert_eq!(
        annotate::refusal("sh: /work/scratch/b: Operation not permitted", &roots),
        None
    );
    assert_eq!(
        annotate::refusal("cat: '/etc/x': No such file or directory", &roots),
        None
    );
    assert_eq!(
        annotate::refusal(
            "touch: cannot touch 'relative': Read-only file system",
            &roots
        ),
        None
    );
    assert_eq!(annotate::refusal("Read-only file system", &roots), None);
}

#[test]
fn promise_text_per_platform() {
    let linux = promise(Platform::Linux, SandboxAvailability::Available);
    assert!(
        linux
            .body
            .starts_with("Jobs (workflow programs, agent steps and task checks)")
    );
    assert!(
        linux
            .body
            .contains("never uses configuration or pointers a job wrote")
    );

    let macos = promise(Platform::MacOs, SandboxAvailability::Available);
    assert!(macos.body.contains("macOS enforces this by path"));
    assert!(macos.body.contains("Reads are not restricted"));

    let unconfined = promise(Platform::Linux, SandboxAvailability::Unavailable);
    assert!(unconfined.body.starts_with("No sandbox is available here"));
    assert!(unconfined.body.contains("Install Bubblewrap (Linux)"));
    assert_eq!(
        promise(Platform::Other, SandboxAvailability::Available),
        unconfined
    );
    assert_eq!(
        promise(Platform::MacOs, SandboxAvailability::Unavailable),
        unconfined
    );
    assert_ne!(linux.title, macos.title);
    assert_ne!(linux.title, unconfined.title);

    assert_eq!(
        switch_off_notes(false, true),
        ["Agent steps are not confined (confinement.agent_steps is off)"]
    );
    assert_eq!(
        switch_off_notes(true, false),
        ["Task checks are not confined (confinement.task_checks is off)"]
    );
    assert!(switch_off_notes(true, true).is_empty());
}
