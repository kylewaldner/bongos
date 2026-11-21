use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use crate::{
    private_git::{
        BUSY, DETACHED, ImportReport, MARKER, Marker, PrivateGit, RecoverOutcome, UNBORN,
        next_temporary_counter, prepare_with, recover,
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

    /// Commits `name` with `contents` in the tree (with whatever git directory it has now) and
    /// returns the new commit.
    fn commit_file(&self, name: &str, contents: &str, message: &str) -> String {
        self.write_tree_file(name, contents);
        self.git(&self.tree, &["add", name]);
        self.git(&self.tree, &["commit", "-q", "-m", message]);
        self.rev(&self.tree, "HEAD")
    }

    /// The object id `rev` names in `directory`.
    fn rev(&self, directory: &Path, rev: &str) -> String {
        self.git_text(directory, &["rev-parse", "--verify", rev])
            .trim()
            .to_string()
    }

    /// The object id of the shared ref `name`, or `None`.
    fn shared_ref(&self, name: &str) -> Option<String> {
        let output = self.git_unchecked(&self.main, &["rev-parse", "--verify", "-q", name]);
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_string())
    }

    /// Imports through host git isolated like the fixture's, with the scratch and the tree as
    /// the job roots.
    fn import(&self, private: PrivateGit) -> anyhow::Result<ImportReport> {
        let git = SafeGit::without_work_tree(&self.linked)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", &self.home);
        let scratch = self.scratch.clone();
        let tree = self.tree.clone();
        let in_job_root = move |path: &Path| path.starts_with(&scratch) || path.starts_with(&tree);
        smol::block_on(private.import_with(&git, &in_job_root))
    }

    /// The tree is back on shared git, the import directory is gone and the marker released;
    /// the private directory is kept exactly when `kept`.
    fn assert_imported_back(&self, original: &[u8], kept: bool) {
        assert_eq!(self.dot_git(), original);
        assert!(fs::symlink_metadata(self.linked.join(MARKER)).is_err());
        assert!(fs::symlink_metadata(self.linked.join("bongos-import")).is_err());
        assert_eq!(self.private_dir().is_dir(), kept);
    }

    /// The tree's index matches its `HEAD`.
    fn assert_index_matches_head(&self) {
        self.git(&self.tree, &["diff", "--cached", "--quiet"]);
    }

    /// The pack files of the shared repository.
    fn shared_packs(&self) -> Vec<PathBuf> {
        files_under(&self.main.join(".git/objects/pack"))
            .into_iter()
            .filter(|file| {
                file.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .collect()
    }

    /// The loose object files of the shared repository.
    fn shared_loose_objects(&self) -> Vec<PathBuf> {
        files_under(&self.main.join(".git/objects"))
            .into_iter()
            .filter(|file| {
                file.components().count() == 2
                    && file.to_string_lossy().len() > 3
                    && !file.starts_with("pack")
                    && !file.starts_with("info")
            })
            .collect()
    }

    /// Writes an executable script through a child process, so this process never holds a write
    /// descriptor on it (another test thread's forked child could inherit one).
    fn write_script(&self, path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = util::command::new_command("sh");
        writer
            .args(["-c", "printf '%s' \"$1\" > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .arg(body);
        assert!(smol::block_on(writer.output()).unwrap().status.success());
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
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let script = fixture.base.join("bin/git");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    // Created by a child process (script text passed as an argument), so this process never holds a write descriptor on the script:
    // another test thread's forked child could inherit one and make the exec fail with ETXTBSY.
    let body = "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = for-each-ref ]; then\n    \
                echo 'for-each-ref broken' >&2\n    exit 2\n  fi\ndone\nexec git \"$@\"\n";
    let mut writer = util::command::new_command("sh");
    writer
        .args(["-c", "printf '%s' \"$1\" > \"$0\" && chmod 755 \"$0\""])
        .arg(&script)
        .arg(body);
    assert!(smol::block_on(writer.output()).unwrap().status.success());

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

#[cfg(unix)]
#[test]
fn private_git_symlinked_dot_git_refuses() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside-git");
    fs::write(&outside, fixture.dot_git()).unwrap();
    let dot_git = fixture.tree.join(".git");
    fs::remove_file(&dot_git).unwrap();
    symlink(&outside, &dot_git);
    let shared = fixture.shared_state();

    assert!(fixture.prepare_using(None).is_err());
    assert!(
        fs::symlink_metadata(&dot_git)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&outside).unwrap(), fs::read(&dot_git).unwrap());
    assert!(!fixture.linked.join(MARKER).exists());
    assert!(!fixture.private_dir().exists());
    assert_eq!(fixture.shared_state(), shared);
}

#[cfg(unix)]
#[test]
fn private_git_symlinked_marker_is_refused() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let elsewhere = fixture.base.join("marker-elsewhere");
    fs::write(&elsewhere, fixture.dead_marker().encode()).unwrap();
    symlink(&elsewhere, &fixture.linked.join(MARKER));
    fs::create_dir_all(fixture.private_dir()).unwrap();

    assert!(recover(&fixture.linked).is_err());
    assert!(fixture.prepare_using(None).is_err());
    assert!(
        fs::symlink_metadata(fixture.linked.join(MARKER))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fixture.private_dir().is_dir());
    assert_eq!(fixture.dot_git(), original);
    assert!(elsewhere.is_file());
}

#[cfg(unix)]
#[test]
fn private_git_predictable_temporary_name_is_not_followed() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let sentinel = fixture.base.join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    // Plant a symlink at every temporary name this process could pick next to `.git`.
    let start = next_temporary_counter();
    for n in start..start + 64 {
        symlink(
            &sentinel,
            &fixture
                .tree
                .join(format!("..git.bongos-{}-{n}", std::process::id())),
        );
    }
    let private = fixture.prepare();
    private.abandon().unwrap();
    assert_eq!(fixture.dot_git(), original);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn private_git_recovers_dot_git_replaced_by_fifo() {
    use std::os::unix::ffi::OsStrExt as _;

    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    std::mem::forget(private);
    let dot_git = fixture.tree.join(".git");
    fs::remove_file(&dot_git).unwrap();
    let c_path = std::ffi::CString::new(dot_git.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    fixture.kill_owner();

    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::Recovered);
    fixture.assert_switched_back(&original);
    assert!(fs::symlink_metadata(&dot_git).unwrap().is_file());
}

#[test]
fn private_git_recovers_dot_git_replaced_by_directory() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    std::mem::forget(private);
    let dot_git = fixture.tree.join(".git");
    fs::remove_file(&dot_git).unwrap();
    fs::create_dir_all(dot_git.join("sub")).unwrap();
    fs::write(dot_git.join("sub/file"), "junk").unwrap();
    fixture.kill_owner();

    assert_eq!(recover(&fixture.linked).unwrap(), RecoverOutcome::Recovered);
    fixture.assert_switched_back(&original);
    assert!(fs::symlink_metadata(&dot_git).unwrap().is_file());
}

#[test]
fn private_git_import_takes_the_own_branch_advanced() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "two\n", "second");
    let tip = fixture.commit_file("c.txt", "three\n", "third");

    let report = fixture.import(private).unwrap();
    assert_eq!(
        report,
        ImportReport {
            imported: vec!["refs/heads/feature".to_string()],
            ..ImportReport::default()
        }
    );
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip));
    assert_eq!(
        fixture.git_text(&fixture.main, &["log", "--format=%s", "feature"]),
        "third\nsecond\nfirst\n"
    );
    fixture.assert_index_matches_head();
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );
}

