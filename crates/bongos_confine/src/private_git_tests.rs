use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use crate::{
    private_git::{
        BUSY, DETACHED, MARKER, Marker, PrivateGit, RecoverOutcome, UNBORN, prepare_with, recover,
    },
    safe_git::SafeGit,
};

/// A pid no process can have: above the largest `pid_max` Linux allows (2^22).
const DEAD_PID: u32 = 2_000_000_000;

/// A shared repository with one commit on `main`, an annotated tag, `origin/main` (with
/// `origin/HEAD` pointing at it), and a linked worktree `tree` on the branch `feature`; plus an
/// empty home and a scratch directory, all in a fresh temporary directory.
struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    main: PathBuf,
    tree: PathBuf,
    linked: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_init_args(&[])
    }

    fn with_init_args(init_args: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        let main = base.join("main");
        let scratch = base.join("scratch");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        let mut fixture = Self {
            _temp: temp,
            base: base.clone(),
            home,
            main: main.clone(),
            tree: base.join("tree"),
            linked: PathBuf::new(),
            scratch,
        };
        let mut init = vec!["init"];
        init.extend_from_slice(init_args);
        fixture.git(&main, &init);
        fs::write(main.join("a.txt"), "one\n").unwrap();
        fs::write(main.join(".gitignore"), "ignored.txt\n").unwrap();
        fixture.git(&main, &["add", "a.txt", ".gitignore"]);
        fixture.git(&main, &["commit", "-q", "-m", "first"]);
        fixture.git(&main, &["tag", "-a", "v1", "-m", "v1"]);
        fixture.git(&main, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        fixture.git(
            &main,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        fs::write(main.join(".git/info/exclude"), "excluded.txt\n").unwrap();
        let tree = fixture.tree.clone();
        fixture.git(
            &main,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-q"),
                OsStr::new("-b"),
                OsStr::new("feature"),
                tree.as_os_str(),
            ],
        );
        fixture.linked = fs::canonicalize(main.join(".git/worktrees/tree")).unwrap();
        fixture
    }

    /// Real git with `args` in `directory`, isolated from the user's and system's configuration.
    fn git_unchecked<S: AsRef<OsStr>>(&self, directory: &Path, args: &[S]) -> Output {
        let mut command = util::command::new_command("git");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
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
                "-c",
                "tag.gpgsign=false",
                "-c",
                "gc.auto=0",
            ])
            .args(args);
        smol::block_on(command.output()).unwrap()
    }

    /// [`Fixture::git_unchecked`], asserting git succeeded.
    fn git<S: AsRef<OsStr>>(&self, directory: &Path, args: &[S]) -> Output {
        let output = self.git_unchecked(directory, args);
        assert!(
            output.status.success(),
            "git {:?} in {} failed: {}",
            args.iter().map(|arg| arg.as_ref()).collect::<Vec<_>>(),
            directory.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_text(&self, directory: &Path, args: &[&str]) -> String {
        String::from_utf8(self.git(directory, args).stdout).unwrap()
    }

    fn prepare_using(&self, binary: Option<&Path>) -> anyhow::Result<PrivateGit> {
        let home = self.home.clone();
        let binary = binary.map(Path::to_path_buf);
        let configure = move |git: SafeGit| {
            let git = git
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("HOME", &home);
            match &binary {
                Some(binary) => git.binary(binary),
                None => git,
            }
        };
        smol::block_on(prepare_with(
            &self.tree,
            &self.linked,
            &self.scratch,
            &configure,
        ))
    }

    fn prepare(&self) -> PrivateGit {
        self.prepare_using(None).unwrap()
    }

    fn private_dir(&self) -> PathBuf {
        self.scratch.join("git")
    }

    fn dot_git(&self) -> Vec<u8> {
        fs::read(self.tree.join(".git")).unwrap()
    }

    /// Every ref of the shared repository, the linked worktree's `HEAD` and index, and every
    /// file in the shared object directory.
    fn shared_state(&self) -> (String, Vec<u8>, Vec<u8>, Vec<PathBuf>) {
        let refs = self.git_text(
            &self.main,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        );
        let head = fs::read(self.linked.join("HEAD")).unwrap();
        let index = fs::read(self.linked.join("index")).unwrap_or_default();
        (
            refs,
            head,
            index,
            files_under(&self.main.join(".git/objects")),
        )
    }

    /// Writes `marker` as the tree's marker, as a crashed owner would have left it.
    fn plant_marker(&self, marker: &Marker) {
        fs::write(self.linked.join(MARKER), marker.encode()).unwrap();
    }

    /// The marker a dead owner left after taking the tree, before anything else.
    fn dead_marker(&self) -> Marker {
        Marker {
            pid: DEAD_PID,
            start_time: 1,
            tree: self.tree.clone(),
            private_dir: self.private_dir(),
            original_dot_git: self.dot_git(),
            branch: None,
            snapshot: None,
        }
    }

    /// Makes the owner of the tree's marker a process that is gone.
    fn kill_owner(&self) {
        let mut marker = Marker::read(&self.linked).unwrap().unwrap();
        marker.pid = DEAD_PID;
        marker.replace(&self.linked).unwrap();
    }

    fn assert_switched_back(&self, original: &[u8]) {
        assert_eq!(self.dot_git(), original);
        assert!(fs::symlink_metadata(self.private_dir()).is_err());
        assert!(fs::symlink_metadata(self.linked.join(MARKER)).is_err());
    }

    fn write_tree_file(&self, name: &str, contents: &str) {
        fs::write(self.tree.join(name), contents).unwrap();
    }
}

/// Every file below `dir`, relative to it, sorted; symlinks are listed, not followed.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path().strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    files.sort();
    files
}

