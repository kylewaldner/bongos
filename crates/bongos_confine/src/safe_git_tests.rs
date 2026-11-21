use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use util::job_roots::{JobRoots, RootLifetime};

use crate::safe_git::{SafeGit, check_git_dir_with, linked_git_dir_from_record};

/// A temporary directory with its own job roots registry and an empty home, so neither the
/// user's git configuration nor the real registry is ever read.
struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    roots: JobRoots,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let roots = JobRoots::new(&base.join("registry").join("roots.json"));
        Self {
            _temp: temp,
            base,
            home,
            roots,
        }
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

    /// A shell script at `relative` that creates `marker`, then runs `rest`.
    fn marker_script(&self, relative: &str, marker: &Path, rest: &str) -> PathBuf {
        let path = self.file(
            relative,
            &format!("#!/bin/sh\ntouch '{}'\n{rest}\n", marker.display()),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn register(&self, root: &Path) {
        self.roots
            .register(root, &[], RootLifetime::Permanent)
            .unwrap();
    }

    /// Runs real git with `args`, isolated from the user's and system's configuration, and
    /// asserts it succeeded.
    fn git<S: AsRef<OsStr>>(&self, directory: &Path, args: &[S]) -> Output {
        let mut command = util::command::new_command("git");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_PARAMETERS",
        ] {
            command.env_remove(name);
        }
        command
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", &self.home)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@localhost",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args);
        let output = smol::block_on(command.output()).unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args.iter().map(|arg| arg.as_ref()).collect::<Vec<_>>(),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn safe_git(&self, git_dir: &Path, work_tree: &Path) -> SafeGit {
        SafeGit::new(git_dir, work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", &self.home)
    }

    /// Runs `args` through `git`, deciding job roots with this fixture's registry, and asserts
    /// it succeeded.
    fn run_safe(&self, git: &SafeGit, args: &[&str]) -> Output {
        let output = self.run_safe_unchecked(git, args);
        assert!(
            output.status.success(),
            "safe git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Runs `args` through `git` like [`Fixture::run_safe`], whatever its exit status.
    fn run_safe_unchecked(&self, git: &SafeGit, args: &[&str]) -> Output {
        let in_job_root = |path: &Path| self.roots.contains(path);
        let mut command = smol::block_on(git.command_with(args, &in_job_root)).unwrap();
        smol::block_on(command.output()).unwrap()
    }

    /// The error building `args` through `git` fails with.
    fn refused(&self, git: &SafeGit, args: &[&str]) -> String {
        let in_job_root = |path: &Path| self.roots.contains(path);
        smol::block_on(git.command_with(args, &in_job_root))
            .unwrap_err()
            .to_string()
    }

    /// The fixture's isolated git, plus an identity for commands that commit.
    fn safe_git_with_identity(&self, git_dir: &Path, work_tree: &Path) -> SafeGit {
        self.safe_git(git_dir, work_tree)
            .config("user.name", "test")
            .config("user.email", "test@localhost")
            .config("commit.gpgsign", "false")
    }

    /// Runs real git on the host repository `host_git` with the work tree `tree`, as git would
    /// without the protections.
    fn plain(&self, host_git: &Path, tree: &Path, args: &[&str]) -> Output {
        let mut all = vec![
            format!("--git-dir={}", host_git.display()),
            format!("--work-tree={}", tree.display()),
        ];
        all.extend(args.iter().map(|arg| arg.to_string()));
        self.git(tree, &all)
    }
}

#[test]
fn safe_git_refuses_a_git_directory_inside_a_job_root() {
    let fixture = Fixture::new();
    let job = fixture.dir("job");
    fixture.file("job/.git/HEAD", "ref: refs/heads/main\n");
    let common_in_job = fixture.dir("job/common");
    let linked = fixture.file("host/worktrees/run/commondir", "../../../job/common\n");
    let linked = linked.parent().unwrap().to_path_buf();
    let clean_git_dir = fixture.dir("clean/.git");
    let outside_tree = fixture.dir("outside");
    fixture.register(&job);
    let in_job_root = |path: &Path| fixture.roots.contains(path);

    let error = smol::block_on(
        fixture
            .safe_git(&job.join(".git"), &job)
            .command_with(&["status"], &in_job_root),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "refusing to run git with a job-written git directory: {}",
            job.join(".git").display()
        )
    );

    let error = check_git_dir_with(&linked, &in_job_root).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "refusing to run git with a job-written git directory: {}",
            linked.join("../../../job/common").display()
        )
    );
    assert!(in_job_root(&common_in_job));

    let command = smol::block_on(
        fixture
            .safe_git(&clean_git_dir, &outside_tree)
            .command_with(&["status"], &in_job_root),
    )
    .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut git_dir_arg = String::from("--git-dir=");
    git_dir_arg.push_str(&clean_git_dir.to_string_lossy());
    assert_eq!(args[0], git_dir_arg);
    for expected in [
        "core.fsmonitor=false",
        "safe.bareRepository=explicit",
        "core.untrackedCache=false",
        "--no-optional-locks",
    ] {
        assert!(args.iter().any(|arg| arg == expected), "{args:?}");
    }
    assert_eq!(args.last().map(String::as_str), Some("status"));
}

