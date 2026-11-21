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
        let in_job_root = |path: &Path| self.roots.contains(path);
        let mut command = smol::block_on(git.command_with(args, &in_job_root)).unwrap();
        let output = smol::block_on(command.output()).unwrap();
        assert!(
            output.status.success(),
            "safe git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
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
}