#[test]
fn private_git_import_takes_the_own_branch_rewritten() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let first = fixture.rev(&fixture.main, "feature");
    let private = fixture.prepare();
    // Amending the snapshot's own commit: the new tip does not descend from the old one.
    fixture.write_tree_file("b.txt", "two\n");
    fixture.git(&fixture.tree, &["add", "b.txt"]);
    fixture.git(
        &fixture.tree,
        &["commit", "-q", "--amend", "-m", "rewritten"],
    );
    let tip = fixture.rev(&fixture.tree, "HEAD");
    assert!(
        !fixture
            .git_unchecked(
                &fixture.tree,
                &["merge-base", "--is-ancestor", &first, &tip]
            )
            .status
            .success()
    );

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert!(report.refused.is_empty(), "{report:?}");
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip));
    assert_eq!(fixture.shared_ref("refs/heads/main"), Some(first));
    fixture.assert_index_matches_head();
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );
}

#[test]
fn private_git_import_creates_a_new_branch_head_was_on() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let feature = fixture.rev(&fixture.main, "feature");
    let index = fs::read(fixture.linked.join("index")).unwrap();
    let private = fixture.prepare();
    fixture.git(&fixture.tree, &["checkout", "-q", "-b", "topic"]);
    let tip = fixture.commit_file("b.txt", "two\n", "on topic");

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/topic".to_string()]);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(
        report.notes,
        vec!["HEAD was on topic; the tree is back on feature".to_string()]
    );
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/topic"), Some(tip));
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(feature));
    // The own branch did not move, so its index is the one from before the job.
    assert_eq!(fs::read(fixture.linked.join("index")).unwrap(), index);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature\n"
    );
}