#[test]
fn safe_git_never_runs_a_planted_fsmonitor() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("fsmonitor-ran");
    let hook = fixture.marker_script("bin/fsmonitor", &marker, "exit 1");
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    let tree = fixture.dir("tree");
    fixture.git(&fixture.base, &["init", "-q", "tree"]);
    fixture.file("tree/file.txt", "one\n");
    fixture.git(&tree, &["add", "file.txt"]);
    fixture.git(&tree, &["commit", "-q", "-m", "first"]);
    let hook = hook.to_string_lossy().into_owned();
    fixture.git(&tree, &["config", "core.fsmonitor", &hook]);
    fixture.git(&host, &["config", "core.fsmonitor", &hook]);

    // Plain git in the tree runs the planted hook.
    fixture.git(&tree, &["status", "--porcelain"]);
    assert!(marker.exists(), "the fsmonitor hook was never run by git");
    fs::remove_file(&marker).unwrap();

    fixture.register(&tree);
    let git = fixture.safe_git(&host.join(".git"), &tree);
    fixture.run_safe(&git, &["status", "--porcelain"]);
    assert!(!marker.exists());
}

#[test]
fn safe_git_switches_off_textconv_and_filters_in_a_job_tree() {
    let fixture = Fixture::new();
    let textconv_marker = fixture.base.join("textconv-ran");
    let filter_marker = fixture.base.join("filter-ran");
    let textconv = fixture.marker_script("bin/textconv", &textconv_marker, "cat \"$1\"");
    let filter = fixture.marker_script("bin/filter", &filter_marker, "cat");
    let textconv = textconv.to_string_lossy().into_owned();
    let filter = filter.to_string_lossy().into_owned();
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    let host_git = host.join(".git");
    let tree = fixture.dir("tree");
    // The tree's own planted repository defines the driver too.
    fixture.git(&fixture.base, &["init", "-q", "tree"]);
    fixture.git(&tree, &["config", "diff.evil.textconv", &textconv]);
    fixture.git(&host, &["config", "diff.evil.textconv", &textconv]);
    fixture.git(&host, &["config", "filter.evil.clean", &filter]);
    fixture.git(&host, &["config", "filter.evil.smudge", &filter]);
    fixture.git(&host, &["config", "filter.evil.required", "true"]);

    let mut git_dir_arg = String::from("--git-dir=");
    git_dir_arg.push_str(&host_git.to_string_lossy());
    let mut work_tree_arg = String::from("--work-tree=");
    work_tree_arg.push_str(&tree.to_string_lossy());
    let plain = |args: &[&str]| {
        let mut all = vec![git_dir_arg.as_str(), work_tree_arg.as_str()];
        all.extend(args);
        fixture.git(&tree, &all)
    };
    fixture.file("tree/a.txt", "one\n");
    fixture.file("tree/b.dat", "one\n");
    plain(&["add", "a.txt", "b.dat"]);
    plain(&["commit", "-q", "-m", "first"]);
    fixture.file(
        "tree/.gitattributes",
        "*.txt diff=evil\n*.dat filter=evil\n",
    );
    fixture.file("tree/a.txt", "two\n");
    fixture.file("tree/b.dat", "two\n");

    // Plain git runs both drivers the job-written attributes select.
    plain(&["diff"]);
    assert!(
        textconv_marker.exists(),
        "the textconv was never run by git"
    );
    assert!(filter_marker.exists(), "the filter was never run by git");
    fs::remove_file(&textconv_marker).unwrap();
    fs::remove_file(&filter_marker).unwrap();

    fixture.register(&tree);
    let git = fixture.safe_git(&host_git, &tree);
    let diff = fixture.run_safe(&git, &["diff"]);
    assert!(String::from_utf8_lossy(&diff.stdout).contains("+two"));
    fixture.run_safe(&git, &["status", "--porcelain"]);
    fixture.run_safe(&git, &["log", "-p", "-1"]);
    assert!(!textconv_marker.exists());
    assert!(!filter_marker.exists());
}

