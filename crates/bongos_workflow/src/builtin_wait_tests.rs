//! Tests of the `wait` built-in with a fake `gh` script, so nothing reaches GitHub. The sandbox
//! rows run under the real OS sandbox and skip without `bwrap`; the others use a probe that finds
//! no sandbox. Every wait uses a fake clock, so nothing sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{Kind, TaskFile, Todo, TodoState};
use bongos_workflow_model::definition::{AgentKind, parse};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::unsandboxed::{AllowedBy, Mode, Permit, REFUSED_TEXT};
use bongos_workflow_model::{Event, Failure, FailureReason, LayerDirs, NodeKey, RunState};
use chrono::{TimeZone as _, Utc};
use gpui::TestAppContext;
use sandbox::{SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::builtin_run::{EnvLookup, NO_GH_HOST, NO_GH_REPO, WaitClock, WaitNode};
use crate::builtin_wait::{WaitCondition, WaitLaunch, WaitOutcome, WaitRequest, wait};
use crate::dispatch_program::scratch_dir;
use crate::lifecycle::{
    BY_PERSON, Lifecycle, RunStatus, RunWorktrees, StartChecks, StartContext, StartRequest,
};
use crate::runner_program::{SandboxProbe, os_sandbox_probe};
use crate::runtime::RunBinding;
use crate::runtime::fake::FakeRunner;
use crate::worktrees::{Git, RunWorktree, TaskCheckout, WorktreeError};

/// A temporary directory holding a fake `gh` that prints `stdout` and exits with `code`.
fn fake_gh(stdout: &str, code: i32) -> (TempDir, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|path| path != std::path::Path::new("/tmp"));
    let temp = match base {
        Some(base) => TempDir::new_in(base).unwrap(),
        None => TempDir::new().unwrap(),
    };
    let script = temp.path().join("gh");
    fs::write(
        &script,
        format!("#!/bin/sh\ncat <<'EOF'\n{stdout}\nEOF\nexit {code}\n"),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (temp, script)
}

/// Like [`fake_gh`], but also prints `stderr`.
fn fake_gh_stderr(stdout: &str, stderr: &str, code: i32) -> (TempDir, PathBuf) {
    let (temp, script) = fake_gh(stdout, code);
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat <<'EOF'\n{stdout}\nEOF\ncat >&2 <<'EOF'\n{stderr}\nEOF\nexit {code}\n"
        ),
    )
    .unwrap();
    (temp, script)
}

fn request(condition: WaitCondition, gh: PathBuf) -> WaitRequest {
    WaitRequest {
        condition,
        pr: "42".to_string(),
        command: Vec::new(),
        merge_is_ship: false,
        worktree: std::env::temp_dir(),
        gh,
        elapsed: Duration::ZERO,
        give_up: Some(Duration::from_millis(5)),
        launch: WaitLaunch::AsUser,
        env: BTreeMap::new(),
        scratch: PathBuf::new(),
        repo: None,
    }
}

fn timed_out(mut request: WaitRequest) -> WaitRequest {
    request.elapsed = Duration::from_millis(5);
    request
}

fn is_timed_out(outcome: &WaitOutcome) -> bool {
    matches!(outcome, WaitOutcome::TimedOut(reason) if reason == "wait timed out")
}

#[gpui::test]
async fn ci_green_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let green = r#"[{"bucket":"pass"},{"bucket":"skipping"}]"#;
    let pending = r#"[{"bucket":"pass"},{"bucket":"pending"}]"#;
    let red = r#"[{"bucket":"pass"},{"bucket":"fail"}]"#;
    let (_a, gh) = fake_gh(green, 0);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Satisfied
    );
    let (_b, gh) = fake_gh(pending, 8);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh.clone())).await,
        WaitOutcome::Waiting
    );
    assert!(is_timed_out(
        &wait(&timed_out(request(WaitCondition::CiGreen, gh))).await
    ));
    let (_c, gh) = fake_gh(red, 1);
    assert!(matches!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Failed(reason) if reason.contains("failed")
    ));
    // Unreadable output falls back to the exit code.
    let (_d, gh) = fake_gh("", 8);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Waiting
    );
}