#[test]
fn private_git_import_refuses_a_tag_and_a_moved_foreign_branch() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let main = fixture.rev(&fixture.main, "main");
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    fixture.git(&fixture.tree, &["tag", "v2"]);
    fixture.git(&fixture.tree, &["branch", "-f", "main", "HEAD"]);

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert_eq!(
        report.refused,
        vec![
            "not imported: refs/heads/main (a branch of the shared repository other than feature \
             moved)"
                .to_string(),
            "not imported: refs/tags/v2 (only commits and branches travel back)".to_string(),
        ]
    );
    assert_eq!(report.kept_private_dir, Some(fixture.private_dir()));
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip.clone()));
    assert_eq!(fixture.shared_ref("refs/heads/main"), Some(main));
    assert_eq!(fixture.shared_ref("refs/tags/v2"), None);
    // The kept directory still holds the refused refs, and the next prepare replaces it.
    assert!(fixture.private_dir().join("refs/tags/v2").is_file());
    let private = fixture.prepare();
    assert!(!fixture.private_dir().join("refs/tags/v2").exists());
    assert_eq!(fixture.rev(&fixture.tree, "HEAD"), tip);
    private.abandon().unwrap();
}

#[cfg(unix)]
#[test]
fn private_git_import_never_runs_private_fsmonitor_or_hooks() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    fixture.git(&fixture.tree, &["checkout", "-q", "-b", "topic"]);

    // Planted after the job's last git command, as a job's last act.
    let ran = fixture.base.join("ran");
    let script = fixture.base.join("bin/planted");
    fixture.write_script(
        &script,
        &format!("#!/bin/sh\necho \"$0\" >> '{}'\nexit 0\n", ran.display()),
    );
    let hooks = fixture.private_dir().join("hooks");
    for hook in [
        "reference-transaction",
        "post-checkout",
        "post-index-change",
        "pre-commit",
        "post-rewrite",
    ] {
        fixture.write_script(
            &hooks.join(hook),
            &format!("#!/bin/sh\necho {hook} >> '{}'\nexit 0\n", ran.display()),
        );
    }
    let config = fixture.private_dir().join("config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n[filter \"x\"]\n\tclean = {}\n",
        script.display(),
        hooks.display(),
        script.display()
    ));
    fs::write(&config, text).unwrap();
    fs::write(fixture.tree.join(".gitattributes"), "* filter=x\n").unwrap();

    let report = fixture.import(private).unwrap();
    assert_eq!(
        report.imported,
        vec![
            "refs/heads/feature".to_string(),
            "refs/heads/topic".to_string()
        ]
    );
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip));
    fixture.git(&fixture.tree, &["status", "--porcelain"]);
    assert!(!ran.exists(), "{}", fs::read_to_string(&ran).unwrap());
}

