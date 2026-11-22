//! The known-escape suite: every way a confined job was found to make host git run a command
//! of its choosing, replayed against real git and, when `bwrap` is installed, the real sandbox.
//!
//! Each row builds a fresh repository (a primary checkout, the job's linked worktree and another
//! linked worktree) in its own temporary directory, confines a job in the job's worktree the way
//! bongos does ([`private_git::prepare`], [`Confinement::build`], the sandbox policy) and runs the
//! row's attack as a shell script under the sandbox. While the job is still running (it signals
//! through a FIFO, then blocks until the host releases it), after it ends and after the tree is
//! handed back, the host runs git the ways bongos and the user do: plain git in the primary
//! checkout and in the other worktree, [`SafeGit`] in the job tree from bongos's own record of it,
//! and the editor's [`RealGitRepository`] (opened before the job and opened afresh) running status,
//! diff and blame. Every hostile configuration a job plants names `core.fsmonitor`, a filter
//! driver, a textconv driver and `diff.external` commands that create a marker file in the row's
//! directory, outside every job root; a row fails when any marker exists. Plain git that discovers
//! its repository from the job tree reads the job's private configuration by design and is not a
//! host path, so each row's control runs exactly that (or plants the blocked file itself) to prove
//! its markers really fire, and a neutered marker cannot pass a row.
//!
//! Besides the linked-tree rows, the import rows hand a private directory the job filled with
//! hostile refs and object files to [`private_git::PrivateGit::import`] (they need no sandbox: a job may write
//! its private directory anyway, so without `bwrap` the job runs unconfined); the crashed-pass row
//! kills the process that prepared the tree and replays a lane's recovery and `reset --hard`; and
//! the main-worktree rows confine a job in the primary checkout itself, then kill the process
//! that owns its job root entry. The owner of a crash row is this test binary run again as a
//! helper ([`owner_process_helper`]), so killing it is a real crash, never a sleep.
//!
//! Adding a row (step by step): write the job script, a `host_check` with the row's own
//! assertions and a `control`, then add one line to the `escapes!` table at the bottom. The
//! runner, [`run_row`], does the rest.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs,
    io::{self, Write as _},
    os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _},
    path::{Path, PathBuf},
    process::{ExitStatus, Output},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use bongos_confine::{
    ConfineError, Confinement, HostGit, JobDeclaration, JobKind, JobTree, TreeAccess, TreeGit,
    private_git::{self, ImportReport, RecoverOutcome},
    safe_git::{SafeGit, check_git_dir, linked_git_dir_from_record},
};
use futures::{AsyncBufReadExt as _, FutureExt as _, StreamExt as _, future::LocalBoxFuture};
use git::repository::{DiffType, GitRepository as _, RealGitRepository, RepoPath};
use gpui::{BackgroundExecutor, TestAppContext};
use sandbox::{CommandAndArgs, Sandbox, SandboxNetPolicy};
use text::{LineEnding, Rope};

/// How long the host waits for a job to signal or to exit before it kills the job and fails the
/// row. Only a guard against a hang: a healthy job signals within a second.
const HANG_GUARD: Duration = Duration::from_secs(180);

/// Shell functions for every job script, and for the hostile configuration the host writes in a
/// control. `attempt <name> <command>` runs the command in a subshell and prints
/// `<name>: written` or `<name>: blocked`; `hostile_config <file>` names a marker command for
/// `core.fsmonitor`, the filter `evil`, the diff driver `evil` and `diff.external` in `<file>`.
const PREAMBLE: &str = r#"
set -u
attempt() {
    if (eval "$2"); then echo "$1: written"; else echo "$1: blocked"; fi
}
hostile_config() {
    git config --file "$1" core.fsmonitor "touch '$MARKERS/fsmonitor'; false" &&
    git config --file "$1" filter.evil.clean "touch '$MARKERS/clean'; cat" &&
    git config --file "$1" filter.evil.smudge "touch '$MARKERS/smudge'; cat" &&
    git config --file "$1" diff.evil.textconv "touch '$MARKERS/textconv'; cat" &&
    git config --file "$1" diff.external "touch '$MARKERS/external'; true"
}
"#;

/// Rows of one process run one at a time: each points the process's environment and job roots
/// registry at its own temporary directory. Under nextest every row is a process of its own.
static ROWS: Mutex<()> = Mutex::new(());

/// One recorded escape.
struct Escape {
    name: &'static str,
    setup: Setup,
    /// The attack: the body of a shell function run in the job tree under the sandbox, printing one
    /// `attempt` line per write it tries. Empty for a row whose job never starts.
    job: &'static str,
    /// The row's own assertions, run after the shared host checks in every phase.
    host_check: fn(&Fixture, &Outcome, Phase),
    /// Proves the row's markers fire when git does read the hostile configuration (and leaves the
    /// fixture as it found it, with no marker).
    control: for<'a> fn(&'a Fixture) -> LocalBoxFuture<'a, ()>,
}

enum Setup {
    /// A job in the linked worktree `tree` with private git, run under the sandbox, then handed
    /// back.
    LinkedTree {
        hand_back: HandBack,
        sandbox: SandboxUse,
    },
    /// The build must refuse each declaration naming one of these declared directories; no job
    /// runs.
    Refused {
        declared_dirs: fn(&Fixture) -> Vec<PathBuf>,
    },
    /// A job in `tree` with private git, prepared by an owner process that is killed while the
    /// job runs; the job is killed too. Then a lane's job-start sequence: recovery, then status,
    /// `reset --hard` and `clean -fd` through `SafeGit`.
    CrashedPass,
    /// A job in the primary checkout `main` (read-only git), with `prepare` run on the fixture
    /// first. After the job ends and its root is retired, an owner process confines `main` again
    /// and is killed, and the next registry start retires its root.
    MainWorktree { prepare: fn(&Fixture) },
}

#[derive(Clone, Copy)]
enum SandboxUse {
    /// The row is skipped without `bwrap`.
    Required,
    /// Without `bwrap` the job runs unconfined: it attacks only what it may write anyway.
    IfPresent,
}

#[derive(Clone, Copy)]
enum HandBack {
    /// Imports the job's work; the function checks the result.
    Import(fn(&Fixture, &anyhow::Result<ImportReport>)),
    Abandon,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// The job is blocked in the handshake.
    During,
    /// The job exited and its confinement finished (or, in a crash row, the owner and the job
    /// were killed).
    AfterJob,
    /// The tree was imported or abandoned.
    AfterHandBack,
    /// A crashed pass was recovered and the lane sequence ran.
    AfterRecovery,
    /// An owner confining the primary checkout was killed and the registry restarted.
    AfterOwnerCrash,
    /// The build refused; no job ran.
    NoJob,
}

/// Where the row's job works.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Site {
    /// The linked worktree `tree`.
    Linked,
    /// The primary checkout `main`.
    Main,
}

/// What a row's `host_check` sees.
struct Outcome {
    /// Each attempt the job reported, `written` or `blocked`.
    attempts: BTreeMap<String, String>,
    report: HostReport,
}

impl Outcome {
    #[track_caller]
    fn expect(&self, attempt: &str, result: &str) {
        assert_eq!(
            self.attempts.get(attempt).map(String::as_str),
            Some(result),
            "attempt {attempt:?}; the job reported {:?}",
            self.attempts
        );
    }
}

/// What the shared host checks found besides the markers (which they assert themselves).
#[derive(Debug)]
struct HostReport {
    /// [`linked_git_dir_from_record`] for the linked worktree `tree`.
    tree_record: Result<PathBuf, String>,
    /// `SafeGit` status, diff and blame in the job tree: run, or the refusal.
    safe_git_tree: Vec<Result<(), String>>,
    /// The editor's fresh open of the job tree's `.git`.
    editor_tree_open: Result<(), String>,
    /// The paths the editor's repository for `tree`, opened before the job, reported in status.
    open_tree_status: Result<Vec<String>, String>,
    /// That repository's worktree diff.
    open_tree_diff: Result<String, String>,
}

/// The repositories the editor opened before the job started, as a user has them open.
struct Editors {
    main: RealGitRepository,
    other: RealGitRepository,
    tree: RealGitRepository,
}

/// A row's repository: `main` (the primary checkout, whose `.git` is `common`), the job's linked
/// worktree `tree` on `feature` and another linked worktree `other` on `side`. Every checkout
/// tracks `a.txt` with the filter and diff driver `evil` (`.gitattributes`) and has it changed
/// without a size change and dated long ago, so status must hash it (running any clean filter) and
/// diff must compare it (running any textconv or external diff).
struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    main: PathBuf,
    common: PathBuf,
    other: PathBuf,
    other_git_dir: PathBuf,
    tree: PathBuf,
    tree_git_dir: PathBuf,
    scratch: PathBuf,
    markers: PathBuf,
    /// The row's job roots registry file.
    registry: PathBuf,
    /// The commit every branch starts on.
    initial: String,
    site: Site,
    executor: BackgroundExecutor,
    editors: Editors,
}

impl Fixture {
    fn new(executor: BackgroundExecutor) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("escape-")
            .tempdir_in(temp_parent())
            .unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        let main = base.join("main");
        let scratch = base.join("scratch");
        let markers = base.join("markers");
        for dir in [&home, &main, &scratch, &markers] {
            fs::create_dir_all(dir).unwrap();
        }
        isolate_environment(&home);
        let registry = base.join("registry").join("roots.json");
        util::job_roots::init(&registry).unwrap();