#[gpui::test]
async fn pr_merged_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_a, gh) = fake_gh(r#"{"state":"MERGED"}"#, 0);
    assert_eq!(
        wait(&request(WaitCondition::PrMerged, gh)).await,
        WaitOutcome::Satisfied
    );
    let (_b, gh) = fake_gh(r#"{"state":"OPEN"}"#, 0);
    assert_eq!(
        wait(&request(WaitCondition::PrMerged, gh.clone())).await,
        WaitOutcome::Waiting
    );
    assert!(is_timed_out(
        &wait(&timed_out(request(WaitCondition::PrMerged, gh))).await
    ));
    let (_c, gh) = fake_gh(r#"{"state":"CLOSED"}"#, 0);
    assert!(matches!(
        wait(&request(WaitCondition::PrMerged, gh)).await,
        WaitOutcome::Failed(reason) if reason.contains("closed")
    ));
}

#[gpui::test]
async fn command_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let gh = PathBuf::from("gh");
    let mut ok = request(WaitCondition::Command, gh.clone());
    ok.command = vec!["true".to_string()];
    assert_eq!(wait(&ok).await, WaitOutcome::Satisfied);
    let mut no = request(WaitCondition::Command, gh);
    no.command = vec!["false".to_string()];
    assert_eq!(wait(&no).await, WaitOutcome::Waiting);
    assert!(is_timed_out(&wait(&timed_out(no)).await));
}

#[gpui::test]
async fn deploy_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let gh = PathBuf::from("gh");
    let mut ship = request(WaitCondition::Deploy, gh.clone());
    ship.merge_is_ship = true;
    assert_eq!(wait(&ship).await, WaitOutcome::Satisfied);
    let gate = request(WaitCondition::Deploy, gh.clone());
    assert_eq!(
        wait(&gate).await,
        WaitOutcome::OpenGate("Confirm deploy of 42?".to_string())
    );
    let mut with_command = request(WaitCondition::Deploy, gh);
    with_command.command = vec!["false".to_string()];
    assert_eq!(wait(&with_command).await, WaitOutcome::Waiting);
    assert!(is_timed_out(&wait(&timed_out(with_command.clone())).await));
    with_command.command = vec!["true".to_string()];
    assert_eq!(wait(&with_command).await, WaitOutcome::Satisfied);
}

#[gpui::test]
async fn edge_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    // No checks yet: gh exits 1 with "no checks reported" on stderr; that is waiting.
    let (_a, gh) = fake_gh_stderr("", "no checks reported on the 'x' branch", 1);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Waiting
    );
    let (_b, gh) = fake_gh_stderr("", "No Checks Reported", 1);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Waiting
    );
    // A cancelled check fails.
    let (_c, gh) = fake_gh(r#"[{"bucket":"cancel"}]"#, 1);
    assert!(matches!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Failed(_)
    ));
    // A draft pull request is waiting.
    let (_d, gh) = fake_gh(r#"{"state":"DRAFT"}"#, 0);
    assert_eq!(
        wait(&request(WaitCondition::PrMerged, gh.clone())).await,
        WaitOutcome::Waiting
    );
    // Bad pull request names are refused.
    for pr in ["-x", ""] {
        let mut bad = request(WaitCondition::PrMerged, gh.clone());
        bad.pr = pr.to_string();
        assert!(matches!(wait(&bad).await, WaitOutcome::Failed(_)));
        let mut bad = request(WaitCondition::CiGreen, gh.clone());
        bad.pr = pr.to_string();
        assert!(matches!(wait(&bad).await, WaitOutcome::Failed(_)));
    }
    // A command wait with no command fails.
    for condition in [WaitCondition::Command] {
        let none = request(condition, gh.clone());
        assert_eq!(
            wait(&none).await,
            WaitOutcome::Failed("the wait declares no command".to_string())
        );
    }
}