/// Every directory below `dir`, relative to it, sorted.
fn dirs_under(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
                dirs.push(entry.path().strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    dirs.sort();
    dirs
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn private_git_commit_and_branch_land_privately() {
    let fixture = Fixture::new();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    assert_eq!(private.branch(), "refs/heads/feature");
    assert_eq!(private.private_dir(), fixture.private_dir());
    assert_eq!(
        fixture.dot_git(),
        format!("gitdir: {}\n", fixture.private_dir().display()).into_bytes()
    );
    let tree = &fixture.tree;
    assert_eq!(fixture.git_text(tree, &["status", "--porcelain"]), "");

    fixture.write_tree_file("b.txt", "two\n");
    fixture.write_tree_file("excluded.txt", "x\n");
    fixture.write_tree_file("ignored.txt", "x\n");
    assert_eq!(
        fixture.git_text(tree, &["status", "--porcelain"]),
        "?? b.txt\n",
        "the shared info/exclude and .gitignore still apply"
    );
    fixture.git(tree, &["add", "b.txt"]);
    fixture.git(tree, &["commit", "-q", "-m", "second"]);
    fixture.git(tree, &["branch", "side"]);
    fixture.git(tree, &["tag", "v2"]);
    assert_eq!(fixture.git_text(tree, &["status", "--porcelain"]), "");

    // The job's history and refs are in the private directory.
    let private_dir = fixture.private_dir();
    let commit = fixture.git_text(tree, &["rev-parse", "HEAD"]);
    assert_eq!(
        fs::read_to_string(private_dir.join("refs/heads/feature")).unwrap(),
        commit
    );
    assert_eq!(
        fs::read_to_string(private_dir.join("refs/heads/side")).unwrap(),
        commit
    );
    assert!(private_dir.join("refs/tags/v2").is_file());
    assert!(
        files_under(&private_dir.join("objects"))
            .iter()
            .any(|file| file.starts_with(&commit[..2])),
        "the new commit is a loose object of the private directory"
    );

    // The snapshot answers the usual questions.
    assert_eq!(
        fixture.git_text(tree, &["diff", "--name-only", "origin/main"]),
        "b.txt\n"
    );
    assert_eq!(
        fixture.git_text(tree, &["diff", "--name-only", "origin"]),
        "b.txt\n"
    );
    let tagged = fixture.git_text(tree, &["rev-parse", "v1^{commit}"]);
    let first = fixture.git_text(tree, &["rev-parse", "HEAD~1"]);
    assert_eq!(tagged, first);
    assert_eq!(
        fixture.git_text(tree, &["log", "--format=%s"]),
        "second\nfirst\n"
    );
    fixture.git(tree, &["fsck", "--no-progress", "--connectivity-only"]);

    // The shared repository did not change.
    assert_eq!(fixture.shared_state(), before);
    assert!(
        fixture
            .git_unchecked(
                &fixture.main,
                &["rev-parse", "--verify", "-q", "refs/heads/side"]
            )
            .stdout
            .is_empty()
    );
    let marker = Marker::read(&fixture.linked).unwrap().unwrap();
    assert_eq!(marker.snapshot.as_ref(), Some(private.snapshot()));
    assert_eq!(marker.branch.as_deref(), Some("refs/heads/feature"));
    private.abandon().unwrap();
}

#[test]
fn private_git_writes_no_object() {
    let fixture = Fixture::new();
    let private = fixture.prepare();
    let objects = fixture.private_dir().join("objects");
    assert_eq!(
        files_under(&objects),
        vec![PathBuf::from("info/alternates")]
    );
    assert_eq!(
        dirs_under(&objects),
        vec![PathBuf::from("info"), PathBuf::from("pack")]
    );
    assert_eq!(
        fs::read_to_string(objects.join("info/alternates")).unwrap(),
        format!("{}\n", fixture.main.join(".git/objects").display())
    );
    // Nothing that looks like an object or pack anywhere else either.
    for file in files_under(&fixture.private_dir()) {
        let name = file.to_string_lossy();
        assert!(
            !name.ends_with(".pack") && !name.ends_with(".idx"),
            "{name}"
        );
    }
    let config = fs::read_to_string(fixture.private_dir().join("config")).unwrap();
    assert!(config.contains("bare = false"), "{config}");
    assert!(config.contains("logallrefupdates = false"), "{config}");
    assert!(!config.contains("objectformat"), "{config}");
    assert!(!fixture.private_dir().join("commondir").exists());
    private.abandon().unwrap();
}

#[test]
fn private_git_second_prepare_refuses() {
    let fixture = Fixture::new();
    let private = fixture.prepare();
    let redirected = fixture.dot_git();
    let marker = fs::read(fixture.linked.join(MARKER)).unwrap();

    let error = fixture.prepare_using(None).unwrap_err();
    assert!(format!("{error:#}").contains(BUSY), "{error:#}");
    assert_eq!(fixture.dot_git(), redirected);
    assert_eq!(fs::read(fixture.linked.join(MARKER)).unwrap(), marker);
    assert!(fixture.private_dir().join("HEAD").is_file());
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::InUse);
    assert_eq!(fs::read(fixture.linked.join(MARKER)).unwrap(), marker);

    let original = private.original_dot_git().to_vec();
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_recovers_after_crash() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.git(
        &fixture.tree,
        &["commit", "-q", "--allow-empty", "-m", "job"],
    );
    // The owner dies without switching back.
    std::mem::forget(private);
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::InUse);
    fixture.kill_owner();

    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::Recovered);
    fixture.assert_switched_back(&original);
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::NoMarker);
    fixture.assert_switched_back(&original);
    assert_eq!(fixture.shared_state(), before);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );
}