#[cfg(unix)]
#[test]
fn private_git_import_refuses_a_symlinked_loose_ref() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    let outside = fixture.base.join("outside-ref");
    fs::write(&outside, format!("{tip}\n")).unwrap();
    symlink(&outside, &fixture.private_dir().join("refs/heads/linked"));

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert_eq!(
        report.refused,
        vec![
            "not imported: refs/heads/linked (a symlink or special file, not a ref file)"
                .to_string()
        ]
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_ref("refs/heads/linked"), None);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip));
}

#[test]
fn private_git_import_refuses_changed_alternates() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "two\n", "second");
    let alternates = fixture.private_dir().join("objects/info/alternates");
    let mut text = fs::read_to_string(&alternates).unwrap();
    text.push_str(&format!("{}\n", fixture.base.join("elsewhere").display()));
    fs::write(&alternates, text).unwrap();

    let error = fixture.import(private).unwrap_err();
    assert!(
        format!("{error:#}").contains("objects/info/alternates was changed"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("nothing was imported"),
        "{error:#}"
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_state(), before);
}

#[test]
fn private_git_import_refuses_a_planted_commit_graph() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "two\n", "second");
    fixture.git(&fixture.tree, &["commit-graph", "write", "--reachable"]);
    assert!(
        fixture
            .private_dir()
            .join("objects/info/commit-graph")
            .is_file()
    );

    let error = fixture.import(private).unwrap_err();
    assert!(
        format!("{error:#}").contains("objects/info/commit-graph is not allowed"),
        "{error:#}"
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_state(), before);
}

#[test]
fn private_git_import_refuses_a_detached_head() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.git(&fixture.tree, &["checkout", "-q", "--detach"]);
    let detached = fixture.commit_file("b.txt", "two\n", "detached");

    let report = fixture.import(private).unwrap();
    assert!(report.imported.is_empty(), "{report:?}");
    assert_eq!(
        report.refused,
        vec![format!(
            "HEAD detached at {}: not on a branch, not imported",
            &detached[..12]
        )]
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_state(), before);
}

#[test]
fn private_git_import_notes_a_detached_head_on_an_imported_commit() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    fixture.commit_file("c.txt", "three\n", "third");
    fixture.git(&fixture.tree, &["checkout", "-q", "--detach", "HEAD~1"]);

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(
        report.notes,
        vec![format!(
            "HEAD was detached at {}; the tree is back on feature",
            &tip[..12]
        )]
    );
    fixture.assert_imported_back(&original, false);
}

#[test]
fn private_git_import_fails_when_the_branch_moved_meanwhile() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "two\n", "job");
    // Someone moves the branch in the shared repository while the job runs.
    let other = fixture
        .git_text(
            &fixture.main,
            &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "other"],
        )
        .trim()
        .to_string();
    fixture.git(&fixture.main, &["update-ref", "refs/heads/feature", &other]);
    let before = fixture.git_text(
        &fixture.main,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );

    let error = fixture.import(private).unwrap_err();
    assert!(
        format!("{error:#}").contains("branch feature moved while the job ran; nothing imported"),
        "{error:#}"
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(
        fixture.git_text(
            &fixture.main,
            &["for-each-ref", "--format=%(objectname) %(refname)"]
        ),
        before
    );
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(other));
}