#[test]
fn linked_git_dir_is_trusted_only_under_worktrees() {
    let fixture = Fixture::new();
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    fixture.file("host/file.txt", "one\n");
    fixture.git(&host, &["add", "file.txt"]);
    fixture.git(&host, &["commit", "-q", "-m", "first"]);
    let linked = fixture.base.join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    fixture.git(&host, &["worktree", "add", "-q", "--detach", &linked_arg]);

    assert_eq!(
        linked_git_dir_from_record(&linked, &host.join(".git")).unwrap(),
        fs::canonicalize(host.join(".git/worktrees/linked")).unwrap()
    );

    let fake = fixture.dir("fake");
    fixture.file("fake/gitdir", "elsewhere\n");
    fixture.file("fake/commondir", "../host/.git\n");
    fixture.file("planted/.git", &format!("gitdir: {}\n", fake.display()));
    let error =
        linked_git_dir_from_record(&fixture.base.join("planted"), &host.join(".git")).unwrap_err();
    assert!(
        error.to_string().contains("not directly under worktrees/"),
        "{error}"
    );

    fixture.dir("main/.git");
    assert!(linked_git_dir_from_record(&fixture.base.join("main"), &host.join(".git")).is_err());

    // A `.git` naming a sibling worktree's git directory is refused.
    let sibling = fixture.base.join("sibling");
    let sibling_arg = sibling.to_string_lossy().into_owned();
    fixture.git(&host, &["worktree", "add", "-q", "--detach", &sibling_arg]);
    fs::write(
        linked.join(".git"),
        format!(
            "gitdir: {}\n",
            host.join(".git/worktrees/sibling").display()
        ),
    )
    .unwrap();
    let error = linked_git_dir_from_record(&linked, &host.join(".git")).unwrap_err();
    assert!(
        error.to_string().contains("records another worktree"),
        "{error}"
    );
}