        git(&main, &["init", "-q", "-b", "main"]);
        fs::write(main.join(".gitattributes"), "a.txt filter=evil diff=evil\n").unwrap();
        fs::write(main.join("a.txt"), "initial\n").unwrap();
        git(&main, &["add", ".gitattributes", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "initial"]);
        let initial = String::from_utf8(git(&main, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        let tree = base.join("tree");
        let other = base.join("other");
        git_os(
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
        git_os(
            &main,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-q"),
                OsStr::new("-b"),
                OsStr::new("side"),
                other.as_os_str(),
            ],
        );
        for checkout in [&main, &tree, &other] {
            change_keeping_size(&checkout.join("a.txt"), "changed\n");
        }
        let common = main.join(".git");
        let tree_git_dir = linked_git_dir_from_record(&tree, &common).unwrap();
        let other_git_dir = linked_git_dir_from_record(&other, &common).unwrap();
        let editors = Editors {
            main: open_editor(&main.join(".git"), &executor).unwrap(),
            other: open_editor(&other.join(".git"), &executor).unwrap(),
            tree: open_editor(&tree.join(".git"), &executor).unwrap(),
        };
        Self {
            _temp: temp,
            base,
            home,
            main,
            common,
            other,
            other_git_dir,
            tree,
            tree_git_dir,
            scratch,
            markers,
            registry,
            initial,
            site: Site::Linked,
            executor,
            editors,
        }
    }

    /// The job's tree.
    fn job_tree(&self) -> &Path {
        match self.site {
            Site::Linked => &self.tree,
            Site::Main => &self.main,
        }
    }

    /// The git directory bongos records for the job's tree.
    fn job_git_dir(&self) -> &Path {
        match self.site {
            Site::Linked => &self.tree_git_dir,
            Site::Main => &self.common,
        }
    }

    /// The trusted git directories, as [`Fixture::host_git`] lists them.
    fn host_git_dirs(&self) -> Vec<PathBuf> {
        self.host_git().git_dirs
    }

    /// The shared repository's refs, each with its object id.
    fn shared_refs(&self) -> BTreeMap<String, String> {
        let output = git(
            &self.main,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name.to_string(), oid.to_string())
            })
            .collect()
    }

    /// Asserts the shared repository still has exactly its three branches on the initial commit.
    #[track_caller]
    fn assert_shared_refs_unchanged(&self) {
        let expected: BTreeMap<String, String> = ["feature", "main", "side"]
            .into_iter()
            .map(|branch| (format!("refs/heads/{branch}"), self.initial.clone()))
            .collect();
        assert_eq!(self.shared_refs(), expected);
    }

    /// Writes the hostile configuration (see [`PREAMBLE`]) into `config`.
    fn write_hostile_config(&self, config: &Path) {
        let mut command = util::command::new_command("sh");
        command
            .args(["-c", &format!("{PREAMBLE}\nhostile_config \"$1\""), "sh"])
            .arg(config)
            .env("MARKERS", &self.markers);
        assert!(smol::block_on(command.status()).unwrap().success());
    }

    /// The trusted git directories, as bongos reads them before the job starts.
    fn host_git(&self) -> HostGit {
        HostGit {
            git_dirs: vec![
                self.tree_git_dir.clone(),
                self.common.clone(),
                self.other_git_dir.clone(),
            ],
            records_dir: None,
        }
    }

    /// A program job in `tree` with private git.
    fn declaration(&self, declared_dirs: Vec<PathBuf>) -> JobDeclaration {
        JobDeclaration {
            kind: JobKind::Program,
            trees: vec![JobTree {
                path: self.tree.clone(),
                access: TreeAccess::Writable,
                git: TreeGit::Private,
            }],
            scratch: self.scratch.clone(),
            declared_dirs,
            extra_writable: Vec::new(),
            network: SandboxNetPolicy::Blocked,
        }
    }

    /// Host git as bongos runs it in the job tree: the git directory from its own record.
    fn safe_git_in_tree(&self) -> SafeGit {
        self.isolated(SafeGit::new(self.job_git_dir(), self.job_tree()))
    }

    fn isolated(&self, git: SafeGit) -> SafeGit {
        git.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", &self.home)
    }

    /// The marker files present.
    fn markers_found(&self) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(&self.markers)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[track_caller]
    fn assert_no_markers(&self, context: &str) {
        let found = self.markers_found();
        assert!(
            found.is_empty(),
            "a job-planted command ran on the host ({context}): {found:?}"
        );
    }

    /// Runs plain discovering git (`status`, then `diff`) in `directory` with `env` added, then
    /// asserts the hostile configuration's commands ran (an fsmonitor or clean filter for status,
    /// a textconv or external diff for diff), and removes the markers.
    #[track_caller]
    fn assert_plain_git_fires(&self, directory: &Path, env: &[(&str, &Path)]) {
        self.assert_no_markers("before the control");
        let status = git_unchecked(directory, &["status", "--porcelain"], env);
        let diff = git_unchecked(directory, &["diff"], env);
        let found = self.markers_found();
        let fired = |names: &[&str]| names.iter().any(|name| found.iter().any(|f| f == name));
        assert!(
            fired(&["fsmonitor", "clean"]) && fired(&["textconv", "external"]),
            "the control's git in {} did not run the hostile commands (found {found:?}); \
             status: {}; diff: {}",
            directory.display(),
            String::from_utf8_lossy(&status.stderr),
            String::from_utf8_lossy(&diff.stderr),
        );
        for name in found {
            fs::remove_file(self.markers.join(name)).unwrap();
        }
    }
}

/// A program job in the primary checkout `main`, whose git is masked read-only.
fn main_declaration(main: &Path, scratch: &Path) -> JobDeclaration {
    JobDeclaration {
        kind: JobKind::Program,
        trees: vec![JobTree {
            path: main.to_path_buf(),
            access: TreeAccess::Writable,
            git: TreeGit::ReadOnly,
        }],
        scratch: scratch.to_path_buf(),
        declared_dirs: Vec::new(),
        extra_writable: Vec::new(),
        network: SandboxNetPolicy::Blocked,
    }
}

/// Where row directories go: the system temporary directory, unless it is under `/tmp`, which the
/// sandbox replaces with an empty tmpfs (the job could not see the fixture there).
fn temp_parent() -> PathBuf {
    let dir = std::env::temp_dir();
    if dir.starts_with("/tmp") {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
    } else {
        dir
    }
}

/// Points everything that reads the user's git configuration at `home` and nothing, so neither
/// the fixture's git nor the code under test ever reads the real home or global configuration.
fn isolate_environment(home: &Path) {
    // SAFETY: rows of one process run one at a time (`ROWS`), and every other thread of this test
    // binary is one a row started and joined, or a reaper that never reads the environment.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            std::env::remove_var(name);
        }
    }
}

/// Real git with `args` in `directory`, with `env` added; never asserts.
fn git_unchecked<S: AsRef<OsStr>>(directory: &Path, args: &[S], env: &[(&str, &Path)]) -> Output {
    let mut command = util::command::new_command("git");
    command
        .current_dir(directory)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    smol::block_on(command.output()).unwrap()
}