#[test]
fn private_git_import_moves_only_new_objects() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    for number in 0..30 {
        fixture.commit_file(
            &format!("file{number}.txt"),
            &format!("history {number}\n"),
            &format!("history {number}"),
        );
    }
    let old = fixture.rev(&fixture.tree, "HEAD");
    let history: Vec<String> = fixture
        .git_text(&fixture.main, &["rev-list", "--objects", &old])
        .lines()
        .map(|line| line[..40].to_string())
        .collect();
    assert!(history.len() > 90);
    let private = fixture.prepare();
    let tip = fixture.commit_file("new.txt", "one new file\n", "the job's commit");
    let packs_before = fixture.shared_packs();
    let loose_before = fixture.shared_loose_objects();

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_loose_objects(), loose_before);
    let new_packs: Vec<PathBuf> = fixture
        .shared_packs()
        .into_iter()
        .filter(|pack| !packs_before.contains(pack))
        .collect();
    assert_eq!(new_packs.len(), 1, "{new_packs:?}");
    let index = fixture
        .main
        .join(".git/objects/pack")
        .join(new_packs[0].with_extension("idx"));
    let verify = fixture.git_text(
        &fixture.main,
        &["verify-pack", "-v", index.to_str().unwrap()],
    );
    let in_pack: Vec<String> = verify
        .lines()
        .filter(|line| {
            line.get(..40)
                .is_some_and(|oid| oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
                && line.as_bytes().get(40) == Some(&b' ')
        })
        .map(|line| line[..40].to_string())
        .collect();
    let new_objects: Vec<String> = fixture
        .git_text(
            &fixture.main,
            &["rev-list", "--objects", &tip, "--not", &old],
        )
        .lines()
        .map(|line| line[..40].to_string())
        .collect();
    assert_eq!(new_objects.len(), 3, "a commit, its tree and one blob");
    for object in &new_objects {
        assert!(
            in_pack.contains(object),
            "{object} missing from {in_pack:?}"
        );
    }
    // Anything else in the pack is a delta base index-pack appended from the shared history.
    for object in &in_pack {
        assert!(
            new_objects.contains(object) || history.contains(object),
            "{object}"
        );
    }
    assert!(in_pack.len() < new_objects.len() + 3, "{in_pack:?}");
}

#[test]
fn private_git_import_hands_off_commits_to_the_shared_repository() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    assert_ne!(fixture.dot_git(), original);
    let tip = fixture.commit_file("b.txt", "two\n", "job");
    // Staged but not committed: unstaged again after the import.
    fixture.write_tree_file("c.txt", "staged\n");
    fixture.git(&fixture.tree, &["add", "c.txt"]);

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert!(report.notes.is_empty(), "{report:?}");
    fixture.assert_imported_back(&original, false);
    assert_eq!(
        fixture.git_text(&fixture.main, &["log", "--format=%s", "feature"]),
        "job\nfirst\n"
    );
    assert_eq!(fixture.rev(&fixture.tree, "HEAD"), tip);
    fixture.assert_index_matches_head();
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        "?? c.txt\n"
    );
    fs::remove_file(fixture.tree.join("c.txt")).unwrap();
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        ""
    );
    fixture.git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
    // The tree is free for the next job.
    let private = fixture.prepare();
    private.abandon().unwrap();
}

#[test]
fn private_git_import_fails_on_a_corrupt_object() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "a blob the job wrote\n", "second");
    let blob = fixture.rev(&fixture.tree, "HEAD:b.txt");
    let loose = fixture
        .private_dir()
        .join("objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    fs::remove_file(&loose).unwrap();
    fs::write(&loose, "not a zlib stream").unwrap();

    let error = fixture.import(private).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("nothing was imported"), "{text}");
    assert!(text.contains("git pack-objects failed"), "{text}");
    fixture.assert_imported_back(&original, true);
    let (refs, head, index, _) = fixture.shared_state();
    assert_eq!((refs, head, index), (before.0, before.1, before.2));
    assert!(
        !fixture
            .git_unchecked(&fixture.main, &["cat-file", "-e", &blob])
            .status
            .success()
    );
}

#[test]
fn private_git_import_fails_on_a_stale_lock_and_keeps_it() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let feature = fixture.rev(&fixture.main, "feature");
    let private = fixture.prepare();
    fixture.commit_file("b.txt", "two\n", "second");
    let lock = fixture.main.join(".git/refs/heads/feature.lock");
    fs::write(&lock, "stale").unwrap();

    let error = fixture.import(private).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("feature.lock"), "{text}");
    assert!(text.contains("File exists"), "{text}");
    assert_eq!(fs::read_to_string(&lock).unwrap(), "stale");
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(feature));
}