// The wait in the sandbox: one decision at start, then confined passes.

const RUN: &str = "01890a5d-ac96-774b-bcce-b30200000057";

/// Dumps the shell's own environment, as it was started, into the scratch directory.
const DUMP_ENV: &str = r#"command = ["sh", "-c", "cat /proc/$$/environ > \"$TMPDIR/env\""]"#;

fn temp() -> TempDir {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|path| path != Path::new("/tmp"));
    match base {
        Some(base) => TempDir::new_in(base).unwrap(),
        None => TempDir::new().unwrap(),
    }
}

fn test_git(home: &Path) -> Git {
    Git::new("git")
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
}

/// A clock whose `sleep` only moves time forward.
#[derive(Default)]
struct FakeClock(Mutex<Duration>);

impl WaitClock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

fn no_sandbox() -> SandboxProbe {
    Arc::new(|_, _| Err("bwrap not found (test)".to_string()))
}

/// A probe that finds no sandbox and counts its calls.
fn counting_probe(calls: Arc<AtomicUsize>) -> SandboxProbe {
    Arc::new(move |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err("bwrap not found (test)".to_string())
    })
}

/// The writer's environment, as these pairs only.
fn vars(pairs: &[(&str, &str)]) -> EnvLookup {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    Arc::new(move |name| map.get(name).cloned())
}

fn path_var() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
}

fn person_grant() -> Permit {
    Permit::Allowed(AllowedBy::RunConsent {
        at: "2026-01-02T03:04:05Z".to_string(),
        by: BY_PERSON.to_string(),
    })
}

/// A task worktree (a git repository, with `origin` when given), a home, the run's directory and
/// a directory outside all of them.
struct Place {
    _temp: TempDir,
    home: PathBuf,
    worktree: PathBuf,
    run_dir: PathBuf,
    outside: PathBuf,
}

fn place(origin: Option<&str>) -> Place {
    let temp = temp();
    let root = temp.path().canonicalize().unwrap();
    let [home, worktree, run_dir, outside] =
        ["home", "worktree", "run", "outside"].map(|name| root.join(name));
    for dir in [&home, &worktree, &run_dir, &outside] {
        fs::create_dir_all(dir).unwrap();
    }
    let git = |args: &[&str]| futures::executor::block_on(test_git(&home).run(&worktree, args));
    git(&["init", "-q", "-b", "task"]).unwrap();
    if let Some(origin) = origin {
        git(&["remote", "add", "origin", origin]).unwrap();
    }
    Place {
        _temp: temp,
        home,
        worktree,
        run_dir,
        outside,
    }
}

fn binding(place: &Place) -> RunBinding {
    RunBinding {
        workflow: "t".to_string(),
        task_id: "todo-1".to_string(),
        task_dir: place.run_dir.clone(),
        run_dir: place.run_dir.clone(),
        attempt_dir: place.run_dir.clone(),
        worktree: RunWorktree {
            run_dir: place.run_dir.clone(),
            path: place.run_dir.join("run"),
            branch: "run".to_string(),
        },
        checkout: TaskCheckout {
            slug: "todo".to_string(),
            worktree: Some(place.worktree.clone()),
            branch: Some("task".to_string()),
        },
        inputs: Vec::new(),
        setup: Default::default(),
    }
}

fn wait_key() -> NodeKey {
    NodeKey::new("wait", 1)
}

fn scratch(place: &Place) -> PathBuf {
    scratch_dir(&binding(place), &wait_key(), 1)
}