#[track_caller]
fn git_os(directory: &Path, args: &[&OsStr]) -> Output {
    let output = git_unchecked(directory, args, &[]);
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[track_caller]
fn git(directory: &Path, args: &[&str]) -> Output {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git_os(directory, &args)
}

/// Writes `contents` (the file's size) and dates the file long before its index entry, so git
/// must hash it, running any clean filter, to tell whether it changed.
fn change_keeping_size(path: &Path, contents: &str) {
    assert_eq!(fs::metadata(path).unwrap().len(), contents.len() as u64);
    fs::write(path, contents).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
}

/// Whether `program` is a file in a `PATH` directory.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

/// Opens `dotgit` as the editor does, trusted (the user's own checkout), so only the job root rule
/// stands between a job's configuration and the editor's git.
fn open_editor(dotgit: &Path, executor: &BackgroundExecutor) -> anyhow::Result<RealGitRepository> {
    let repository =
        RealGitRepository::new(dotgit, None, Some(PathBuf::from("git")), executor.clone())?;
    repository.set_trusted(true);
    Ok(repository)
}

/// Status, a worktree diff and a blame of `a.txt` through the editor's `build_command`.
async fn editor_status_diff_blame(repository: &RealGitRepository) -> Vec<anyhow::Result<()>> {
    let status = repository.status(&[]).await.map(|_| ());
    let diff = repository.diff(DiffType::HeadToWorktree).await.map(|_| ());
    let blame = repository
        .blame(
            RepoPath::new("a.txt").unwrap(),
            Rope::from("changed\n"),
            LineEnding::Unix,
        )
        .await
        .map(|_| ());
    vec![status, diff, blame]
}

/// The checks every row runs in every phase: git the way the host runs it, then no marker.
async fn host_checks(fixture: &Fixture, phase: Phase) -> HostReport {
    let context = |what: &str| format!("{what}, {phase:?}");

    // Plain git in the primary checkout and the other worktree: the user's own git.
    for checkout in [&fixture.main, &fixture.other] {
        for args in [&["status", "--porcelain"][..], &["diff"][..]] {
            let output = git_unchecked(checkout, args, &[]);
            assert!(
                output.status.success(),
                "git {args:?} in {} failed ({phase:?}): {}",
                checkout.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    fixture.assert_no_markers(&context(
        "plain git in the primary checkout and other worktree",
    ));

    // The host's git directories stay trusted; nothing a job could write is.
    for git_dir in [
        &fixture.common,
        &fixture.other_git_dir,
        &fixture.tree_git_dir,
    ] {
        check_git_dir(git_dir)
            .unwrap_or_else(|error| panic!("{} refused ({phase:?}): {error:#}", git_dir.display()));
    }
    if phase != Phase::NoJob {
        assert!(check_git_dir(&fixture.scratch.join("git")).is_err());
    }

    // `SafeGit` in the job tree, from bongos's record of the tree.
    let mut safe_git_tree = Vec::new();
    for args in [
        &["status", "--porcelain"][..],
        &["diff"][..],
        &["blame", "--", "a.txt"][..],
    ] {
        let result = match smol::block_on(fixture.safe_git_in_tree().command(args)) {
            Ok(mut command) => {
                let output = smol::block_on(command.output()).unwrap();
                if output.status.success() {
                    Ok(())
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).into_owned())
                }
            }
            Err(error) => Err(format!("{error:#}")),
        };
        safe_git_tree.push(result);
    }
    fixture.assert_no_markers(&context("SafeGit in the job tree"));
    let tree_record = linked_git_dir_from_record(&fixture.tree, &fixture.common)
        .map_err(|error| format!("{error:#}"));
    if let Ok(record) = &tree_record {
        assert_eq!(record, &fixture.tree_git_dir, "{phase:?}");
    }

    // The editor: the repositories it had open before the job, then fresh opens.
    let editors = &fixture.editors;
    for (name, repository) in [
        ("primary checkout", &editors.main),
        ("other", &editors.other),
    ] {
        for result in editor_status_diff_blame(repository).await {
            result.unwrap_or_else(|error| {
                panic!("the editor's git in the {name} failed ({phase:?}): {error:#}")
            });
        }
    }
    let _ = editor_status_diff_blame(&editors.tree).await;
    let open_tree_status = editors
        .tree
        .status(&[])
        .await
        .map(|status| {
            status
                .entries
                .iter()
                .map(|(path, _)| path.as_unix_str().to_string())
                .collect()
        })
        .map_err(|error| format!("{error:#}"));
    let open_tree_diff = editors
        .tree
        .diff(DiffType::HeadToWorktree)
        .await
        .map_err(|error| format!("{error:#}"));
    for checkout in [&fixture.main, &fixture.other] {
        let repository =
            open_editor(&checkout.join(".git"), &fixture.executor).unwrap_or_else(|error| {
                panic!(
                    "the editor refused {} ({phase:?}): {error:#}",
                    checkout.display()
                )
            });
        for result in editor_status_diff_blame(&repository).await {
            result.unwrap();
        }
    }
    let editor_tree_open = match open_editor(&fixture.job_tree().join(".git"), &fixture.executor) {
        Ok(repository) => {
            assert!(
                !util::job_roots::contains(&repository.git_dir)
                    && !util::job_roots::contains(&repository.common_dir),
                "the editor opened job-written git data: {:?} ({phase:?})",
                repository.git_dir
            );
            let _ = editor_status_diff_blame(&repository).await;
            Ok(())
        }
        Err(error) => Err(format!("{error:#}")),
    };
    fixture.assert_no_markers(&context("the editor"));

    HostReport {
        tree_record,
        safe_git_tree,
        editor_tree_open,
        open_tree_status,
        open_tree_diff,
    }
}

/// Makes a FIFO at `path`.
fn mkfifo(path: &Path) {
    let path_c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `path_c` is a valid NUL-terminated string that outlives the call.
    let result = unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "mkfifo {}: {}",
        path.display(),
        io::Error::last_os_error()
    );
}

/// Opens the FIFO at `path` for writing without blocking: fails (`ENXIO`) when no reader has it
/// open.
fn open_fifo_writer(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

enum Event {
    /// What the job wrote to the `ready` FIFO before closing it.
    Ready(io::Result<String>),
    Exited(io::Result<ExitStatus>),
}

/// A job running under the sandbox, and the handshake with it: the job opens the `release` FIFO,
/// tries its attack, writes its attempts to the `ready` FIFO and closes it, then blocks reading
/// `release` until the host writes a line. A thread reads `ready` and another waits for the job,
/// both reporting on one channel, so the host never blocks on the job: a job that dies early is
/// seen exiting (and the reader is released by opening `ready` for writing), and a job that hangs
/// is killed after [`HANG_GUARD`].
struct Job {
    pid: u32,
    events: mpsc::Receiver<Event>,
    ready: PathBuf,
    release: PathBuf,
    log: PathBuf,
    reader: Option<JoinHandle<()>>,
    reaped: Arc<Mutex<bool>>,
    _sandbox: Option<Sandbox>,
}

impl Job {
    /// Starts the job in the fixture's job tree, under `sandbox`, or unconfined without one.
    fn start(fixture: &Fixture, mut sandbox: Option<Sandbox>, body: &str) -> Self {
        let handshake = fixture.base.join("handshake");
        fs::create_dir_all(&handshake).unwrap();
        let ready = handshake.join("ready");
        let release = handshake.join("release");
        mkfifo(&ready);
        mkfifo(&release);
        let script = format!(
            "{PREAMBLE}\njob() {{\n{body}\n}}\n\
             exec 3<>\"$RELEASE\"\n\
             result=$(job)\n\
             printf '%s\\n' \"$result\" > \"$READY\"\n\
             read -r _ <&3\n"
        );
        let path_var = std::env::var("PATH").unwrap_or_default();
        let env: Vec<(&str, String)> = vec![
            ("PATH", path_var),
            ("HOME", fixture.home.display().to_string()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_AUTHOR_NAME", "job".into()),
            ("GIT_AUTHOR_EMAIL", "job@localhost".into()),
            ("GIT_COMMITTER_NAME", "job".into()),
            ("GIT_COMMITTER_EMAIL", "job@localhost".into()),
            ("TREE", fixture.tree.display().to_string()),
            ("MAIN", fixture.main.display().to_string()),
            ("SCRATCH", fixture.scratch.display().to_string()),
            ("COMMON", fixture.common.display().to_string()),
            ("OTHER_GIT_DIR", fixture.other_git_dir.display().to_string()),
            ("MARKERS", fixture.markers.display().to_string()),
            ("READY", ready.display().to_string()),
            ("RELEASE", release.display().to_string()),
        ];
        let command = CommandAndArgs {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script, "job".into()],
            env: env
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
            cwd: Some(fixture.job_tree().to_path_buf()),
        };
        let (program, args, env) = match &mut sandbox {
            Some(sandbox) => {
                let wrapped = smol::block_on(sandbox.wrap(&command)).unwrap();
                (wrapped.program, wrapped.args, wrapped.env)
            }
            None => (command.program, command.args, command.env),
        };
        // The job's output goes to a file outside every job root, through inherited descriptors.
        let log = fixture.base.join("job.log");
        let log_file = fs::File::create(&log).unwrap();
        let mut process = util::command::new_command(&program);
        process
            .args(&args)
            .env_clear()
            .envs(&env)
            .current_dir(fixture.job_tree())
            .stdin(util::command::Stdio::null())
            .stdout(log_file.try_clone().unwrap())
            .stderr(log_file);
        let mut child = process.spawn().unwrap();
        let pid = child.id();

        let (sender, events) = mpsc::channel();
        let reader = std::thread::spawn({
            let sender = sender.clone();
            let ready = ready.clone();
            move || {
                sender.send(Event::Ready(fs::read_to_string(&ready))).ok();
            }
        });
        let reaped = Arc::new(Mutex::new(false));
        std::thread::spawn({
            let reaped = reaped.clone();
            move || {
                // Wait without reaping, so `kill` can tell under the lock whether the pid is
                // still ours.
                // SAFETY: a zeroed siginfo is a valid out-parameter for waitid.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: plain syscall on our child's pid; WNOWAIT leaves it unreaped.
                let waited = loop {
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid as libc::id_t,
                            &mut info,
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if result == 0 {
                        break true;
                    }
                    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        break false;
                    }
                };
                if !waited {
                    // waitid failed for another reason, so the job may still be running: poll
                    // without ever holding the lock across a blocking wait.
                    loop {
                        let mut guard = reaped.lock().unwrap_or_else(|error| error.into_inner());
                        if let Ok(Some(status)) = child.try_status() {
                            *guard = true;
                            drop(guard);
                            sender.send(Event::Exited(Ok(status))).ok();
                            return;
                        }
                        drop(guard);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                // The child has exited, so `status` returns at once under the lock.
                let mut reaped = reaped.lock().unwrap_or_else(|error| error.into_inner());
                let status = smol::block_on(child.status());
                *reaped = true;
                drop(reaped);
                sender.send(Event::Exited(status)).ok();
            }
        });
        Self {
            reaped,
            pid,
            events,
            ready,
            release,
            log,
            reader: Some(reader),
            _sandbox: sandbox,
        }
    }

    /// Waits for the job's attempts, parsed from its `<name>: <result>` lines.
    fn wait_ready(&mut self) -> BTreeMap<String, String> {
        let text = match self.events.recv_timeout(HANG_GUARD) {
            Ok(Event::Ready(Ok(text))) if !text.trim().is_empty() => text,
            Ok(Event::Ready(result)) => self.fail(&format!("the job signaled nothing: {result:?}")),
            Ok(Event::Exited(status)) => {
                self.fail(&format!("the job exited before signaling: {status:?}"))
            }
            Err(error) => self.fail(&format!("no signal from the job: {error}")),
        };
        self.join_reader();
        text.lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(name, result)| (name.to_string(), result.to_string()))
            .collect()
    }

    /// Lets the job go and waits for it to exit successfully.
    fn release_and_wait(mut self) {
        let written = open_fifo_writer(&self.release).and_then(|mut fifo| fifo.write_all(b"go\n"));
        if let Err(error) = written {
            self.fail(&format!("cannot release the job: {error}"));
        }
        match self.events.recv_timeout(HANG_GUARD) {
            Ok(Event::Exited(Ok(status))) if status.success() => {}
            Ok(Event::Exited(status)) => self.fail(&format!("the job failed: {status:?}")),
            Ok(Event::Ready(_)) => self.fail("the job signaled twice"),
            Err(error) => self.fail(&format!("the job did not exit: {error}")),
        }
    }

    /// Kills the job, as a crash of everything running it would, and waits for it to exit.
    fn kill_and_wait(mut self) {
        self.kill();
        match self.events.recv_timeout(HANG_GUARD) {
            Ok(Event::Exited(_)) => {}
            Ok(Event::Ready(_)) => self.fail("the job signaled twice"),
            Err(error) => self.fail(&format!("the killed job did not exit: {error}")),
        }
    }

    /// Joins the reader thread, which has already sent its event.
    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.join().ok();
        }
    }

    /// Never waits for the reader: when the job died before the reader opened `ready`, nobody can
    /// unblock its `open`, so an unfinished reader is nudged (if a writer can be opened now) and
    /// detached. It ends with the test process.
    fn release_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            if !reader.is_finished() {
                drop(open_fifo_writer(&self.ready));
            }
        }
    }

    fn kill(&mut self) {
        // The watcher reaps the child under this lock, so a false flag means the pid is still ours
        // (running or an unreaped zombie) and cannot have been reused.
        let reaped = self
            .reaped
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !*reaped {
            // SAFETY: plain syscall; the pid is our child and the watcher has not reaped it.
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    fn fail(&mut self, message: &str) -> ! {
        self.kill();
        self.release_reader();
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        panic!("{message}\njob output:\n{log}");
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // A failed host check unwinds while the job is still blocked in the handshake.
        self.kill();
        self.release_reader();
    }
}

/// Runs one row (see the module documentation).
#[allow(
    clippy::await_holding_lock,
    reason = "each row's future runs alone on its own test executor; the lock only keeps rows of \
              one process apart"
)]
async fn run_row(row: &Escape, cx: &mut TestAppContext) {
    let _serial = ROWS.lock().unwrap_or_else(|error| error.into_inner());
    if !on_path("git") {
        println!("skipped: git not found ({})", row.name);
        return;
    }
    cx.executor().allow_parking();
    let needs_bwrap = match &row.setup {
        Setup::LinkedTree { sandbox, .. } => matches!(sandbox, SandboxUse::Required),
        Setup::Refused { .. } => false,
        Setup::CrashedPass | Setup::MainWorktree { .. } => true,
    };
    let bwrap = on_path("bwrap");
    if needs_bwrap && !bwrap {
        println!("skipped: bwrap not found ({})", row.name);
        return;
    }
    match &row.setup {
        Setup::LinkedTree { hand_back, .. } => {
            if !bwrap {
                println!("bwrap not found: the job runs unconfined ({})", row.name);
            }
            run_linked_tree(row, *hand_back, bwrap, cx).await;
        }
        Setup::Refused { declared_dirs } => run_refused(row, *declared_dirs, cx).await,
        Setup::CrashedPass => run_crashed_pass(row, cx).await,
        Setup::MainWorktree { prepare } => run_main_worktree(row, *prepare, cx).await,
    }
}