#[cfg(target_os = "linux")]
#[test]
fn private_git_recovers_owner_with_reused_pid() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    std::mem::forget(private);
    // A live process with the owner's pid that started at another time is not the owner.
    let mut marker = Marker::read(&fixture.linked).unwrap().unwrap();
    marker.start_time += 1;
    marker.replace(&fixture.linked).unwrap();
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::Recovered);
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_recovers_crash_between_marker_and_redirect() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    // The owner took the tree and began the private directory, then died.
    fixture.plant_marker(&fixture.dead_marker());
    fs::create_dir_all(fixture.private_dir().join("objects")).unwrap();
    fs::write(fixture.private_dir().join("HEAD"), "partial").unwrap();

    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::Recovered);
    fixture.assert_switched_back(&original);
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::NoMarker);
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_recovery_refuses_unparseable_marker() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let mut text = fixture.dead_marker().encode();
    text.truncate(text.len() - "end\n".len());
    fs::write(fixture.linked.join(MARKER), &text).unwrap();
    fs::create_dir_all(fixture.private_dir()).unwrap();

    assert!(recover(&fixture.linked).is_err());
    assert!(fixture.prepare_using(None).is_err());
    // Nothing was guessed at.
    assert_eq!(fs::read(fixture.linked.join(MARKER)).unwrap(), text);
    assert!(fixture.private_dir().is_dir());
    assert_eq!(fixture.dot_git(), original);
}

#[test]
fn private_git_live_owner_refuses_dead_owner_recovered() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let mut live = fixture.dead_marker();
    live.pid = std::process::id();
    live.start_time = util::job_roots::process_start_time(live.pid).unwrap_or_default();
    fixture.plant_marker(&live);

    let error = fixture.prepare_using(None).unwrap_err();
    assert!(format!("{error:#}").contains(BUSY), "{error:#}");
    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::InUse);
    assert_eq!(Marker::read(&fixture.linked).unwrap(), Some(live));
    assert_eq!(fixture.dot_git(), original);

    fixture.plant_marker(&fixture.dead_marker());
    let private = fixture.prepare();
    let marker = Marker::read(&fixture.linked).unwrap().unwrap();
    assert_eq!(marker.pid, std::process::id());
    assert_eq!(marker.original_dot_git, original);
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
}