#[test]
fn safe_git_runs_no_hook_in_a_job_tree() {
    let fixture = Fixture::new();
    let file_hook_marker = fixture.base.join("file-hook-ran");
    let config_hook_marker = fixture.base.join("config-hook-ran");
    let config_hook = fixture.marker_script("bin/config-hook", &config_hook_marker, "exit 0");
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    let host_git = host.join(".git");
    let tree = fixture.dir("tree");
    // A relative hooks path, as husky sets: it resolves in the work tree, which the job wrote.
    fixture.git(&host, &["config", "core.hooksPath", ".husky/_"]);
    fixture.git(
        &host,
        &[
            "config",
            "hook.lint.command",
            &config_hook.to_string_lossy(),
        ],
    );
    fixture.git(&host, &["config", "hook.lint.event", "pre-commit"]);
    fixture.marker_script("tree/.husky/_/pre-commit", &file_hook_marker, "exit 0");

    fixture.file("tree/a.txt", "one\n");
    fixture.plain(&host_git, &tree, &["add", "a.txt"]);
    fixture.plain(&host_git, &tree, &["commit", "-q", "-m", "first"]);
    assert!(
        file_hook_marker.exists(),
        "the hook file was never run by git"
    );
    assert!(
        config_hook_marker.exists(),
        "the configured hook was never run by git"
    );
    fs::remove_file(&file_hook_marker).unwrap();
    fs::remove_file(&config_hook_marker).unwrap();

    fixture.register(&tree);
    let git = fixture.safe_git_with_identity(&host_git, &tree);
    fixture.file("tree/a.txt", "two\n");
    fixture.run_safe(&git, &["commit", "-q", "-a", "-m", "second"]);
    fixture.run_safe(&git, &["checkout", "-q", "HEAD~1"]);
    fixture.run_safe(&git, &["reset", "-q", "--hard", "main"]);
    assert!(!file_hook_marker.exists());
    assert!(!config_hook_marker.exists());
}

#[test]
fn safe_git_never_enters_a_job_replaced_submodule() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("submodule-filter-ran");
    let filter = fixture.marker_script("bin/filter", &marker, "cat");
    let filter = filter.to_string_lossy().into_owned();
    let source = fixture.dir("source");
    fixture.git(&fixture.base, &["init", "-q", "source"]);
    fixture.file("source/s.txt", "one\n");
    fixture.git(&source, &["add", "s.txt"]);
    fixture.git(&source, &["commit", "-q", "-m", "source"]);
    let tree = fixture.dir("tree");
    fixture.git(&fixture.base, &["init", "-q", "tree"]);
    let source_arg = source.to_string_lossy().into_owned();
    fixture.git(
        &tree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &source_arg,
            "sub",
        ],
    );
    fixture.git(&tree, &["commit", "-q", "-m", "submodule"]);

    // The job replaces the submodule's repository with its own, which defines a filter its
    // attributes select, and asks for the submodule to be checked through `.gitmodules`.
    fs::remove_file(tree.join("sub/.git"))
        .or_else(|_| fs::remove_dir_all(tree.join("sub/.git")))
        .unwrap();
    let sub = tree.join("sub");
    fixture.git(&sub, &["init", "-q", "."]);
    fixture.git(&sub, &["add", "s.txt"]);
    fixture.git(&sub, &["commit", "-q", "-m", "planted"]);
    fixture.git(&sub, &["config", "filter.planted.clean", &filter]);
    fixture.file("tree/sub/.gitattributes", "* filter=planted\n");
    fixture.file("tree/sub/s.txt", "two\n");
    fixture.git(
        &tree,
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.sub.ignore",
            "none",
        ],
    );

    // Plain git enters the submodule and runs the planted filter.
    fixture.git(&tree, &["status", "--porcelain"]);
    assert!(marker.exists(), "the submodule filter was never run by git");
    fs::remove_file(&marker).unwrap();

    // The main worktree is the job root, without its masked `.git`.
    fixture
        .roots
        .register(&tree, &[tree.join(".git")], RootLifetime::Permanent)
        .unwrap();
    let git = fixture.safe_git_with_identity(&tree.join(".git"), &tree);
    for args in [
        &["status", "--porcelain"][..],
        &["diff"],
        &["diff", "HEAD"],
        &["diff-files"],
        &["diff-index", "HEAD"],
        &["describe", "--dirty", "--always"],
        &["add", "-A"],
        &["add", "sub"],
        &["rm", "-n", "sub"],
        &["commit", "-q", "-a", "-m", "job work"],
        &["stash", "-q"],
    ] {
        assert_eq!(
            fixture.refused(&git, args),
            format!(
                "refusing to run git in a job tree that holds a nested repository: {}",
                sub.display()
            ),
            "{args:?}"
        );
    }
    assert!(!marker.exists());

    // Once the planted repository is gone, the command runs, with every submodule the work
    // tree's `.gitmodules` names (read from git's standard input) left unchecked.
    fs::remove_dir_all(sub.join(".git")).unwrap();
    fixture.git(
        &tree,
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.only-in-tree.path",
            "elsewhere",
        ],
    );
    let in_job_root = |path: &Path| fixture.roots.contains(path);
    let command = smol::block_on(git.command_with(&["add", "-A"], &in_job_root)).unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    for expected in [
        "submodule.sub.ignore=all",
        "submodule.only-in-tree.ignore=all",
    ] {
        assert!(args.iter().any(|arg| arg == expected), "{args:?}");
    }
    fixture.run_safe(&git, &["add", "-A"]);
    assert!(!marker.exists());
}