/// The sandbox enforcing `confinement`, failing the row when `bwrap` is installed but cannot
/// create it.
fn sandbox_for(confinement: &Confinement, fixture: &Fixture) -> Sandbox {
    let policy = confinement.sandbox_policy();
    Sandbox::can_create(&policy, Some(fixture.job_tree()))
        .unwrap_or_else(|error| panic!("bwrap is installed but cannot sandbox: {error}"));
    Sandbox::new(policy).unwrap()
}

async fn run_linked_tree(
    row: &Escape,
    hand_back: HandBack,
    sandboxed: bool,
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx.executor());
    let original_dot_git = fs::read(fixture.tree.join(".git")).unwrap();
    let private = smol::block_on(private_git::prepare(
        &fixture.tree,
        &fixture.tree_git_dir,
        &fixture.scratch,
    ))
    .unwrap();
    let confinement =
        Confinement::build(&fixture.declaration(Vec::new()), &fixture.host_git()).unwrap();
    let sandbox = sandboxed.then(|| sandbox_for(&confinement, &fixture));

    let mut job = Job::start(&fixture, sandbox, row.job);
    let attempts = job.wait_ready();
    println!("{}: the job reported {attempts:?}", row.name);
    check_phase(row, &fixture, &attempts, Phase::During).await;
    job.release_and_wait();
    confinement.finish().unwrap();
    check_phase(row, &fixture, &attempts, Phase::AfterJob).await;

    (row.control)(&fixture).await;
    fixture.assert_no_markers("after the control");

    let linked_git_dir = private.linked_git_dir().to_path_buf();
    match hand_back {
        HandBack::Import(check) => {
            let git = fixture.isolated(SafeGit::without_work_tree(&linked_git_dir));
            let result = smol::block_on(private.import(&git));
            println!("{}: the import returned {result:?}", row.name);
            fixture.assert_no_markers("the import");
            check(&fixture, &result);
        }
        HandBack::Abandon => private.abandon().unwrap(),
    }
    assert_eq!(
        private_git::recover(&linked_git_dir).unwrap(),
        RecoverOutcome::NoMarker
    );
    assert_eq!(
        fs::read(fixture.tree.join(".git")).unwrap(),
        original_dot_git
    );
    check_phase(row, &fixture, &attempts, Phase::AfterHandBack).await;
}

async fn run_refused(
    row: &Escape,
    declared_dirs: fn(&Fixture) -> Vec<PathBuf>,
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx.executor());
    for dir in declared_dirs(&fixture) {
        let result =
            Confinement::build(&fixture.declaration(vec![dir.clone()]), &fixture.host_git());
        assert!(
            matches!(result, Err(ConfineError::GitDirectory { .. })),
            "declaring {} writable: {result:?}",
            dir.display()
        );
        for path in [&dir, &fixture.tree, &fixture.scratch] {
            assert!(
                !util::job_roots::contains(path),
                "a refused build registered {}",
                path.display()
            );
        }
    }
    check_phase(row, &fixture, &BTreeMap::new(), Phase::NoJob).await;
    (row.control)(&fixture).await;
    fixture.assert_no_markers("after the control");
}

async fn run_crashed_pass(row: &Escape, cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx.executor());
    let original_dot_git = fs::read(fixture.tree.join(".git")).unwrap();
    let owner = Owner::start(
        &fixture,
        OWNER_PREPARES,
        &[
            ("ESCAPE_TREE", &fixture.tree),
            ("ESCAPE_LINKED_GIT_DIR", &fixture.tree_git_dir),
            ("ESCAPE_SCRATCH", &fixture.scratch),
        ],
    );
    assert!(fixture.tree_git_dir.join(private_git::MARKER).is_file());
    let confinement =
        Confinement::build(&fixture.declaration(Vec::new()), &fixture.host_git()).unwrap();
    let mut job = Job::start(&fixture, Some(sandbox_for(&confinement, &fixture)), row.job);
    let attempts = job.wait_ready();
    println!("{}: the job reported {attempts:?}", row.name);
    check_phase(row, &fixture, &attempts, Phase::During).await;

    // The app dies mid-pass: its process, then the job it ran. The owner only prepares, so after
    // the crash only private-git recovery is verified; the test process finishes the confinement
    // itself, normally.
    owner.crash();
    job.kill_and_wait();
    confinement.finish().unwrap();
    check_phase(row, &fixture, &attempts, Phase::AfterJob).await;
    (row.control)(&fixture).await;
    fixture.assert_no_markers("after the control");

    // A lane's job-start sequence in the tree: recovery, then git through `SafeGit` from the
    // recorded git directory.
    assert_eq!(
        private_git::recover(&fixture.tree_git_dir).unwrap(),
        RecoverOutcome::Recovered
    );
    assert_eq!(
        private_git::recover(&fixture.tree_git_dir).unwrap(),
        RecoverOutcome::NoMarker
    );
    assert_eq!(
        fs::read(fixture.tree.join(".git")).unwrap(),
        original_dot_git
    );
    let git = fixture.safe_git_in_tree();
    let mut outputs = Vec::new();
    for args in [
        &["status", "--porcelain"][..],
        &["reset", "-q", "--hard"][..],
        &["clean", "-q", "-fd"][..],
    ] {
        let mut command = smol::block_on(git.command(args))
            .unwrap_or_else(|error| panic!("SafeGit refused {args:?}: {error:#}"));
        let output = smol::block_on(command.output()).unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    // The status a lane saves sees the uncommitted work.
    assert!(outputs[0].contains("a.txt"), "{}", outputs[0]);
    assert!(outputs[0].contains("untracked.txt"), "{}", outputs[0]);
    fixture.assert_no_markers("the lane sequence after recovery");
    check_phase(row, &fixture, &attempts, Phase::AfterRecovery).await;
}