fn wait_node(
    place: &Place,
    gh: impl Into<PathBuf>,
    probe: SandboxProbe,
    vars: EnvLookup,
) -> WaitNode {
    WaitNode::new(Arc::new(FakeClock::default()), gh)
        .with_probe(probe)
        .with_git(test_git(&place.home))
        .with_vars(vars)
}

/// Starts the one `wait` node, its `with` and any other keys (`extra`) as given.
fn start_wait(
    node: &WaitNode,
    place: &Place,
    with: &str,
    extra: &str,
    permit: Permit,
    mode: Mode,
) -> Result<(), Failure> {
    let definition = parse(&format!(
        r#"
schema = 1
name = "t"
start = "wait"

[nodes.wait]
kind = "program"
use = "wait"
with = {with}
{extra}
routes = [{{ to = "done" }}]

[ends.done]
kind = "success"
"#
    ))
    .unwrap();
    let state = RunState::default();
    let binding = binding(place);
    node.start(BuiltinStart {
        run: RUN,
        definition: &definition,
        state: &state,
        key: &wait_key(),
        try_number: 1,
        binding: Some(&binding),
        permit,
        mode,
    })
}

fn assert_satisfied(node: &WaitNode) {
    let finished = node.wait_finished(1);
    let outcome = &finished[0].outcome;
    assert_eq!(
        outcome.as_ref().map(|result| result["result"].clone()).ok(),
        Some(serde_json::json!("satisfied")),
        "{outcome:?}"
    );
}