/// A repository at `tree/<name>` the job made, holding a committed `f` and a filter, selected
/// for every file by its attributes, that creates `marker` when it runs.
fn plant_nested_repository(fixture: &Fixture, tree: &Path, name: &str, marker: &Path) -> PathBuf {
    let filter = fixture.marker_script(&format!("bin/{name}-filter"), marker, "cat");
    fixture.git(tree, &["init", "-q", name]);
    let nested = tree.join(name);
    fs::write(nested.join("f"), "one\n").unwrap();
    fixture.git(&nested, &["add", "f"]);
    fixture.git(&nested, &["commit", "-q", "-m", "planted"]);
    fixture.git(
        &nested,
        &["config", "filter.planted.clean", &filter.to_string_lossy()],
    );
    fs::write(nested.join(".gitattributes"), "* filter=planted\n").unwrap();
    nested
}

/// A main worktree registered as a job root (without its masked `.git`), with one commit.
fn job_main_worktree(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.git(&fixture.base, &["init", "-q", name]);
    let tree = fixture.base.join(name);
    fixture.git(&tree, &["commit", "-q", "--allow-empty", "-m", "first"]);
    fixture
        .roots
        .register(&tree, &[tree.join(".git")], RootLifetime::Permanent)
        .unwrap();
    tree
}

#[test]
fn safe_git_refuses_a_job_tree_holding_a_repository_the_job_made() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("nested-filter-ran");

    // Plain git: the first `add -A` stages the job's repository as a gitlink, and once the job
    // edits it, the second runs the job's filter.
    let control = fixture.dir("control");
    fixture.git(&control, &["init", "-q", "."]);
    fixture.git(&control, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let nested = plant_nested_repository(&fixture, &control, "evil", &marker);
    fixture.git(&control, &["add", "-A"]);
    fs::write(nested.join("f"), "two\n").unwrap();
    fixture.git(&control, &["add", "-A"]);
    assert!(marker.exists(), "the nested filter was never run by git");
    fs::remove_file(&marker).unwrap();

    let tree = job_main_worktree(&fixture, "tree");
    let nested = plant_nested_repository(&fixture, &tree, "evil", &marker);
    let git = fixture.safe_git_with_identity(&tree.join(".git"), &tree);
    let refusal = format!(
        "refusing to run git in a job tree that holds a nested repository: {}",
        nested.display()
    );
    assert_eq!(fixture.refused(&git, &["add", "-A"]), refusal);
    fs::write(nested.join("f"), "two\n").unwrap();
    assert_eq!(fixture.refused(&git, &["add", "-A"]), refusal);
    assert!(!marker.exists());

    // A repository deeper in the tree, with a `.git` file instead of a directory, is found too.
    fs::remove_dir_all(nested.join(".git")).unwrap();
    fixture.file("tree/a/b/.git", "gitdir: /nowhere\n");
    assert_eq!(
        fixture.refused(&git, &["status"]),
        format!(
            "refusing to run git in a job tree that holds a nested repository: {}",
            tree.join("a/b").display()
        )
    );
    fs::remove_dir_all(tree.join("a")).unwrap();
    fixture.run_safe(&git, &["add", "-A"]);
}