async fn run_main_worktree(row: &Escape, prepare: fn(&Fixture), cx: &mut TestAppContext) {
    let mut fixture = Fixture::new(cx.executor());
    fixture.site = Site::Main;
    prepare(&fixture);
    let declaration = main_declaration(&fixture.main, &fixture.scratch);
    let confinement = Confinement::build(&declaration, &fixture.host_git()).unwrap();
    assert!(confinement.requires_sandbox());
    let mut job = Job::start(&fixture, Some(sandbox_for(&confinement, &fixture)), row.job);
    let attempts = job.wait_ready();
    println!("{}: the job reported {attempts:?}", row.name);
    check_phase(row, &fixture, &attempts, Phase::During).await;
    job.release_and_wait();
    confinement.finish().unwrap();
    check_phase(row, &fixture, &attempts, Phase::AfterJob).await;
    (row.control)(&fixture).await;
    fixture.assert_no_markers("after the control");

    // Another process confines the checkout for a job and dies before finishing it; the next
    // registry start (any process's) retires the root of the dead owner.
    let git_dirs = std::env::join_paths(fixture.host_git_dirs()).unwrap();
    let owner = Owner::start(
        &fixture,
        OWNER_CONFINES_MAIN,
        &[
            ("ESCAPE_REGISTRY", &fixture.registry),
            ("ESCAPE_MAIN", &fixture.main),
            ("ESCAPE_SCRATCH", &fixture.scratch),
            ("ESCAPE_GIT_DIRS", Path::new(&git_dirs)),
        ],
    );
    assert!(util::job_roots::contains(&fixture.main));
    assert!(!util::job_roots::contains(&fixture.common));
    owner.crash();
    assert!(util::job_roots::contains(&fixture.main));
    util::job_roots::init(&fixture.registry).unwrap();
    assert!(!util::job_roots::contains(&fixture.main));
    check_phase(row, &fixture, &attempts, Phase::AfterOwnerCrash).await;
}

/// The environment variable naming the role of [`owner_process_helper`].
const OWNER_ROLE: &str = "BONGOS_ESCAPE_OWNER_ROLE";
/// The helper prepares private git for `ESCAPE_TREE`.
const OWNER_PREPARES: &str = "prepare";
/// The helper confines a job in `ESCAPE_MAIN`, recording its root in `ESCAPE_REGISTRY`.
const OWNER_CONFINES_MAIN: &str = "confine-main";
/// The line the helper prints once it owns what it took.
const OWNER_READY: &str = "escape owner ready";

/// An owner process: this test binary run again as [`owner_process_helper`]. It takes a tree
/// (private git) or a job root (a confinement) as its own, then blocks until it is killed.
struct Owner {
    child: util::command::Child,
    pid: u32,
}

impl Owner {
    fn start(fixture: &Fixture, role: &str, env: &[(&str, &Path)]) -> Self {
        let log = fixture.base.join(format!("owner-{role}.log"));
        let mut command = util::command::new_command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "owner_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(OWNER_ROLE, role)
            .env("ESCAPE_HOME", &fixture.home)
            .stdin(util::command::Stdio::piped())
            .stdout(util::command::Stdio::piped())
            .stderr(fs::File::create(&log).unwrap())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let (sender, ready) = mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = futures::io::BufReader::new(stdout).lines();
            let found = smol::block_on(async {
                while let Some(Ok(line)) = lines.next().await {
                    if line.contains(OWNER_READY) {
                        return true;
                    }
                }
                false
            });
            sender.send(found).ok();
        });
        match ready.recv_timeout(HANG_GUARD) {
            Ok(true) => Self { child, pid },
            outcome => {
                child.kill().ok();
                panic!(
                    "the owner process ({role}) did not get ready ({outcome:?}):\n{}",
                    fs::read_to_string(&log).unwrap_or_default()
                );
            }
        }
    }

    /// Kills the owner and reaps it, so its pid is no longer a live owner.
    fn crash(mut self) {
        let start_time = util::job_roots::process_start_time(self.pid).unwrap();
        self.child.kill().unwrap();
        let status = smol::block_on(self.child.status()).unwrap();
        assert!(!status.success(), "{status:?}");
        assert!(!util::job_roots::process_alive(self.pid, start_time));
    }
}

/// Not a row: the owner process of the crash rows (see [`Owner`]). Does nothing unless
/// [`OWNER_ROLE`] is set.
#[test]
fn owner_process_helper() {
    let Some(role) = std::env::var_os(OWNER_ROLE) else {
        return;
    };
    let path = |name: &str| PathBuf::from(std::env::var_os(name).unwrap());
    isolate_environment(&path("ESCAPE_HOME"));
    match role.to_str() {
        Some(OWNER_PREPARES) => {
            let private = smol::block_on(private_git::prepare(
                &path("ESCAPE_TREE"),
                &path("ESCAPE_LINKED_GIT_DIR"),
                &path("ESCAPE_SCRATCH"),
            ))
            .unwrap();
            std::mem::forget(private);
        }
        Some(OWNER_CONFINES_MAIN) => {
            util::job_roots::init(&path("ESCAPE_REGISTRY")).unwrap();
            let host = HostGit {
                git_dirs: std::env::split_paths(&path("ESCAPE_GIT_DIRS")).collect(),
                records_dir: None,
            };
            let declaration = main_declaration(&path("ESCAPE_MAIN"), &path("ESCAPE_SCRATCH"));
            let confinement = Confinement::build(&declaration, &host).unwrap();
            std::mem::forget(confinement);
        }
        _ => panic!("unknown owner role {role:?}"),
    }
    println!("{OWNER_READY}");
    io::stdout().flush().unwrap();
    // Blocks until the test kills this process (or, should the test die first, its pipe closes).
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    std::process::exit(1);
}

async fn check_phase(
    row: &Escape,
    fixture: &Fixture,
    attempts: &BTreeMap<String, String>,
    phase: Phase,
) {
    let report = host_checks(fixture, phase).await;
    let outcome = Outcome {
        attempts: attempts.clone(),
        report,
    };
    (row.host_check)(fixture, &outcome, phase);
}

// E1: the job plants `<common>/commondir`, which would make every checkout of the repository read
// its configuration from a common directory the job wrote.

const E1_JOB: &str = r#"
attempt "evil common" 'cp -R "$COMMON" "$SCRATCH/evil-common" && hostile_config "$SCRATCH/evil-common/config"'
attempt commondir 'printf "%s\n" "$SCRATCH/evil-common" > "$COMMON/commondir"'
"#;

fn e1_check(fixture: &Fixture, outcome: &Outcome, _: Phase) {
    outcome.expect("evil common", "written");
    outcome.expect("commondir", "blocked");
    assert!(!fixture.common.join("commondir").exists());
}

fn e1_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        // Had the write landed, the user's git in the primary checkout would run the job's
        // commands, and `SafeGit` would refuse the repository.
        let planted = fixture.common.join("commondir");
        fs::write(
            &planted,
            format!("{}\n", fixture.scratch.join("evil-common").display()),
        )
        .unwrap();
        assert!(check_git_dir(&fixture.common).is_err());
        fixture.assert_plain_git_fires(&fixture.main, &[]);
        fs::remove_file(&planted).unwrap();
    }
    .boxed_local()
}

// E2: the job rewrites the other worktree's `commondir` and plants its `config.worktree`.

const E2_JOB: &str = r#"
attempt "evil common" 'cp -R "$COMMON" "$SCRATCH/evil-common" && hostile_config "$SCRATCH/evil-common/config"'
attempt "other commondir" 'printf "%s\n" "$SCRATCH/evil-common" > "$OTHER_GIT_DIR/commondir"'
attempt "other config.worktree" 'hostile_config "$OTHER_GIT_DIR/config.worktree"'
"#;

fn e2_check(fixture: &Fixture, outcome: &Outcome, _: Phase) {
    outcome.expect("evil common", "written");
    outcome.expect("other commondir", "blocked");
    outcome.expect("other config.worktree", "blocked");
    assert_eq!(
        fs::read_to_string(fixture.other_git_dir.join("commondir")).unwrap(),
        "../..\n"
    );
    assert!(!fixture.other_git_dir.join("config.worktree").exists());
}

fn e2_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        // This control proves only the `commondir` rewrite. A planted `config.worktree` is read
        // only when `extensions.worktreeConfig` is set, so that marker is covered by the
        // blocked-write assertions in `e2_check` alone.
        let commondir = fixture.other_git_dir.join("commondir");
        let original = fs::read(&commondir).unwrap();
        fs::write(
            &commondir,
            format!("{}\n", fixture.scratch.join("evil-common").display()),
        )
        .unwrap();
        assert!(check_git_dir(&fixture.other_git_dir).is_err());
        fixture.assert_plain_git_fires(&fixture.other, &[]);
        fs::write(&commondir, original).unwrap();
    }
    .boxed_local()
}

// The job writes its private git configuration, which it owns: allowed, and never read by host
// git.

const PRIVATE_CONFIG_JOB: &str = r#"
attempt "private config" 'hostile_config "$(git rev-parse --absolute-git-dir)/config"'
"#;

fn private_config_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("private config", "written");
    let report = &outcome.report;
    for result in &report.safe_git_tree {
        assert!(
            result.is_ok(),
            "SafeGit in the job tree ({phase:?}): {result:?}"
        );
    }
    match phase {
        Phase::During | Phase::AfterJob => {
            let config = fs::read_to_string(fixture.scratch.join("git/config")).unwrap();
            assert!(config.contains("fsmonitor"), "{config}");
            let error = report.editor_tree_open.as_ref().unwrap_err();
            assert!(error.contains("written by a bongos job"), "{error}");
        }
        _ => assert!(report.editor_tree_open.is_ok(), "{report:?}"),
    }
}

fn private_config_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move { fixture.assert_plain_git_fires(&fixture.tree, &[]) }.boxed_local()
}