#[test]
fn private_git_import_without_changes_packs_nothing() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    // Staged only: no ref moves.
    fixture.write_tree_file("b.txt", "two\n");
    fixture.git(&fixture.tree, &["add", "b.txt"]);

    let report = fixture.import(private).unwrap();
    assert_eq!(report, ImportReport::default());
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_state(), before);
    assert_eq!(
        fixture.git_text(&fixture.tree, &["status", "--porcelain"]),
        "?? b.txt\n"
    );
}

#[cfg(unix)]
#[test]
fn private_git_import_refuses_hostile_ref_entries() {
    use std::os::unix::ffi::OsStrExt as _;

    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    fixture.git(&fixture.tree, &["branch", "kept"]);
    let heads = fixture.private_dir().join("refs/heads");
    // Never opened as a pipe: the import would block forever.
    let fifo = std::ffi::CString::new(heads.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    fs::write(heads.join("huge"), format!("{tip}\n").repeat(200)).unwrap();
    fs::write(heads.join("short"), &tip[..20]).unwrap();
    fs::write(heads.join("upper"), tip.to_uppercase()).unwrap();
    fs::write(heads.join("nul"), format!("{tip}\0")).unwrap();
    fs::write(heads.join("symbolic"), "ref: refs/heads/feature\n").unwrap();
    fs::write(heads.join("x.lock"), format!("{tip}\n")).unwrap();
    fs::create_dir_all(heads.join("deep")).unwrap();
    let mut deep = heads.join("deep");
    for _ in 0..40 {
        deep = deep.join("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("ref"), format!("{tip}\n")).unwrap();
    // A loose ref that is refused never falls back to its packed value.
    let packed = fixture.private_dir().join("packed-refs");
    let mut text = fs::read_to_string(&packed).unwrap();
    text.push_str(&format!(
        "{tip} refs/heads/fifo\n{tip} refs/heads/bad..name\n^{tip}\ngarbage\n{tip} refs/heads/at@{{1}}\n"
    ));
    fs::write(&packed, text).unwrap();
    fs::write(
        fixture.private_dir().join("HEAD"),
        "ref: refs/heads/../../config\n",
    )
    .unwrap();

    let report = fixture.import(private).unwrap();
    assert_eq!(
        report.imported,
        vec![
            "refs/heads/feature".to_string(),
            "refs/heads/kept".to_string()
        ]
    );
    let refused = report.refused.join("\n");
    for entry in [
        "refs/heads/fifo (a symlink or special file",
        "refs/heads/huge (larger than",
        "refs/heads/short (not a full object name",
        "refs/heads/upper (not a full object name",
        "refs/heads/nul (not a full object name",
        "refs/heads/symbolic (a symbolic ref",
        "refs/heads/x.lock (not a valid ref name",
        "(nested too deep)",
        "refs/heads/bad..name (not a valid ref name",
        "packed-refs line",
        "refs/heads/at@{1} (not a valid ref name",
        "not imported: HEAD (it names",
    ] {
        assert!(refused.contains(entry), "{entry} not in\n{refused}");
    }
    assert!(!refused.contains("not deleted"), "{refused}");
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip.clone()));
    assert_eq!(fixture.shared_ref("refs/heads/kept"), Some(tip));
    for name in ["fifo", "huge", "short", "symbolic", "bad..name"] {
        assert!(
            !fixture
                .git_unchecked(
                    &fixture.main,
                    &["show-ref", "--verify", "-q", &format!("refs/heads/{name}")]
                )
                .status
                .success(),
            "{name}"
        );
    }
}

#[test]
fn private_git_import_refuses_a_deleted_own_branch() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let before = fixture.shared_state();
    let private = fixture.prepare();
    fixture.git(&fixture.tree, &["checkout", "-q", "--detach"]);
    fixture.git(&fixture.tree, &["branch", "-D", "feature"]);
    fixture.git(&fixture.tree, &["branch", "-D", "main"]);

    let report = fixture.import(private).unwrap();
    assert!(report.imported.is_empty(), "{report:?}");
    assert_eq!(
        report.refused,
        vec![
            "not deleted: refs/heads/feature (the job deleted its own branch; the shared one is \
             unchanged)"
                .to_string(),
            "not deleted: refs/heads/main (the job deleted a ref of the shared repository)"
                .to_string(),
        ]
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_state(), before);
}