#[cfg(unix)]
#[test]
fn private_git_symlinks_in_scratch_are_not_followed() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let outside = fixture.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("sentinel"), "keep").unwrap();
    symlink(&outside, &fixture.private_dir());

    let private = fixture.prepare();
    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "keep"
    );
    assert_eq!(files_under(&outside), vec![PathBuf::from("sentinel")]);
    let metadata = fs::symlink_metadata(fixture.private_dir()).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());

    // A symlink the job leaves inside the private directory is unlinked, not followed.
    let inside = fixture.base.join("inside");
    fs::create_dir_all(&inside).unwrap();
    fs::write(inside.join("sentinel"), "keep").unwrap();
    symlink(&inside, &fixture.private_dir().join("refs/link"));
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
    assert_eq!(fs::read_to_string(inside.join("sentinel")).unwrap(), "keep");
    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "keep"
    );

    // The same inside a stale private directory a fresh prepare replaces.
    fs::create_dir_all(fixture.private_dir()).unwrap();
    symlink(&inside, &fixture.private_dir().join("objects"));
    let private = fixture.prepare();
    assert_eq!(files_under(&inside), vec![PathBuf::from("sentinel")]);
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_keeps_sha256() {
    let fixture = Fixture::with_init_args(&["--object-format=sha256"]);
    let before = fixture.shared_state();
    let private = fixture.prepare();
    let config = fs::read_to_string(fixture.private_dir().join("config")).unwrap();
    assert!(config.contains("objectformat = sha256"), "{config}");
    assert!(config.contains("repositoryformatversion = 1"), "{config}");
    assert_eq!(
        fixture.git_text(&fixture.tree, &["rev-parse", "--show-object-format"]),
        "sha256\n"
    );
    fixture.write_tree_file("b.txt", "two\n");
    fixture.git(&fixture.tree, &["add", "b.txt"]);
    fixture.git(&fixture.tree, &["commit", "-q", "-m", "second"]);
    let head = fixture.git_text(&fixture.tree, &["rev-parse", "HEAD"]);
    assert_eq!(head.trim().len(), 64);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["diff", "--name-only", "origin/main"]),
        "b.txt\n"
    );
    assert_eq!(fixture.shared_state(), before);
    private.abandon().unwrap();
}

#[cfg(unix)]
#[test]
fn private_git_failed_snapshot_undoes_everything() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let script = fixture.base.join("bin/git");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(
        &script,
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = for-each-ref ]; then\n    \
         echo 'for-each-ref broken' >&2\n    exit 2\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let error = fixture.prepare_using(Some(&script)).unwrap_err();
    assert!(
        format!("{error:#}").contains("for-each-ref broken"),
        "{error:#}"
    );
    fixture.assert_switched_back(&original);
    assert_eq!(fixture.shared_state(), before);
    // The tree is free again.
    let private = fixture.prepare();
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_unborn_branch_refuses() {
    let fixture = Fixture::new();
    fixture.git(&fixture.tree, &["symbolic-ref", "HEAD", "refs/heads/fresh"]);
    let original = fixture.dot_git();
    let error = fixture.prepare_using(None).unwrap_err();
    assert!(format!("{error:#}").contains(UNBORN), "{error:#}");
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_detached_head_refuses() {
    let fixture = Fixture::new();
    fixture.git(&fixture.tree, &["checkout", "-q", "--detach"]);
    let original = fixture.dot_git();
    let error = fixture.prepare_using(None).unwrap_err();
    assert!(format!("{error:#}").contains(DETACHED), "{error:#}");
    fixture.assert_switched_back(&original);
}

#[test]
fn private_git_no_index_gets_none() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.linked.join("index")).unwrap();
    let private = fixture.prepare();
    assert!(!fixture.private_dir().join("index").exists());
    private.abandon().unwrap();

    // With an index, the private one is a copy.
    let fixture = Fixture::new();
    let index = fs::read(fixture.linked.join("index")).unwrap();
    let private = fixture.prepare();
    assert_eq!(
        fs::read(fixture.private_dir().join("index")).unwrap(),
        index
    );
    private.abandon().unwrap();
}

#[test]
fn private_git_hand_off_switches_back() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();

    // Switching over and back with no change leaves a clean tree.
    let private = fixture.prepare();
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );

    // A commit made while switched stays private; back on shared git only its file shows, as an
    // uncommitted change (importing it is not part of switching back).
    let private = fixture.prepare();
    fixture.write_tree_file("b.txt", "two\n");
    fixture.git(&fixture.tree, &["add", "b.txt"]);
    fixture.git(&fixture.tree, &["commit", "-q", "-m", "job"]);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );
    private.abandon().unwrap();
    fixture.assert_switched_back(&original);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        "?? b.txt\n"
    );
    assert_eq!(
        fixture.git_text(&fixture.tree, &["log", "--format=%s"]),
        "first\n"
    );
    assert_eq!(fixture.shared_state(), before);
}

#[test]
fn private_git_marker_round_trips() {
    let fixture = Fixture::new();
    let mut marker = fixture.dead_marker();
    assert_eq!(Marker::decode(&marker.encode()).unwrap(), marker);
    marker.branch = Some("refs/heads/feature".to_string());
    marker.snapshot = Some(
        [(
            "refs/heads/feature".to_string(),
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    assert_eq!(Marker::decode(&marker.encode()).unwrap(), marker);
    assert!(Marker::decode(b"").is_err());
    assert!(Marker::decode(b"bongos-confined 1\nend\n").is_err());
}