/// The hand-back of a job that imports nothing it may not.
fn imported_cleanly(_: &Fixture, result: &anyhow::Result<ImportReport>) {
    let report = result.as_ref().unwrap();
    assert!(report.refused.is_empty(), "{report:?}");
}

/// Asserts the import failed on the private object directory, naming one of `names`, and that
/// no ref moved.
#[track_caller]
fn assert_objects_refused(
    fixture: &Fixture,
    result: &anyhow::Result<ImportReport>,
    names: &[&str],
) {
    let error = match result {
        Ok(report) => panic!("the import accepted the private objects: {report:?}"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("refusing the private object directory")
            && names.iter().any(|name| error.contains(name)),
        "{error}"
    );
    assert!(error.contains("nothing was imported"), "{error}");
    fixture.assert_shared_refs_unchanged();
    assert!(fixture.scratch.join(private_git::PRIVATE_DIR).is_dir());
}

// Alternates and symlinks planted in the private directory: the job adds an object directory of
// its own to the private alternates and links entries at host files. The import refuses
// the object directory (nothing moves), and the private directory is later removed without
// following any link.

// The import rows write the hostile private configuration last: without a sandbox the job's own
// git would run its commands and create the markers itself.

const PRIVATE_LINKS_JOB: &str = r#"
PRIVATE=$(git rev-parse --absolute-git-dir)
attempt "commit" 'git commit -q --allow-empty -m "job work"'
attempt "alternates" 'mkdir "$SCRATCH/evil-objects" && printf "%s\n" "$SCRATCH/evil-objects" >> "$PRIVATE/objects/info/alternates"'
attempt "symlinked checkout" 'ln -s "$MAIN" "$PRIVATE/checkout"'
attempt "symlinked shared objects" 'ln -s "$COMMON/objects" "$PRIVATE/shared-objects"'
attempt "private config" 'hostile_config "$PRIVATE/config"'
"#;

fn private_links_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    for attempt in [
        "private config",
        "commit",
        "alternates",
        "symlinked checkout",
        "symlinked shared objects",
    ] {
        outcome.expect(attempt, "written");
    }
    fixture.assert_shared_refs_unchanged();
    for result in &outcome.report.safe_git_tree {
        assert!(
            result.is_ok(),
            "SafeGit in the job tree ({phase:?}): {result:?}"
        );
    }
    let alternates = fs::read_to_string(fixture.common.join("objects/info/alternates"));
    assert!(alternates.is_err(), "the shared alternates: {alternates:?}");
}

fn private_links_import(fixture: &Fixture, result: &anyhow::Result<ImportReport>) {
    assert_objects_refused(fixture, result, &["alternates"]);
    let private = fixture.scratch.join(private_git::PRIVATE_DIR);
    assert!(
        fs::symlink_metadata(private.join("checkout"))
            .unwrap()
            .is_symlink()
    );

    // The next preparation replaces the kept directory, and abandoning it removes it; neither
    // follows a link into the host's files.
    let again = smol::block_on(private_git::prepare(
        &fixture.tree,
        &fixture.tree_git_dir,
        &fixture.scratch,
    ))
    .unwrap();
    assert!(fs::symlink_metadata(private.join("checkout")).is_err());
    assert_eq!(
        fs::read_to_string(private.join("objects/info/alternates")).unwrap(),
        format!("{}\n", fixture.common.join("objects").display())
    );
    again.abandon().unwrap();
    assert!(!private.exists());
    assert_eq!(
        fs::read_to_string(fixture.main.join(".gitattributes")).unwrap(),
        "a.txt filter=evil diff=evil\n"
    );
    assert!(fixture.common.join("refs/heads/main").is_file());
    git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
    fixture.assert_shared_refs_unchanged();
}

// A tag and a moved foreign branch at import: the job tags its commit, moves `main` and `side`
// (which the other worktree has checked out) and makes a branch of its own. Only its own branch
// and the new branch come back.

const FOREIGN_REFS_JOB: &str = r#"
attempt "commit" 'git commit -q --allow-empty -m "job work"'
attempt "new branch" 'git branch new-work'
attempt "tag" 'git tag job-tag'
attempt "annotated tag" 'git tag -a -m "job tag" job-annotated'
attempt "move side" 'git branch -f side HEAD'
attempt "move main" 'git update-ref refs/heads/main HEAD'
attempt "private config" 'hostile_config "$(git rev-parse --absolute-git-dir)/config"'
"#;

fn foreign_refs_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    for attempt in [
        "private config",
        "commit",
        "new branch",
        "tag",
        "annotated tag",
        "move side",
        "move main",
    ] {
        outcome.expect(attempt, "written");
    }
    if phase != Phase::AfterHandBack {
        fixture.assert_shared_refs_unchanged();
    }
}

fn foreign_refs_import(fixture: &Fixture, result: &anyhow::Result<ImportReport>) {
    let report = result.as_ref().unwrap();
    assert_eq!(
        report.imported,
        vec![
            "refs/heads/feature".to_string(),
            "refs/heads/new-work".to_string()
        ]
    );
    for name in [
        "refs/tags/job-tag",
        "refs/tags/job-annotated",
        "refs/heads/side",
        "refs/heads/main",
    ] {
        assert!(
            report.refused.iter().any(|line| line.contains(name)),
            "{name} is not reported refused: {report:?}"
        );
    }
    assert_eq!(
        report.kept_private_dir.as_deref(),
        Some(fixture.scratch.join(private_git::PRIVATE_DIR).as_path())
    );
    let refs = fixture.shared_refs();
    let feature = refs["refs/heads/feature"].clone();
    assert_ne!(feature, fixture.initial);
    let expected: BTreeMap<String, String> = [
        ("refs/heads/feature", &feature),
        ("refs/heads/main", &fixture.initial),
        ("refs/heads/new-work", &feature),
        ("refs/heads/side", &fixture.initial),
    ]
    .into_iter()
    .map(|(name, oid)| (name.to_string(), oid.clone()))
    .collect();
    assert_eq!(refs, expected);
    let subject = git(
        &fixture.main,
        &["log", "-1", "--format=%s", "refs/heads/feature"],
    )
    .stdout;
    assert_eq!(String::from_utf8(subject).unwrap(), "job work\n");
    let other_head = git(&fixture.other, &["rev-parse", "HEAD"]).stdout;
    assert_eq!(
        String::from_utf8(other_head).unwrap().trim(),
        fixture.initial
    );
    git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
}

// E-open: the editor opened the job's tree before the pass. While the job's private
// configuration names a fsmonitor, filter, textconv and external diff (and its private
// attributes select them for every file), status and diff through that open repository still run
// and run none of them.

const OPEN_EDITOR_JOB: &str = r#"
PRIVATE=$(git rev-parse --absolute-git-dir)
attempt "private config" 'hostile_config "$PRIVATE/config"'
attempt "private attributes" 'printf "* filter=evil diff=evil\n" > "$PRIVATE/info/attributes"'
"#;

fn open_editor_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("private config", "written");
    outcome.expect("private attributes", "written");
    let report = &outcome.report;
    assert_eq!(
        fs::canonicalize(&fixture.editors.tree.git_dir).unwrap(),
        fixture.tree_git_dir,
        "the open repository is pinned to the tree's own git directory"
    );
    let status = report
        .open_tree_status
        .as_ref()
        .unwrap_or_else(|error| panic!("status through the open repository ({phase:?}): {error}"));
    assert!(status.iter().any(|path| path == "a.txt"), "{status:?}");
    let diff = report
        .open_tree_diff
        .as_ref()
        .unwrap_or_else(|error| panic!("diff through the open repository ({phase:?}): {error}"));
    assert!(diff.contains("+changed"), "{diff}");
    if phase != Phase::AfterHandBack {
        let error = report.editor_tree_open.as_ref().unwrap_err();
        assert!(error.contains("written by a bongos job"), "{error}");
    }
}

// A commit graph, and separately a multi-pack index, planted in the private `objects/`: git would
// read either as trusted, so the import refuses the object directory and nothing moves. The
// refusal stops at the first entry it dislikes, so each file gets a row of its own.

const COMMIT_GRAPH_JOB: &str = r#"
PRIVATE=$(git rev-parse --absolute-git-dir)
attempt "commit" 'git commit -q --allow-empty -m "job work"'
attempt "pack" 'git -c pack.writeReverseIndex=false repack -q -d -l && rm -f "$PRIVATE/objects/info/packs"'
attempt "commit graph" 'git commit-graph write --reachable --no-progress && test -f "$PRIVATE/objects/info/commit-graph"'
attempt "private config" 'hostile_config "$PRIVATE/config"'
"#;

const MULTI_PACK_INDEX_JOB: &str = r#"
PRIVATE=$(git rev-parse --absolute-git-dir)
attempt "commit" 'git commit -q --allow-empty -m "job work"'
attempt "pack" 'git -c pack.writeReverseIndex=false repack -q -d -l && rm -f "$PRIVATE/objects/info/packs"'
attempt "multi-pack index" 'git multi-pack-index write --no-progress && test -f "$PRIVATE/objects/pack/multi-pack-index"'
attempt "private config" 'hostile_config "$PRIVATE/config"'
"#;

fn commit_graph_check(fixture: &Fixture, outcome: &Outcome, _: Phase) {
    for attempt in ["private config", "commit", "pack", "commit graph"] {
        outcome.expect(attempt, "written");
    }
    fixture.assert_shared_refs_unchanged();
}

fn multi_pack_index_check(fixture: &Fixture, outcome: &Outcome, _: Phase) {
    for attempt in ["private config", "commit", "pack", "multi-pack index"] {
        outcome.expect(attempt, "written");
    }
    fixture.assert_shared_refs_unchanged();
}