/// The variables in a `/proc/<pid>/environ` copy.
fn read_env(path: &Path) -> BTreeMap<String, String> {
    fs::read(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let (name, value) = entry.split_once('=')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

fn names(env: &BTreeMap<String, String>) -> BTreeSet<&str> {
    env.keys().map(String::as_str).collect()
}

fn toml_text(text: &str) -> String {
    toml::Value::from(text).to_string()
}

/// A fake `gh` that records its arguments and environment in `record` and prints `stdout`.
fn recording_gh(record: &Path, stdout: &str) -> (TempDir, PathBuf) {
    let temp = temp();
    let script = temp.path().join("gh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{dir}/args'\ncat /proc/$$/environ > '{dir}/env'\ncat <<'EOF'\n{stdout}\nEOF\n",
            dir = record.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (temp, script)
}

/// Whether the real OS sandbox works here; prints why a sandbox row is skipped.
fn sandbox_ready(place: &Place) -> bool {
    // The sandbox mounts an empty /tmp, which would hide a worktree there.
    if place.worktree.starts_with("/tmp") {
        println!("skipped: the temp directory is under /tmp, which the sandbox hides");
        return false;
    }
    let policy = SandboxPolicy {
        fs: SandboxFsPolicy::Restricted {
            writable_paths: Vec::new(),
        },
        network: SandboxNetPolicy::Blocked,
        git: Default::default(),
    };
    if os_sandbox_probe()(&policy, &place.worktree).is_err() {
        println!("skipped: bwrap not found");
        return false;
    }
    true
}

#[test]
fn a_sandboxed_wait_sees_only_its_named_variables() {
    let place = place(None);
    if !sandbox_ready(&place) {
        return;
    }
    let path = path_var();
    let home = place.home.to_string_lossy().into_owned();
    let node = wait_node(
        &place,
        "gh",
        os_sandbox_probe(),
        vars(&[
            ("PATH", &path),
            ("HOME", &home),
            ("LANG", "C.UTF-8"),
            ("NAMED", "kept"),
            ("SECRET", "not named"),
            ("GH_TOKEN", "a gh condition's only"),
        ]),
    );
    let with =
        format!(r#"{{ condition = "command", {DUMP_ENV}, env = ["NAMED"], give_up_minutes = 5 }}"#);
    start_wait(&node, &place, &with, "", Permit::Ask, Mode::Ask).unwrap();
    assert_satisfied(&node);
    let scratch = scratch(&place);
    let env = read_env(&scratch.join("env"));
    // No network is declared, so the wrap adds no proxy variables of its own: the process sees
    // exactly the wrap's environment, which is the named variables and the four every wait gets,
    // plus the `PWD` Bubblewrap sets for the working directory it enters.
    assert_eq!(
        names(&env),
        BTreeSet::from(["HOME", "LANG", "NAMED", "PATH", "PWD", "TMPDIR"])
    );
    assert_eq!(env["NAMED"], "kept");
    assert_eq!(Path::new(&env["TMPDIR"]), scratch);
    assert_eq!(Path::new(&env["PWD"]), place.worktree);
}

#[test]
fn a_sandboxed_wait_writes_nowhere_but_its_scratch() {
    let place = place(None);
    if !sandbox_ready(&place) {
        return;
    }
    let in_worktree = place.worktree.join("written");
    let outside = place.outside.join("written");
    let script = format!(
        r#"echo ok > "$TMPDIR/ok" && ! (echo x > '{}') 2>/dev/null && ! (echo x > '{}') 2>/dev/null"#,
        in_worktree.display(),
        outside.display()
    );
    let with = format!(
        r#"{{ condition = "command", command = ["sh", "-c", {}], give_up_minutes = 5 }}"#,
        toml_text(&script)
    );
    let path = path_var();
    let node = wait_node(&place, "gh", os_sandbox_probe(), vars(&[("PATH", &path)]));
    start_wait(&node, &place, &with, "", Permit::Ask, Mode::Ask).unwrap();
    assert_satisfied(&node);
    assert!(
        scratch(&place).join("ok").exists(),
        "the scratch is writable"
    );
    assert!(!in_worktree.exists(), "the worktree is read-only");
    assert!(!outside.exists(), "nothing else is writable");
}

#[test]
fn ci_green_with_no_host_refuses() {
    let calls = Arc::new(AtomicUsize::new(0));
    let ci_green = r#"{ condition = "ci_green", pr = "42" }"#;
    // No remote, a number for the pull request and no declared host: gh could reach nothing.
    let no_remote = place(None);
    let node = wait_node(&no_remote, "gh", counting_probe(calls.clone()), vars(&[]));
    let failure = start_wait(&node, &no_remote, ci_green, "", Permit::Ask, Mode::Ask).unwrap_err();
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(failure.detail, NO_GH_HOST);
    // A declared host says where, but not which repository.
    let declared = r#"permissions = { network = ["github.com"] }"#;
    let failure = start_wait(
        &node,
        &no_remote,
        ci_green,
        declared,
        Permit::Ask,
        Mode::Ask,
    )
    .unwrap_err();
    assert_eq!(failure.detail, NO_GH_REPO);
    // Neither does a remote that is not on GitHub.
    let gitlab = place(Some("https://gitlab.com/acme/widgets.git"));
    let node = wait_node(&gitlab, "gh", counting_probe(calls.clone()), vars(&[]));
    let failure = start_wait(&node, &gitlab, ci_green, "", Permit::Ask, Mode::Ask).unwrap_err();
    assert_eq!(failure.detail, NO_GH_REPO);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "refused before the probe");
}

#[test]
fn a_with_env_name_that_is_not_set_is_skipped() {
    let place = place(None);
    let path = path_var();
    let node = wait_node(
        &place,
        "gh",
        no_sandbox(),
        vars(&[("PATH", &path), ("NAMED", "kept")]),
    );
    let with = format!(
        r#"{{ condition = "command", {DUMP_ENV}, env = ["NAMED", "MISSING"], give_up_minutes = 5 }}"#
    );
    start_wait(&node, &place, &with, "", person_grant(), Mode::Ask).unwrap();
    assert_satisfied(&node);
    let env = read_env(&scratch(&place).join("env"));
    // HOME and LANG are not set either: skipped, never passed empty.
    assert_eq!(names(&env), BTreeSet::from(["NAMED", "PATH", "TMPDIR"]));
    // A name that is no variable name is refused at start.
    let bad = r#"{ condition = "command", command = ["true"], env = ["NOT-A-NAME"] }"#;
    let failure = start_wait(&node, &place, bad, "", person_grant(), Mode::Ask).unwrap_err();
    assert!(failure.detail.contains("`with.env`"), "{failure:?}");
}

#[test]
fn fallback_keeps_todays_path() {
    let place = place(None);
    let calls = Arc::new(AtomicUsize::new(0));
    let (_gh_dir, gh) = recording_gh(&place.outside, r#"[{"bucket":"pass"}]"#);
    let node = wait_node(&place, gh, counting_probe(calls.clone()), vars(&[]));
    // No host, no repository and a `Refuse` permit: under `fallback` none of it is looked at.
    let with = r#"{ condition = "ci_green", pr = "42", env = ["MISSING"] }"#;
    start_wait(&node, &place, with, "", Permit::Refuse, Mode::Fallback).unwrap();
    assert_satisfied(&node);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no probe");
    let args = fs::read_to_string(place.outside.join("args")).unwrap();
    assert_eq!(args.trim(), "pr checks 42 --json bucket");
    // As the user: the writer's whole environment, not a cleared one.
    let env = read_env(&place.outside.join("env"));
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if !matches!(name.as_ref(), "PWD" | "OLDPWD" | "SHLVL" | "_") {
            assert!(env.contains_key(name.as_ref()), "{name} was not passed");
        }
    }
    assert!(!scratch(&place).exists(), "no scratch directory");
}

#[test]
fn a_refuse_permit_refuses() {
    let place = place(None);
    let marker = place.outside.join("ran");
    let with = format!(
        r#"{{ condition = "command", command = ["touch", {}] }}"#,
        toml_text(&marker.to_string_lossy())
    );
    let path = path_var();
    for mode in [Mode::Ask, Mode::Allow, Mode::Refuse] {
        let node = wait_node(&place, "gh", no_sandbox(), vars(&[("PATH", &path)]));
        let failure = start_wait(&node, &place, &with, "", Permit::Refuse, mode).unwrap_err();
        assert_eq!(failure.reason, FailureReason::SandboxRefused, "{mode:?}");
        assert!(failure.detail.starts_with(REFUSED_TEXT), "{failure:?}");
    }
    // A run a person can answer needs that answer first.
    let node = wait_node(&place, "gh", no_sandbox(), vars(&[("PATH", &path)]));
    let failure = start_wait(&node, &place, &with, "", Permit::Ask, Mode::Ask).unwrap_err();
    assert_eq!(failure.reason, FailureReason::SandboxRequired);
    assert!(!marker.exists(), "nothing ran");
}

#[test]
fn gh_pr_checks_gets_the_repo() {
    let place = place(Some("git@github.com:acme/widgets.git"));
    let path = path_var();
    let (_gh_dir, gh) = recording_gh(&place.outside, r#"[{"bucket":"pass"}]"#);
    let node = wait_node(&place, &gh, no_sandbox(), vars(&[("PATH", &path)]));
    let with = r#"{ condition = "ci_green", pr = "42" }"#;
    start_wait(&node, &place, with, "", person_grant(), Mode::Ask).unwrap();
    assert_satisfied(&node);
    let args = fs::read_to_string(place.outside.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "pr checks 42 --json bucket --repo acme/widgets"
    );

    // A pull request URL names its repository and host, with no remote at all.
    let no_remote = self::place(None);
    let node = wait_node(&no_remote, &gh, no_sandbox(), vars(&[("PATH", &path)]));
    let url = "https://github.com/other/thing/pull/7";
    let with = format!(r#"{{ condition = "ci_green", pr = "{url}" }}"#);
    start_wait(&node, &no_remote, &with, "", person_grant(), Mode::Ask).unwrap();
    assert_satisfied(&node);
    let args = fs::read_to_string(place.outside.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        format!("pr checks {url} --json bucket --repo other/thing")
    );
}

// Through the engine: the runtime asks for a wait as for a program node, and one answer covers
// both.

/// A wait alone.
const AWAIT: &str = r#"
schema = 1
name = "await"
start = "hold"

[nodes.hold]
kind = "program"
use = "wait"
with = { condition = "command", command = ["true"] }
output = { result = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// A program node, then a wait.
const LINT_THEN_AWAIT: &str = r#"
schema = 1
name = "lint-await"
start = "lint"

[nodes.lint]
kind = "program"
script = "lint.py"
output = { summary = { type = "text" } }
routes = [{ to = "hold" }]

[nodes.hold]
kind = "program"
use = "wait"
with = { condition = "command", command = ["true"] }
output = { result = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

struct Checks;

impl StartChecks for Checks {
    fn agent(&self, _kind: AgentKind) -> Result<(), String> {
        Ok(())
    }

    fn interpreter(&self) -> Result<(), String> {
        Ok(())
    }

    fn mcp_servers(&self, _kind: AgentKind) -> Option<BTreeSet<String>> {
        None
    }
}

struct DirWorktrees {
    root: PathBuf,
}

impl RunWorktrees for DirWorktrees {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        let run_dir = self.root.join(format!("{}-{run_id}-{attempt}", task.slug));
        let path = run_dir.join("run");
        fs::create_dir_all(&path).unwrap();
        Ok(RunWorktree {
            run_dir,
            path,
            branch: format!("run-{run_id}-{attempt}"),
        })
    }
}

fn task(dir: PathBuf, file: TaskFile, children: Vec<Task>) -> Task {
    Task {
        slug: dir.file_name().unwrap().to_string_lossy().into_owned(),
        dir,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        definition: None,
        pending: None,
        needs_conversion: false,
        done_hold: None,
        broken_edges: Vec::new(),
        files: BTreeMap::<PathBuf, SystemTime>::new(),
        hashes: BTreeMap::new(),
        children,
    }
}

/// A project with one leaf `leaf-1` (a worktree and an open todo), a profile layer with the two
/// workflows above, and a lifecycle under `ask` whose `lint` finds no sandbox and whose `wait`
/// built-in finds none either.
struct World {
    _temp: TempDir,
    dirs: LayerDirs,
    tree: WorkTree,
    lifecycle: Lifecycle<FakeRunner>,
    wait: Arc<WaitNode>,
}

impl World {
    fn new() -> Self {
        let temp = temp();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("workflows");
        for (name, text) in [("await", AWAIT), ("lint-await", LINT_THEN_AWAIT)] {
            let dir = profile.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(DEFINITION_FILE), text).unwrap();
            fs::write(dir.join("lint.py"), "print('ok')\n").unwrap();
        }
        let project_dir = root.join("work").join("app");
        let worktree = root.join("worktree-1");
        fs::create_dir_all(&worktree).unwrap();
        let mut file = TaskFile::new("Task 1", Kind::PrShip);
        file.id = "leaf-1".to_string();
        file.branch = Some("task/1".to_string());
        file.worktree = Some(worktree.to_string_lossy().into_owned());
        file.todos = vec![Todo {
            id: None,
            name: "implement".to_string(),
            role: None,
            workflow: None,
            state: TodoState::Open,
            started: None,
            extra: Default::default(),
        }];
        let leaf = task(project_dir.join("tasks").join("task-1"), file, Vec::new());
        let mut group_file = TaskFile::new("App", Kind::Other);
        group_file.id = "group-id".to_string();
        let tree = WorkTree {
            root: root.join("work"),
            projects: vec![Project {
                slug: "app".to_string(),
                file: None,
                task: task(project_dir, group_file, vec![leaf]),
            }],
            ..WorkTree::default()
        };
        let settings = WorkflowsSettings {
            enabled: true,
            min_free_disk_mb: 0,
            ..WorkflowsSettings::default()
        };
        let mut runner = FakeRunner::new();
        runner.no_sandbox = BTreeSet::from(["lint".to_string()]);
        runner.unsandboxed_mode = Mode::Ask;
        let worktrees = DirWorktrees {
            root: root.join("runs"),
        };
        let mut lifecycle = Lifecycle::new(runner, Box::new(worktrees), settings);
        lifecycle.set_wall_clock(Arc::new(|| {
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
        }));
        let path = path_var();
        let wait = Arc::new(
            WaitNode::new(Arc::new(FakeClock::default()), "gh")
                .with_probe(no_sandbox())
                .with_vars(vars(&[("PATH", &path)])),
        );
        let builtins = BuiltinNodes::default();
        builtins.insert("wait", wait.clone());
        lifecycle.runtime_mut().set_builtins(builtins);
        Self {
            _temp: temp,
            dirs: LayerDirs {
                profile: Some(profile),
                ..LayerDirs::default()
            },
            tree,
            lifecycle,
            wait,
        }
    }

    fn start(&mut self, workflow: &str) -> String {
        let context = StartContext {
            tree: &self.tree,
            active_run: None,
            checks: &Checks,
            definition_commit: None,
            definitions: None,
        };
        let request = StartRequest {
            workflow: Some(workflow.to_string()),
            ..StartRequest::new("leaf-1".to_string())
        };
        self.lifecycle.start(request, &self.dirs, &context).unwrap()
    }

    /// Ticks until nothing changes.
    fn settle(&mut self) {
        for _ in 0..50 {
            if !self.lifecycle.tick() {
                return;
            }
        }
    }

    fn asks(&self, run: &str) -> usize {
        self.lifecycle
            .runtime()
            .events(run)
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::ConsentAsked { .. }))
            .count()
    }

    fn pending(&self, run: &str) -> Option<Vec<String>> {
        let state = self.lifecycle.state(run).unwrap();
        state
            .consent_pending
            .map(|pending| pending.keys.into_iter().map(|key| key.node).collect())
    }
}

#[test]
fn a_no_sandbox_wait_in_an_answerable_run_asks_through_the_engine() {
    let mut world = World::new();
    let run = world.start("await");
    world.settle();
    assert_eq!(world.asks(&run), 1);
    assert_eq!(world.pending(&run), Some(vec!["hold".to_string()]));
    assert_eq!(world.lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    assert!(
        world.lifecycle.state(&run).unwrap().finished.is_empty(),
        "an ask is never a try"
    );

    // The answer starts the held wait without the sandbox, and the run ends.
    world
        .lifecycle
        .answer_unsandboxed(&run, true, BY_PERSON)
        .unwrap();
    world.settle();
    world.wait.wait_ended(1);
    world.settle();
    assert_eq!(world.lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(world.asks(&run), 1);
}

#[test]
fn a_grant_on_an_earlier_program_node_covers_a_later_wait() {
    let mut world = World::new();
    let run = world.start("lint-await");
    world.settle();
    assert_eq!(world.pending(&run), Some(vec!["lint".to_string()]));
    world
        .lifecycle
        .answer_unsandboxed(&run, true, BY_PERSON)
        .unwrap();
    world.settle();
    assert_eq!(
        world
            .lifecycle
            .runtime()
            .runner()
            .permits
            .last()
            .map(|(_, permit)| permit),
        Some(&person_grant())
    );
    // `lint` finishes; the wait starts under the same grant, with no second ask.
    world.lifecycle.runtime_mut().runner_mut().release_all();
    world.settle();
    assert_eq!(world.pending(&run), None);
    world.wait.wait_ended(1);
    world.settle();
    assert_eq!(world.lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(
        world.asks(&run),
        1,
        "one ask for the program node and the wait"
    );
}