#[test]
fn safe_git_refuses_a_gitlink_with_a_repository_in_the_job_tree() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("gitlink-filter-ran");
    let tree = job_main_worktree(&fixture, "tree");
    // A gitlink with no `.gitmodules` entry, staged before the job edited its repository.
    let nested = plant_nested_repository(&fixture, &tree, "sub", &marker);
    fixture.git(&tree, &["add", "sub"]);
    fixture.git(&tree, &["commit", "-q", "-m", "gitlink"]);
    fs::write(nested.join("f"), "two\n").unwrap();

    // Plain git runs the nested filter for each of these.
    for args in [&["add", "sub"][..], &["add", "-u"], &["add", "."]] {
        fixture.git(&tree, args);
        assert!(marker.exists(), "plain git {args:?} never ran the filter");
        fs::remove_file(&marker).unwrap();
    }

    let git = fixture.safe_git_with_identity(&tree.join(".git"), &tree);
    let refusal = format!(
        "refusing to run git in a job tree that holds a nested repository: {}",
        nested.display()
    );
    for args in [
        &["add", "sub"][..],
        &["add", "-u"],
        &["add", "-A"],
        &["add", "."],
        &["rm", "-n", "sub"],
        &["rm", "--cached", "sub"],
        &["status", "--porcelain"],
    ] {
        assert_eq!(fixture.refused(&git, args), refusal, "{args:?}");
    }
    assert!(!marker.exists());
}

#[test]
fn nested_repository_walk_skips_the_root_git_and_symlinks() {
    let fixture = Fixture::new();
    let tree = fixture.dir("tree");
    fixture.dir("tree/.git/objects");
    fixture.dir("outside/repo/.git");
    fixture.file("tree/src/main.kt", "fun main() {}\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(fixture.base.join("outside"), tree.join("link")).unwrap();
    assert_eq!(
        crate::safe_git::find_nested_repository(&tree).unwrap(),
        None
    );

    fixture.file("tree/src/deep/.GIT", "");
    assert_eq!(
        crate::safe_git::find_nested_repository(&tree).unwrap(),
        Some(tree.join("src/deep"))
    );
}

#[cfg(unix)]
#[test]
fn safe_git_refuses_a_gitmodules_that_is_not_a_regular_file() {
    let fixture = Fixture::new();
    let tree = job_main_worktree(&fixture, "tree");
    let git = fixture.safe_git(&tree.join(".git"), &tree);
    let fifo = fixture.base.join("fifo");
    let fifo_c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    // SAFETY: `fifo_c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

    let expected = format!(
        "refusing a {} that is not a regular file",
        tree.join(".gitmodules").display()
    );
    std::os::unix::fs::symlink(&fifo, tree.join(".gitmodules")).unwrap();
    assert_eq!(fixture.refused(&git, &["status"]), expected);
    fs::remove_file(tree.join(".gitmodules")).unwrap();
    fs::rename(&fifo, tree.join(".gitmodules")).unwrap();
    assert_eq!(fixture.refused(&git, &["status"]), expected);
}

#[test]
fn safe_git_switches_off_merge_drivers_and_refuses_reenabling_options() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("merge-driver-ran");
    let driver = fixture.marker_script("bin/merge", &marker, "exit 0");
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    let host_git = host.join(".git");
    let tree = fixture.dir("tree");
    fixture.git(
        &host,
        &[
            "config",
            "merge.evil.driver",
            &format!("{} %A", driver.display()),
        ],
    );
    fixture.git(
        &host,
        &["config", "diff.evil.command", &driver.to_string_lossy()],
    );

    fixture.file("tree/m.txt", "base\n");
    fixture.plain(&host_git, &tree, &["add", "m.txt"]);
    fixture.plain(&host_git, &tree, &["commit", "-q", "-m", "base"]);
    fixture.plain(&host_git, &tree, &["branch", "side"]);
    fixture.file("tree/m.txt", "main\n");
    fixture.plain(&host_git, &tree, &["commit", "-q", "-a", "-m", "main"]);
    fixture.plain(&host_git, &tree, &["checkout", "-q", "side"]);
    fixture.file("tree/m.txt", "side\n");
    fixture.plain(&host_git, &tree, &["commit", "-q", "-a", "-m", "side"]);
    fixture.plain(&host_git, &tree, &["checkout", "-q", "main"]);
    fixture.file("tree/.gitattributes", "m.txt merge=evil diff=evil\n");

    // Plain git runs the driver the job-written attributes select.
    fixture.plain(&host_git, &tree, &["merge", "-q", "side"]);
    assert!(marker.exists(), "the merge driver was never run by git");
    fs::remove_file(&marker).unwrap();
    fixture.plain(&host_git, &tree, &["reset", "-q", "--hard", "ORIG_HEAD"]);

    fixture.register(&tree);
    let git = fixture.safe_git_with_identity(&host_git, &tree);
    // The emptied driver fails to run, so the merge stops with a conflict.
    let merge = fixture.run_safe_unchecked(&git, &["merge", "-q", "side"]);
    assert!(!merge.status.success());
    assert!(!marker.exists());
    fixture.run_safe(&git, &["merge", "--abort"]);

    for option in [
        "--ext-diff",
        "--ext-d",
        "--textconv",
        "--textc",
        "--recurse-submodules",
        "--recurse-submodules=yes",
        "--ignore-submodules=none",
    ] {
        assert_eq!(
            fixture.refused(&git, &["diff", option]),
            format!("refusing the git option {option} in a job tree")
        );
    }
    fixture.run_safe(&git, &["diff", "--text", "--", "--ext-diff"]);
    assert!(!marker.exists());
}