fn commit_graph_import(fixture: &Fixture, result: &anyhow::Result<ImportReport>) {
    assert_objects_refused(fixture, result, &["commit-graph"]);
    git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
}

fn multi_pack_index_import(fixture: &Fixture, result: &anyhow::Result<ImportReport>) {
    assert_objects_refused(fixture, result, &["multi-pack-index"]);
    git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
}

fn commit_graph_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        let objects = fixture.scratch.join("git/objects");
        assert!(objects.join("info/commit-graph").is_file());
        fixture.assert_plain_git_fires(&fixture.tree, &[]);
    }
    .boxed_local()
}

fn multi_pack_index_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        let objects = fixture.scratch.join("git/objects");
        assert!(objects.join("pack/multi-pack-index").is_file());
        fixture.assert_plain_git_fires(&fixture.tree, &[]);
    }
    .boxed_local()
}

// A crashed pass: the process that prepared the tree dies while the job runs, and the job with
// it. A lane's next job start recovers the tree and resets it: nothing from the private directory
// runs, nothing is imported and the shared repository is intact.

const CRASHED_PASS_JOB: &str = r#"
attempt "private config" 'hostile_config "$(git rev-parse --absolute-git-dir)/config"'
attempt "commit" 'git commit -q --allow-empty -m "crashed work"'
attempt "untracked file" 'printf "job\n" > "$TREE/untracked.txt"'
"#;

fn crashed_pass_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    for attempt in ["private config", "commit", "untracked file"] {
        outcome.expect(attempt, "written");
    }
    fixture.assert_shared_refs_unchanged();
    let report = &outcome.report;
    for result in &report.safe_git_tree {
        assert!(
            result.is_ok(),
            "SafeGit in the job tree ({phase:?}): {result:?}"
        );
    }
    let marker = fixture.tree_git_dir.join(private_git::MARKER);
    let private = fixture.scratch.join(private_git::PRIVATE_DIR);
    match phase {
        Phase::During => {
            // The owner is alive: neither recovery nor another preparation touches the tree.
            assert_eq!(
                private_git::recover(&fixture.tree_git_dir).unwrap(),
                RecoverOutcome::InUse
            );
            let busy = smol::block_on(private_git::prepare(
                &fixture.tree,
                &fixture.tree_git_dir,
                &fixture.scratch,
            ))
            .unwrap_err();
            assert!(format!("{busy:#}").contains(private_git::BUSY), "{busy:#}");
            assert!(marker.is_file());
        }
        Phase::AfterJob => {
            // The dead owner's marker and the redirected `.git` are still there.
            assert!(marker.is_file());
            assert!(private.is_dir());
            assert!(report.tree_record.is_err(), "{report:?}");
            let error = report.editor_tree_open.as_ref().unwrap_err();
            assert!(error.contains("written by a bongos job"), "{error}");
        }
        Phase::AfterRecovery => {
            assert!(!marker.exists());
            assert!(!private.exists());
            assert_eq!(report.tree_record, Ok(fixture.tree_git_dir.clone()));
            assert!(report.editor_tree_open.is_ok(), "{report:?}");
            assert_eq!(
                fs::read_to_string(fixture.tree.join("a.txt")).unwrap(),
                "initial\n"
            );
            assert!(!fixture.tree.join("untracked.txt").exists());
            let head = git(&fixture.tree, &["rev-parse", "HEAD"]).stdout;
            assert_eq!(String::from_utf8(head).unwrap().trim(), fixture.initial);
            git(&fixture.main, &["fsck", "--no-progress", "--strict"]);
        }
        phase => panic!("unexpected phase {phase:?}"),
    }
}

// A main-worktree job: the primary checkout is the job tree and its `.git` is masked read-only.
// Every write to the git directory fails, the work tree is writable, and the editor and `SafeGit`
// open the checkout throughout, during the job too.

const MASKED_MAIN_GIT_JOB: &str = r##"
attempt "work tree file" 'printf "job\n" > "$MAIN/job.txt"'
attempt "main config" 'hostile_config "$MAIN/.git/config"'
attempt "main hook" 'printf "#!/bin/sh\ntouch \"$MARKERS/hook\"\n" > "$MAIN/.git/hooks/post-index-change" && chmod +x "$MAIN/.git/hooks/post-index-change"'
attempt "commit in main" 'git -C "$MAIN" commit -q --allow-empty -m job'
attempt "rename .git" 'mv "$MAIN/.git" "$MAIN/.git-moved"'
attempt "remove .git" 'rm -rf "$MAIN/.git"'
"##;

fn masked_main_git_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("work tree file", "written");
    for attempt in [
        "main config",
        "main hook",
        "commit in main",
        "rename .git",
        "remove .git",
    ] {
        outcome.expect(attempt, "blocked");
    }
    let config = fs::read_to_string(fixture.common.join("config")).unwrap();
    assert!(!config.contains("fsmonitor"), "{config}");
    assert!(!fixture.common.join("hooks/post-index-change").exists());
    assert!(!fixture.main.join(".git-moved").exists());
    fixture.assert_shared_refs_unchanged();
    let report = &outcome.report;
    for result in &report.safe_git_tree {
        assert!(
            result.is_ok(),
            "SafeGit in the checkout ({phase:?}): {result:?}"
        );
    }
    assert!(report.editor_tree_open.is_ok(), "{phase:?}: {report:?}");
    assert!(!util::job_roots::contains(&fixture.common));
    assert_eq!(
        util::job_roots::contains(&fixture.main),
        phase == Phase::During,
        "{phase:?}"
    );
}

fn masked_main_git_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        // Had the configuration write landed, the user's git in the checkout would run it.
        let config = fixture.common.join("config");
        let original = fs::read(&config).unwrap();
        fixture.write_hostile_config(&config);
        fixture.assert_plain_git_fires(&fixture.main, &[]);
        fs::write(&config, original).unwrap();
    }
    .boxed_local()
}

/// A clone of the repository nested in the primary checkout, at `vendor/clone`, with `a.txt`
/// changed the same way.
fn plant_nested_clone(fixture: &Fixture) {
    let clone = fixture.main.join("vendor/clone");
    fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git_os(
        &fixture.main,
        &[
            OsStr::new("clone"),
            OsStr::new("-q"),
            fixture.main.as_os_str(),
            clone.as_os_str(),
        ],
    );
    change_keeping_size(&clone.join("a.txt"), "changed\n");
}

// A main-worktree job and nested repositories: it tries to rewrite a nested clone's
// configuration (masked, refused) and makes a new nested repository naming the commands. The new
// repository stays refused by the editor and `SafeGit` after the job ends and after its owner
// crashed, while the checkout itself opens normally again.

const MAIN_NESTED_JOB: &str = r#"
attempt "nested clone config" 'hostile_config "$MAIN/vendor/clone/.git/config"'
attempt "new nested repository" 'git init -q "$MAIN/newrepo" && cd "$MAIN/newrepo" &&
    printf "a.txt filter=evil diff=evil\n" > .gitattributes && printf "initial\n" > a.txt &&
    git add .gitattributes a.txt && git commit -q -m nested &&
    printf "changed\n" > a.txt && touch -d @1000000000 a.txt && hostile_config .git/config'
"#;

fn main_nested_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("nested clone config", "blocked");
    outcome.expect("new nested repository", "written");
    let clone_config = fs::read_to_string(fixture.main.join("vendor/clone/.git/config")).unwrap();
    assert!(!clone_config.contains("fsmonitor"), "{clone_config}");

    let nested = fixture.main.join("newrepo");
    let error = open_editor(&nested.join(".git"), &fixture.executor)
        .err()
        .unwrap_or_else(|| panic!("the editor opened the new nested repository ({phase:?})"));
    assert!(
        format!("{error:#}").contains("written by a bongos job"),
        "{error:#}"
    );
    for args in [&["status"][..], &["diff"][..]] {
        let refused = smol::block_on(
            fixture
                .isolated(SafeGit::new(nested.join(".git"), &nested))
                .command(args),
        );
        let error = refused.err().unwrap_or_else(|| {
            panic!("SafeGit ran {args:?} in the new nested repository ({phase:?})")
        });
        assert!(
            format!("{error:#}").contains("job-written git directory"),
            "{error:#}"
        );
    }
    assert!(util::job_roots::contains(&nested));
    fixture.assert_no_markers("the new nested repository");

    let report = &outcome.report;
    assert!(report.editor_tree_open.is_ok(), "{phase:?}: {report:?}");
    match phase {
        Phase::During => {
            assert!(util::job_roots::contains(&fixture.main));
            for result in &report.safe_git_tree {
                let error = result.as_ref().unwrap_err();
                assert!(error.contains("nested repository"), "{error}");
            }
        }
        Phase::AfterJob | Phase::AfterOwnerCrash => {
            assert!(!util::job_roots::contains(&fixture.main));
            for result in &report.safe_git_tree {
                assert!(
                    result.is_ok(),
                    "SafeGit in the checkout ({phase:?}): {result:?}"
                );
            }
        }
        phase => panic!("unexpected phase {phase:?}"),
    }
}

fn main_nested_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        fixture.assert_plain_git_fires(&fixture.main.join("newrepo"), &[]);
        // Had the write to the clone's configuration landed, git in the clone would run it.
        let clone = fixture.main.join("vendor/clone");
        let config = clone.join(".git/config");
        let original = fs::read(&config).unwrap();
        fixture.write_hostile_config(&config);
        fixture.assert_plain_git_fires(&clone, &[]);
        fs::write(&config, original).unwrap();
    }
    .boxed_local()
}