#[test]
fn private_git_import_keeps_sha256() {
    let fixture = Fixture::with_init_args(&["--object-format=sha256"]);
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    assert_eq!(tip.len(), 64);

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(tip));
    fixture.assert_index_matches_head();
}

#[test]
fn private_git_import_resets_the_index_around_a_nested_repository() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let private = fixture.prepare();
    let tip = fixture.commit_file("b.txt", "two\n", "second");
    fs::create_dir_all(fixture.tree.join("nested/.git")).unwrap();

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/feature".to_string()]);
    assert_eq!(report.notes.len(), 1, "{report:?}");
    assert!(
        report.notes[0].starts_with("the index was reset without the work tree"),
        "{report:?}"
    );
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.rev(&fixture.tree, "HEAD"), tip);
    fixture.assert_index_matches_head();
}

#[test]
fn private_git_import_check_ref_format() {
    use crate::private_git::check_ref_format;

    for good in [
        "refs/heads/a",
        "refs/heads/a/b-c_d",
        "refs/tags/v1.0",
        "refs/heads/-x",
    ] {
        assert!(check_ref_format(good), "{good}");
    }
    for bad in [
        "refs/heads/a..b",
        "refs/heads/a.lock",
        "refs/heads/.a",
        "refs/heads/a/",
        "refs/heads//a",
        "refs/heads/a.",
        "refs/heads/a@{1}",
        "refs/heads/a b",
        "refs/heads/a~1",
        "refs/heads/a^",
        "refs/heads/a:b",
        "refs/heads/a?",
        "refs/heads/a*",
        "refs/heads/a[b",
        "refs/heads/a\\b",
        "refs/heads/a\nb",
        "refs/heads/a\u{7f}",
        "heads/a",
        "refs/",
        "@",
        "HEAD",
    ] {
        assert!(!check_ref_format(bad), "{bad:?}");
    }
}

#[test]
fn private_git_import_new_branch_on_a_shared_commit_packs_nothing() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let feature = fixture.rev(&fixture.main, "feature");
    let objects_before = files_under(&fixture.main.join(".git/objects"));
    let private = fixture.prepare();
    fixture.git(&fixture.tree, &["branch", "side"]);

    let report = fixture.import(private).unwrap();
    assert_eq!(report.imported, vec!["refs/heads/side".to_string()]);
    assert!(report.refused.is_empty(), "{report:?}");
    fixture.assert_imported_back(&original, false);
    assert_eq!(fixture.shared_ref("refs/heads/side"), Some(feature));
    assert_eq!(
        files_under(&fixture.main.join(".git/objects")),
        objects_before
    );
}

#[test]
fn private_git_import_fails_on_a_mismatched_object() {
    let fixture = Fixture::new();
    let original = fixture.dot_git();
    let feature = fixture.rev(&fixture.main, "feature");
    let private = fixture.prepare();
    fixture.write_tree_file("b.txt", "the first blob\n");
    fixture.write_tree_file("c.txt", "the second blob\n");
    fixture.git(&fixture.tree, &["add", "b.txt", "c.txt"]);
    fixture.git(&fixture.tree, &["commit", "-q", "-m", "two blobs"]);
    let loose = |rev: &str| {
        let blob = fixture.rev(&fixture.tree, rev);
        fixture
            .private_dir()
            .join("objects")
            .join(&blob[..2])
            .join(&blob[2..])
    };
    // A valid object stored under another object's name.
    let (first, second) = (loose("HEAD:b.txt"), loose("HEAD:c.txt"));
    let swapped = fs::read(&second).unwrap();
    fs::remove_file(&first).unwrap();
    fs::write(&first, swapped).unwrap();

    let error = fixture.import(private).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("nothing was imported"), "{text}");
    assert!(
        text.contains("git index-pack failed")
            || text.contains("not complete in the shared repository"),
        "{text}"
    );
    fixture.assert_imported_back(&original, true);
    assert_eq!(fixture.shared_ref("refs/heads/feature"), Some(feature));
}