#[test]
fn safe_git_runs_git_from_an_absolute_path_entry() {
    let fixture = Fixture::new();
    let tree = fixture.dir("tree");
    let bin = fixture.dir("bin");
    let fake = fixture.marker_script("bin/git", &fixture.base.join("unused"), "exit 0");
    fixture.marker_script("tree/rel/git", &fixture.base.join("unused"), "exit 0");
    let git_dir = fixture.dir("host/.git");
    let in_job_root = |_: &Path| false;

    let only_relative = SafeGit::new(&git_dir, &tree).env("PATH", "::rel:./rel");
    let error = smol::block_on(only_relative.command_with(&["status"], &in_job_root)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot find git in an absolute PATH entry"
    );

    let mut path = std::ffi::OsString::from("rel::");
    path.push(&bin);
    let command = smol::block_on(
        SafeGit::new(&git_dir, &tree)
            .env("PATH", &path)
            .command_with(&["status"], &in_job_root),
    )
    .unwrap();
    assert_eq!(command.get_program(), fake.as_os_str());

    let relative_binary = SafeGit::new(&git_dir, &tree).binary("rel/git");
    assert!(smol::block_on(relative_binary.command_with(&["status"], &in_job_root)).is_err());
}

#[test]
fn safe_git_gives_git_only_absolute_path_entries_outside_job_roots() {
    let fixture = Fixture::new();
    let tree = fixture.dir("tree");
    let bin = fixture.dir("bin");
    let job_bin = fixture.dir("job/bin");
    let git_dir = fixture.dir("host/.git");
    // A stand-in git that prints the PATH it was given.
    fixture.marker_script(
        "bin/git",
        &fixture.base.join("ran"),
        "printf '%s' \"$PATH\"",
    );
    fixture.marker_script("job/bin/git", &fixture.base.join("job-ran"), "exit 0");
    fixture.register(&fixture.base.join("job"));
    let in_job_root = |path: &Path| fixture.roots.contains(path);

    let mut path = std::ffi::OsString::from("rel::./x:");
    path.push(&job_bin);
    path.push(":");
    path.push(fixture.base.join("missing"));
    path.push(":");
    path.push(&bin);
    let git = SafeGit::new(&git_dir, &tree).env("PATH", &path);
    let mut command = smol::block_on(git.command_with(&["status"], &in_job_root)).unwrap();
    assert_eq!(command.get_program(), bin.join("git").as_os_str());
    let output = smol::block_on(command.output()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        bin.to_string_lossy()
    );

    let error = smol::block_on(
        SafeGit::new(&git_dir, &tree)
            .binary(job_bin.join("git"))
            .command_with(&["status"], &in_job_root),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "refusing to run a git binary inside a job root: {}",
            job_bin.join("git").display()
        )
    );
    assert!(!fixture.base.join("job-ran").exists());
}