// The job rewrites the tree's `.git` to point git at a directory with its configuration.

const REWRITTEN_DOT_GIT_JOB: &str = r#"
attempt "evil git" 'cp -R "$SCRATCH/git" "$SCRATCH/evil-git" && hostile_config "$SCRATCH/evil-git/config"'
attempt "overwrite .git" 'printf "gitdir: %s\n" "$SCRATCH/evil-git" > "$TREE/.git"'
attempt "rename .git" 'mv "$TREE/.git" "$TREE/.git-moved"'
attempt "remove .git" 'rm "$TREE/.git"'
attempt "replace .git" 'printf "gitdir: %s\n" "$SCRATCH/evil-git" > "$SCRATCH/dot-git" && mv -f "$SCRATCH/dot-git" "$TREE/.git"'
"#;

fn rewritten_dot_git_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("evil git", "written");
    for attempt in [
        "overwrite .git",
        "rename .git",
        "remove .git",
        "replace .git",
    ] {
        outcome.expect(attempt, "blocked");
    }
    assert!(!fixture.tree.join(".git-moved").exists());
    let dot_git = fs::read_to_string(fixture.tree.join(".git")).unwrap();
    if phase != Phase::AfterHandBack {
        assert_eq!(
            dot_git,
            format!("gitdir: {}\n", fixture.scratch.join("git").display())
        );
        assert!(outcome.report.tree_record.is_err());
    } else {
        assert_eq!(outcome.report.tree_record, Ok(fixture.tree_git_dir.clone()));
    }
}

fn rewritten_dot_git_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        let dot_git = fixture.tree.join(".git");
        let prepared = fs::read(&dot_git).unwrap();
        fs::write(
            &dot_git,
            format!("gitdir: {}\n", fixture.scratch.join("evil-git").display()),
        )
        .unwrap();
        // The editor's open repository is pinned to the git directory it found at open, and
        // `SafeGit` takes it from bongos's record: neither follows the rewritten `.git`.
        let _ = editor_status_diff_blame(&fixture.editors.tree).await;
        for args in [&["status"][..], &["diff"][..]] {
            let mut command = smol::block_on(fixture.safe_git_in_tree().command(args)).unwrap();
            smol::block_on(command.output()).unwrap();
        }
        fixture.assert_no_markers("the pinned editor and SafeGit with a rewritten .git");
        fixture.assert_plain_git_fires(&fixture.tree, &[]);
        fs::write(&dot_git, prepared).unwrap();
    }
    .boxed_local()
}

// The job makes a nested repository in its tree whose configuration names the commands.

const NESTED_REPOSITORY_JOB: &str = r#"
attempt "nested repository" 'git init -q "$TREE/nested" && cd "$TREE/nested" &&
    printf "a.txt filter=evil diff=evil\n" > .gitattributes && printf "initial\n" > a.txt &&
    git add .gitattributes a.txt && git commit -q -m nested &&
    printf "changed\n" > a.txt && touch -d @1000000000 a.txt && hostile_config .git/config'
"#;

fn nested_repository_check(fixture: &Fixture, outcome: &Outcome, phase: Phase) {
    outcome.expect("nested repository", "written");
    for result in &outcome.report.safe_git_tree {
        let error = result.as_ref().unwrap_err();
        assert!(error.contains("nested repository"), "{phase:?}: {error}");
    }
    let error = open_editor(&fixture.tree.join("nested/.git"), &fixture.executor)
        .err()
        .unwrap_or_else(|| panic!("the editor opened the nested repository ({phase:?})"));
    assert!(
        format!("{error:#}").contains("written by a bongos job"),
        "{error:#}"
    );
    fixture.assert_no_markers("opening the nested repository");
}

fn nested_repository_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move { fixture.assert_plain_git_fires(&fixture.tree.join("nested"), &[]) }.boxed_local()
}

// A declared directory containing the repository's git data: the build refuses the job.

fn declared_repository_dirs(fixture: &Fixture) -> Vec<PathBuf> {
    vec![
        fixture.main.clone(),
        fixture.common.clone(),
        fixture.common.join("worktrees"),
        fixture.base.clone(),
    ]
}

fn declared_repository_check(_: &Fixture, outcome: &Outcome, _: Phase) {
    assert!(outcome.attempts.is_empty());
    assert!(
        outcome.report.editor_tree_open.is_ok(),
        "{:?}",
        outcome.report
    );
    for result in &outcome.report.safe_git_tree {
        assert!(result.is_ok(), "{result:?}");
    }
}

fn declared_repository_control(fixture: &Fixture) -> LocalBoxFuture<'_, ()> {
    async move {
        // The same commands as global configuration fire in the primary checkout.
        let config = fixture.base.join("hostile.config");
        fixture.write_hostile_config(&config);
        fixture.assert_plain_git_fires(&fixture.main, &[("GIT_CONFIG_GLOBAL", &config)]);
    }
    .boxed_local()
}

/// One test per row, named by the row's test function.
macro_rules! escapes {
    ($($test:ident: $row:expr,)*) => {
        $(
            #[gpui::test]
            async fn $test(cx: &mut TestAppContext) {
                run_row(&$row, cx).await;
            }
        )*
    };
}

// The table of recorded escapes. A newly found escape is added as a new row in the same PR as its
// fix.
escapes! {
    known_escape_e1_planted_commondir: Escape {
        name: "E1: planted <common>/commondir",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Abandon,
            sandbox: SandboxUse::Required,
        },
        job: E1_JOB,
        host_check: e1_check,
        control: e1_control,
    },
    known_escape_e2_other_worktree_commondir_and_config_worktree: Escape {
        name: "E2: rewritten <common>/worktrees/<other>/commondir and config.worktree",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Abandon,
            sandbox: SandboxUse::Required,
        },
        job: E2_JOB,
        host_check: e2_check,
        control: e2_control,
    },
    known_escape_private_git_config: Escape {
        name: "the private git config",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(imported_cleanly),
            sandbox: SandboxUse::Required,
        },
        job: PRIVATE_CONFIG_JOB,
        host_check: private_config_check,
        control: private_config_control,
    },
    known_escape_rewritten_dot_git: Escape {
        name: "a rewritten .git",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Abandon,
            sandbox: SandboxUse::Required,
        },
        job: REWRITTEN_DOT_GIT_JOB,
        host_check: rewritten_dot_git_check,
        control: rewritten_dot_git_control,
    },
    known_escape_nested_repository: Escape {
        name: "a nested repository",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(imported_cleanly),
            sandbox: SandboxUse::Required,
        },
        job: NESTED_REPOSITORY_JOB,
        host_check: nested_repository_check,
        control: nested_repository_control,
    },
    known_escape_declared_directory_containing_the_repository: Escape {
        name: "a declared directory containing the repository",
        setup: Setup::Refused { declared_dirs: declared_repository_dirs },
        job: "",
        host_check: declared_repository_check,
        control: declared_repository_control,
    },
    known_escape_alternates_and_symlinks_in_the_private_directory: Escape {
        name: "alternates and symlinks in the private directory",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(private_links_import),
            sandbox: SandboxUse::IfPresent,
        },
        job: PRIVATE_LINKS_JOB,
        host_check: private_links_check,
        control: private_config_control,
    },
    known_escape_tag_and_moved_foreign_branch_at_import: Escape {
        name: "a tag and a moved foreign branch at import",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(foreign_refs_import),
            sandbox: SandboxUse::IfPresent,
        },
        job: FOREIGN_REFS_JOB,
        host_check: foreign_refs_check,
        control: private_config_control,
    },
    known_escape_e_open_repository_opened_before_the_pass: Escape {
        name: "E-open: a repository the editor opened before the pass",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(imported_cleanly),
            sandbox: SandboxUse::Required,
        },
        job: OPEN_EDITOR_JOB,
        host_check: open_editor_check,
        control: private_config_control,
    },
    known_escape_commit_graph_in_private_objects: Escape {
        name: "a commit graph in the private objects",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(commit_graph_import),
            sandbox: SandboxUse::IfPresent,
        },
        job: COMMIT_GRAPH_JOB,
        host_check: commit_graph_check,
        control: commit_graph_control,
    },
    known_escape_multi_pack_index_in_private_objects: Escape {
        name: "a multi-pack index in the private objects",
        setup: Setup::LinkedTree {
            hand_back: HandBack::Import(multi_pack_index_import),
            sandbox: SandboxUse::IfPresent,
        },
        job: MULTI_PACK_INDEX_JOB,
        host_check: multi_pack_index_check,
        control: multi_pack_index_control,
    },
    known_escape_crashed_pass_then_lane_reset: Escape {
        name: "a crashed pass followed by a lane's reset --hard",
        setup: Setup::CrashedPass,
        job: CRASHED_PASS_JOB,
        host_check: crashed_pass_check,
        control: private_config_control,
    },
    known_escape_main_worktree_masked_git_stays_openable: Escape {
        name: "a main-worktree job whose masked .git stays openable",
        setup: Setup::MainWorktree { prepare: |_| {} },
        job: MASKED_MAIN_GIT_JOB,
        host_check: masked_main_git_check,
        control: masked_main_git_control,
    },
    known_escape_main_worktree_nested_clone_and_new_nested_repository: Escape {
        name: "a main-worktree job, a nested clone and a new nested repository",
        setup: Setup::MainWorktree { prepare: plant_nested_clone },
        job: MAIN_NESTED_JOB,
        host_check: main_nested_check,
        control: main_nested_control,
    },
}