#[test]
fn untrusted_tree_applies_the_job_tree_protections_outside_every_job_root() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("filter-ran");
    let filter = fixture.marker_script("bin/filter", &marker, "cat");
    let filter = filter.to_string_lossy().into_owned();
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    let tree = fixture.dir("tree");
    fixture.file("tree/.gitattributes", "*.txt filter=x\n");
    fixture.file("tree/file.txt", "one\n");
    let host_git = host.join(".git");
    fixture.git(&host, &["config", "filter.x.clean", &filter]);
    let add = ["add", "-A", "--ignore-errors", "."];

    // Nothing is registered, so a plain SafeGit leaves the configured filter on.
    fixture.run_safe(&fixture.safe_git(&host_git, &tree), &add);
    assert!(marker.exists(), "the filter should run without the flag");
    fs::remove_file(&marker).unwrap();
    fs::remove_file(host_git.join("index")).unwrap();

    fixture.run_safe(&fixture.safe_git(&host_git, &tree).untrusted_tree(), &add);
    assert!(!marker.exists());

    // The nested repository refusal applies too.
    fixture.dir("tree/inner");
    fixture.git(&fixture.base, &["init", "-q", "tree/inner"]);
    let error = fixture.refused(&fixture.safe_git(&host_git, &tree).untrusted_tree(), &add);
    assert!(error.contains("nested repository"), "{error}");
}

#[test]
fn linked_git_dir_is_refused_when_its_commondir_names_another_repository() {
    let fixture = Fixture::new();
    let host = fixture.dir("host");
    fixture.git(&fixture.base, &["init", "-q", "host"]);
    fixture.git(&host, &["commit", "-q", "--allow-empty", "-m", "first"]);
    fixture.git(&fixture.base, &["init", "-q", "other"]);
    let linked = fixture.base.join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    fixture.git(&host, &["worktree", "add", "-q", "--detach", &linked_arg]);
    let git_dir = host.join(".git/worktrees/linked");
    assert!(linked_git_dir_from_record(&linked, &host.join(".git")).is_ok());

    // Git would read the other repository's configuration, refs and objects through this.
    fs::write(git_dir.join("commondir"), "../../../other/.git\n").unwrap();
    let error = linked_git_dir_from_record(&linked, &host.join(".git")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("its commondir names another common directory"),
        "{error}"
    );
}

#[test]
fn allow_nested_repositories_runs_commands_that_stay_out_of_the_nested_repository() {
    let fixture = Fixture::new();
    let marker = fixture.base.join("nested-filter-ran");
    let tree = job_main_worktree(&fixture, "tree");
    let nested = plant_nested_repository(&fixture, &tree, "sub", &marker);
    fixture.git(&tree, &["add", "sub"]);
    fixture.git(&tree, &["commit", "-q", "-m", "gitlink"]);
    fs::write(nested.join("f"), "two\n").unwrap();

    // Plain git's status checks the gitlink with a status inside it, which runs the filter.
    fixture.git(&tree, &["status", "--porcelain"]);
    assert!(marker.exists(), "plain git status never ran the filter");
    fs::remove_file(&marker).unwrap();

    let git = fixture.safe_git(&tree.join(".git"), &tree);
    let status = ["status", "--porcelain"];
    assert!(fixture.refused(&git, &status).contains("nested repository"));
    let allowed = git.allow_nested_repositories();
    for args in [
        &status[..],
        &["diff", "HEAD", "--"],
        &["ls-files", "--others", "--exclude-standard"],
        &["write-tree"],
    ] {
        fixture.run_safe(&allowed, args);
    }
    assert!(!marker.exists(), "a nested filter ran");
}
