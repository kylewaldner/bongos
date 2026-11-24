//! Tests of the program runner: the state document, the permission checks of the shim (commands,
//! writes, network), the output, time and result caps, polling, the MCP refusal, the interpreter
//! check, the sandbox report and the decision when no sandbox is available (those tests need no
//! Python). Each other test skips when no Python 3.8+ is on `PATH`. The OS sandbox
//! is replaced by a probe that says it is unavailable, so the shim's checks are what is tested and
//! no test needs `bwrap`; one test uses the real probe and accepts either answer. Nothing touches
//! the real `HOME` or the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::io::Write as _;

use bongos_confine::safe_git::linked_git_dir_from_record;
use bongos_confine::{HostGit, RULE, TreeAccess, TreeGit};
use bongos_workflow_model::definition::Permissions;
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::state_doc::{self, Paths, RunIdentity};
use bongos_workflow_model::unsandboxed::{AllowedBy, Mode, Permit, REFUSED_TEXT};
use bongos_workflow_model::{Definition, Event, Failure, FailureReason, NodeKey, parse, replay};
use sandbox::{GitSandboxPolicy, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::runner_program::{
    Clock, DENIALS_END, DENIALS_FILE, INTERPRETER_FIX, Interpreter, MCP_REFUSAL, POLICY_FILE,
    PollPlan, ProgramCaps, ProgramNode, ProgramRunner, ProgramSettings, SNAPSHOT_NEEDS_SANDBOX,
    STATE_FILE, SUMMARY_FILE, SandboxProbe, WORK_DIR_NEEDS_SANDBOX, add_git_config,
    check_policy_outside_writable, os_sandbox_probe,
};
use crate::runtime::Finish;

const CHECK: &str = r#"
schema = 1
name = "check"
start = "triage"

[nodes.triage]
kind = "agent"
prompt = "triage.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] } }
routes = [{ to = "check" }]

[nodes.check]
kind = "program"
script = "check.py"
with = { greeting = "hi" }
permissions = { commands = ["true"] }
output = { status = { type = "choice", values = ["one-shot", "planned", "waiting", "done"] } }
polls = { value = "waiting", interval_minutes = 5, give_up_days = 7 }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn definition() -> Definition {
    parse(CHECK).unwrap()
}

fn base_env(home: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), home.to_string_lossy().into_owned());
    env.insert("LANG".into(), "C.UTF-8".into());
    env.insert("TMPDIR".into(), home.to_string_lossy().into_owned());
    env.insert("SECRET_TOKEN".into(), "do-not-pass".into());
    env
}

/// The interpreter, or `None` (the test skips) when no Python 3.8+ is installed.
fn python(home: &Path) -> Option<Interpreter> {
    match Interpreter::find(None, &base_env(home)) {
        Ok(interpreter) => Some(interpreter),
        Err(failure) => {
            eprintln!("skipped: {failure}");
            None
        }
    }
}

struct FakeClock(Mutex<Instant>);

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

pub(crate) const NO_SANDBOX: &str = "bwrap not found (test)";

pub(crate) fn no_sandbox() -> SandboxProbe {
    Arc::new(|_, _| Err(NO_SANDBOX.to_string()))
}

pub(crate) struct Fixture {
    root: TempDir,
    runner: ProgramRunner,
    clock: Arc<FakeClock>,
}

impl Fixture {
    fn new() -> Option<Self> {
        let root = TempDir::new().unwrap();
        let interpreter = python(root.path())?;
        let clock = FakeClock::new();
        let runner = ProgramRunner::new(interpreter, base_env(root.path()))
            .with_clock(clock.clone())
            .with_sandbox_probe(no_sandbox());
        Some(Self {
            root,
            runner,
            clock,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn task_worktree(&self) -> PathBuf {
        let path = self.path("task-worktree");
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The `check` node running `script`.
    pub(crate) fn node(&self, script: &str) -> ProgramNode {
        let definition = definition();
        let declared = definition.node("check").unwrap();
        let worktree = self.path("run-worktree");
        let scratch = self.path("scratch/check");
        std::fs::create_dir_all(&worktree).unwrap();
        let script_path = self.path("definitions/check.py");
        std::fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        std::fs::write(&script_path, script).unwrap();
        let triage = NodeKey::new("triage", 1);
        let state = replay(&[
            Event::Created,
            Event::NodeFinished {
                key: triage,
                try_number: 1,
                result: Some(json!({ "mode": "planned" })),
                failure: None,
                cost_usd: 0.5,
                turns: None,
                elapsed_ms: 0,
                session_id: None,
                confinement_refusals: Vec::new(),
            },
        ]);
        let state = state_doc::build(
            &[],
            &state,
            RunIdentity {
                id: "run-1".to_string(),
                workflow: "check".to_string(),
                todo: Some(TodoRef {
                    index: 0,
                    name: "check".to_string(),
                }),
                task_id: "task-1".to_string(),
                task_dir: self.path("task"),
                branch: "feature/check".to_string(),
                run_branch: "run/feature/check/1".to_string(),
            },
            &declared.with,
            Paths {
                worktree: worktree.clone(),
                scratch: scratch.clone(),
                trees: Default::default(),
            },
        );
        let settings = ProgramSettings::default();
        let try_dir = self.path("record/nodes/check/1-1");
        ProgramNode {
            script: script_path,
            output: declared.output.clone(),
            schema: declared.schema.clone(),
            permissions: declared.permissions.clone().unwrap_or_default(),
            caps: ProgramCaps::resolve(declared, &settings),
            polls: declared
                .polls
                .as_ref()
                .map(|polls| PollPlan::resolve(polls, &settings)),
            state,
            worktree,
            needs_sandbox: false,
            env: Vec::new(),
            scratch,
            declared_dirs: Vec::new(),
            tree_access: TreeAccess::Writable,
            tree_git: TreeGit::None,
            host_git: HostGit::default(),
            try_dir: try_dir.clone(),
            task_id: "task-1".to_string(),
            task_dir: self.path("task"),
            node_result: try_dir.join("node-result.json"),
            // These tests run the script, with the sandbox probe saying there is none: the
            // setting allows that. The decision itself is tested apart, from `start_with` on.
            permit: Permit::Allowed(AllowedBy::Setting),
            mode: Mode::Allow,
        }
    }

    fn start(&mut self, node: ProgramNode) {
        self.runner.start("run-1", &key(), 1, node).unwrap();
    }

    /// Polls until a finish comes (in real time; the fake clock does not move).
    fn wait(&mut self) -> Finish {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(finish) = self.runner.poll().pop() {
                return finish;
            }
            assert!(Instant::now() < deadline, "the program did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run(&mut self, script: &str) -> Finish {
        let node = self.node(script);
        self.run_node(node)
    }

    fn run_node(&mut self, node: ProgramNode) -> Finish {
        self.start(node);
        self.wait()
    }

    fn summary(&self, dir: &str) -> Value {
        let text = std::fs::read_to_string(self.path(dir).join(SUMMARY_FILE)).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

pub(crate) fn key() -> NodeKey {
    NodeKey::new("check", 1)
}

fn failure(finish: &Finish) -> &Failure {
    finish.outcome.as_ref().unwrap_err()
}

fn says(failure: &Failure) -> String {
    format!("{}: {}", failure.reason, failure.detail)
}

const TRY_DIR: &str = "record/nodes/check/1-1";

/// A script that writes `{"status": <status>}` to `$BONGOS_RESULT` after `body`.
fn script(body: &str, status: &str) -> String {
    format!(
        "import json, os, socket, subprocess, sys, tempfile, time\n{body}\n\
         with open(os.environ['BONGOS_RESULT'], 'w') as result:\n    \
         json.dump({{'status': {status}}}, result)\n"
    )
}

#[test]
fn reads_an_earlier_result_from_the_state_document_and_routes() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "state = json.load(open(os.environ['BONGOS_STATE']))\n\
         assert state['settings'] == {'greeting': 'hi'}, state['settings']\n\
         assert state['run']['id'] == 'run-1'\n\
         assert state['paths']['scratch'].endswith('scratch/check')\n\
         mode = state['results']['triage']['mode']",
        "mode",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "planned" })));
    assert_eq!(finish.cost_usd, 0.0);
    assert_eq!(finish.key, key());

    let state = fixture.path(TRY_DIR).join("state.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
    }
}

#[test]
fn a_denied_write_to_the_state_document_fails_the_node() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "try:\n    open(os.environ['BONGOS_STATE'], 'a')\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(failure.detail.ends_with("state.json"), "{}", failure.detail);
}

#[test]
fn an_allowed_command_runs() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "subprocess.run(['true'], check=True)\n\
         assert os.system('true') == 0\n\
         subprocess.run('true', shell=True, check=True)",
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
}

#[test]
fn an_unlisted_command_fails_even_when_the_script_catches_it() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "try:\n    subprocess.run(['curl', 'https://example.invalid'])\n\
         except PermissionError as error:\n    assert str(error) == 'permission denied: curl', error",
        "'done'",
    ));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(says(failure), "permission denied: curl");
    let denials = std::fs::read_to_string(fixture.path(TRY_DIR).join(DENIALS_FILE)).unwrap();
    assert_eq!(
        denials.lines().collect::<Vec<_>>(),
        [r#"{"what": "curl"}"#, DENIALS_END]
    );
}

#[test]
fn a_shell_line_with_an_unlisted_command_is_denied() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "try:\n    os.system('true; curl https://example.invalid')\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(failure(&finish).reason, FailureReason::PermissionDenied);
    assert!(failure(&finish).detail.starts_with("shell command: "));
}

#[test]
fn a_write_into_the_task_worktree_is_denied() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let task = fixture.task_worktree();
    let target = task.join("x.txt");
    let finish = fixture.run(&script(
        &format!(
            "open('own.txt', 'w').write('mine')\n\
             with tempfile.NamedTemporaryFile(delete=False) as scratch:\n    scratch.write(b'x')\n\
             try:\n    open({target:?}, 'w').write('theirs')\nexcept PermissionError:\n    pass",
            target = target.to_string_lossy()
        ),
        "'done'",
    ));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.starts_with(&format!("{RULE}: ")),
        "{}",
        failure.detail
    );
    assert!(
        failure.detail.contains("task-worktree"),
        "{}",
        failure.detail
    );
    assert!(!target.exists());
    assert!(fixture.path("run-worktree/own.txt").exists());
}

#[test]
fn network_off_denies_socket_connect() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "try:\n    socket.create_connection(('127.0.0.1', 9), timeout=1)\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(failure.detail, "network 127.0.0.1:9");
}

#[test]
fn the_output_cap_names_itself() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node(&script(
        "sys.stdout.write('x' * 100000)\nsys.stdout.flush()\ntime.sleep(30)",
        "'done'",
    ));
    node.caps.max_output_bytes = 1000;
    let started = Instant::now();
    let finish = fixture.run_node(node);
    assert!(started.elapsed() < Duration::from_secs(10));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::OutputCapHit);
    assert!(says(failure).starts_with("output cap hit: "));
    assert!(failure.detail.contains("1000 bytes"), "{}", failure.detail);
    let kept = std::fs::metadata(fixture.path(TRY_DIR).join("stdout.log"))
        .unwrap()
        .len();
    assert!(kept <= 1000, "kept {kept} bytes");
    assert_eq!(
        crate::runner_program::classify(
            &key(),
            &fixture.node("pass"),
            crate::runner_program::Exit::Code(0),
            true,
            &crate::runner_program::DenialLog::ended(Vec::new()),
            crate::runner_program::ResultFile::Missing,
            "",
        )
        .unwrap_err()
        .detail,
        "the output passed 10 MB (workflows.programs.max_output_mb); the rest was dropped"
    );
}

#[test]
fn the_time_cap_names_itself_and_stops_the_process() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node(&script("time.sleep(60)", "'done'"));
    node.caps.max_time = Duration::from_secs(60);
    fixture.start(node);
    assert!(fixture.runner.poll().is_empty());
    fixture.clock.advance(Duration::from_secs(61));
    let started = Instant::now();
    let finishes = fixture.runner.poll();
    assert!(started.elapsed() < Duration::from_secs(5));
    let failure = failure(&finishes[0]);
    assert_eq!(failure.reason, FailureReason::TimeCapHit);
    assert_eq!(
        failure.detail,
        "stopped at the time cap of 1 min (max_minutes)"
    );
    assert!(fixture.runner.in_flight().is_empty());
}

#[test]
fn halt_stops_the_process_within_five_seconds() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let node = fixture.node(&script(
        "import signal\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\ntime.sleep(60)",
        "'done'",
    ));
    fixture.start(node);
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    fixture.runner.stop(&[("run-1".to_string(), key())]);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(fixture.runner.in_flight().is_empty());
    assert!(fixture.runner.poll().is_empty());
}

#[test]
fn an_invalid_result_fails_program_result_invalid() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script("", "'nope'"));
    assert_eq!(failure(&finish).reason, FailureReason::ProgramResultInvalid);

    let finish = fixture.run("print('no result')\n");
    assert_eq!(
        says(failure(&finish)),
        "program result invalid: the script wrote no result to $BONGOS_RESULT"
    );

    let finish =
        fixture.run("import os\nopen(os.environ['BONGOS_RESULT'], 'w').write('not json')\n");
    assert_eq!(failure(&finish).reason, FailureReason::ProgramResultInvalid);
    assert!(
        failure(&finish)
            .detail
            .starts_with("the result is not JSON")
    );

    let finish =
        fixture.run("import os\nopen(os.environ['BONGOS_RESULT'], 'w').write(' ' * 300000)\n");
    assert!(
        failure(&finish).detail.contains("over the cap of"),
        "{}",
        failure(&finish).detail
    );

    let finish = fixture.run("import sys\nsys.stderr.write('broken\\n')\nsys.exit(3)\n");
    assert_eq!(
        says(failure(&finish)),
        "program exited nonzero: exit code 3: broken"
    );
}

#[test]
fn waiting_reruns_after_the_interval_with_no_cost() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "count = os.path.join(os.environ['BONGOS_WORKTREE'], 'count')\n\
         n = int(open(count).read()) + 1 if os.path.exists(count) else 1\n\
         open(count, 'w').write(str(n))",
        "'waiting' if n < 3 else 'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "waiting" })));
    assert_eq!(finish.cost_usd, 0.0);
    assert!(fixture.runner.in_flight().is_empty());
    assert_eq!(
        fixture.runner.sleeping(),
        vec![("run-1".to_string(), key())]
    );

    fixture.clock.advance(Duration::from_secs(4 * 60));
    assert!(fixture.runner.poll().is_empty());
    assert!(fixture.runner.in_flight().is_empty());

    fixture.clock.advance(Duration::from_secs(60));
    assert!(fixture.runner.poll().is_empty());
    assert_eq!(fixture.runner.in_flight().len(), 1);
    assert_eq!(fixture.wait().outcome, Ok(json!({ "status": "waiting" })));

    fixture.clock.advance(Duration::from_secs(5 * 60));
    assert!(fixture.runner.poll().is_empty());
    let finish = fixture.wait();
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert_eq!(finish.cost_usd, 0.0);
    assert!(fixture.runner.sleeping().is_empty());
    assert!(fixture.path(TRY_DIR).join("polls/2/summary.json").exists());
    assert!(fixture.path(TRY_DIR).join("polls/3/summary.json").exists());
}

#[test]
fn waiting_past_the_give_up_time_fails_wait_timed_out() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script("", "'waiting'"));
    assert_eq!(finish.outcome, Ok(json!({ "status": "waiting" })));

    fixture.clock.advance(Duration::from_secs(7 * 24 * 60 * 60));
    let finishes = fixture.runner.poll();
    assert_eq!(finishes.len(), 1);
    assert_eq!(
        says(failure(&finishes[0])),
        "wait timed out: still waiting after 7 days (give_up_days)"
    );
    assert!(fixture.runner.in_flight().is_empty());
    assert!(fixture.runner.sleeping().is_empty());
}

#[test]
fn an_unavailable_sandbox_is_reported_in_the_summary() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script("", "'done'"));
    assert!(finish.outcome.is_ok());
    let summary = fixture.summary(TRY_DIR);
    assert_eq!(summary["sandbox"], "unsandboxed (allowed by setting)");
    assert_eq!(summary["exit"], 0);
}

#[test]
fn the_real_sandbox_probe_runs_either_way() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    // Bubblewrap hides the host's /tmp behind its own, so a fixture there cannot be sandboxed.
    if fixture.root.path().starts_with("/tmp") {
        return;
    }
    let interpreter = fixture.runner.interpreter().clone();
    fixture.runner = ProgramRunner::new(interpreter, base_env(fixture.root.path()))
        .with_clock(fixture.clock.clone())
        .with_sandbox_probe(os_sandbox_probe());
    let finish = fixture.run(&script(
        "subprocess.run(['true'], check=True)\nopen('own.txt', 'w').write('mine')\n\
         try:\n    subprocess.run(['curl', 'https://example.invalid'])\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    let summary = fixture.summary(TRY_DIR);
    let note = summary["sandbox"].as_str().unwrap().to_string();
    assert!(
        note == "sandboxed" || note.starts_with("unsandboxed"),
        "{note}"
    );
    // The denial reaches bongos from inside the sandbox too.
    assert_eq!(says(failure(&finish)), "permission denied: curl", "{note}");
}

#[test]
fn mcp_permissions_are_refused() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node(&script("", "'done'"));
    node.permissions = Permissions {
        mcp: vec!["github".to_string()],
        ..Permissions::default()
    };
    let refused = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
    assert_eq!(refused.reason, FailureReason::PermissionDenied);
    assert_eq!(refused.detail, MCP_REFUSAL);
}

#[test]
fn a_missing_interpreter_refuses_the_start() {
    let root = TempDir::new().unwrap();
    let failure =
        Interpreter::find(Some("/nonexistent/python3"), &base_env(root.path())).unwrap_err();
    assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
    assert_eq!(
        failure.to_string(),
        format!("program interpreter missing: {INTERPRETER_FIX}")
    );
    assert_eq!(
        failure.to_string(),
        "program interpreter missing: install Python 3.8+ or set workflows.programs.python"
    );
}

#[test]
fn the_environment_is_clean() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "open(os.path.join(os.environ['BONGOS_WORKTREE'], 'env.json'), 'w')\
         .write(json.dumps(dict(os.environ)))",
        "'done'",
    ));
    assert!(finish.outcome.is_ok(), "{:?}", finish.outcome);
    let env: BTreeMap<String, String> = serde_json::from_str(
        &std::fs::read_to_string(fixture.path("run-worktree/env.json")).unwrap(),
    )
    .unwrap();
    let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
    names.retain(|name| *name != "LC_CTYPE");
    assert_eq!(
        names,
        [
            "BONGOS_NODE_RESULT",
            "BONGOS_RESULT",
            "BONGOS_RUN_ID",
            "BONGOS_RUN_NODE",
            "BONGOS_STATE",
            "BONGOS_TASK_DIR",
            "BONGOS_TASK_ID",
            "BONGOS_WORKTREE",
            "HOME",
            "LANG",
            "PATH",
            "TMPDIR",
        ]
    );
    assert!(env["TMPDIR"].ends_with("scratch/check/tmp"));
    assert!(env["BONGOS_RESULT"].ends_with("scratch/check/result.json"));
}

#[test]
fn a_copied_binary_named_like_a_listed_command_is_denied() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let copy = "shutil.copy(shutil.which('id') or '/usr/bin/id', 'true')\n";
    let finish = fixture.run(&script(
        &format!(
            "import shutil\n{copy}\
             try:\n    subprocess.run(['./true'])\nexcept PermissionError:\n    pass"
        ),
        "'done'",
    ));
    assert_eq!(says(failure(&finish)), "permission denied: true");

    let finish = fixture.run(&script(
        "import shutil\n\
         env = dict(os.environ, PATH=os.getcwd() + ':' + os.environ['PATH'])\n\
         try:\n    subprocess.run(['true'], env=env)\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(
        says(failure(&finish)),
        "permission denied: command from a writable PATH entry: true"
    );

    let finish = fixture.run(&script(
        "import shutil\n\
         try:\n    subprocess.run(['id'], executable=shutil.which('true'))\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(failure(&finish).reason, FailureReason::PermissionDenied);
}

#[test]
fn a_write_through_a_directory_descriptor_is_checked() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "import shutil\n\
         os.makedirs('tree/sub')\nopen('tree/sub/f', 'w').close()\nshutil.rmtree('tree')",
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));

    let finish = fixture.run(&script(
        "fd = os.open('/', os.O_RDONLY)\n\
         try:\n    os.remove('bongos-no-such-file', dir_fd=fd)\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(
        says(failure(&finish)),
        format!("permission denied: {RULE}: /bongos-no-such-file")
    );

    let finish = fixture.run(&script(
        "fd = os.open('/', os.O_RDONLY)\n\
         try:\n    os.open('bongos-x', os.O_WRONLY | os.O_CREAT, dir_fd=fd)\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(
        says(failure(&finish)),
        format!("permission denied: {RULE}: relative to a directory descriptor: bongos-x")
    );
}

#[test]
fn reaching_the_hook_state_is_denied() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    for (body, what) in [
        ("import gc\ngc.get_objects()", "gc.get_objects"),
        ("import ctypes", "import ctypes"),
        ("sys.settrace(lambda *args: None)", "sys.settrace"),
    ] {
        let finish = fixture.run(&script(
            &format!(
                "try:\n    {}\nexcept PermissionError:\n    pass",
                body.replace('\n', "\n    ")
            ),
            "'done'",
        ));
        assert_eq!(says(failure(&finish)), format!("permission denied: {what}"));
    }
    // An uncaught error is printed without tripping the frame checks.
    let finish = fixture
        .run("import collections\ncollections.namedtuple('A', 'a')\nraise ValueError('boom')\n");
    assert_eq!(
        says(failure(&finish)),
        "program exited nonzero: exit code 1: ValueError: boom"
    );
}

#[test]
fn a_fifo_result_fails_without_hanging() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let started = Instant::now();
    let finish = fixture.run("import os\nos.mkfifo(os.environ['BONGOS_RESULT'])\n");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        says(failure(&finish)),
        "program result invalid: $BONGOS_RESULT is not a regular file"
    );

    let finish = fixture.run("import os\nos.symlink('/dev/zero', os.environ['BONGOS_RESULT'])\n");
    assert_eq!(
        says(failure(&finish)),
        "program result invalid: $BONGOS_RESULT is not a regular file"
    );
}

#[test]
fn a_lost_denial_log_fails_the_node() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    for body in ["os.closerange(3, 4096)", "sys.stdout.flush()\nos._exit(0)"] {
        let finish = fixture.run(&format!(
            "import json, os, sys\n\
             with open(os.environ['BONGOS_RESULT'], 'w') as result:\n    \
             json.dump({{'status': 'done'}}, result)\n{body}\n"
        ));
        let failure = failure(&finish);
        assert_eq!(
            failure.reason,
            FailureReason::ProgramResultInvalid,
            "{body}"
        );
        assert!(
            failure
                .detail
                .starts_with("the shim's denial log ended early"),
            "{}",
            failure.detail
        );
    }
}

#[test]
fn a_shell_runs_only_as_dash_c_with_a_clean_environment() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let marker = fixture.path("run-worktree/ran");
    for (body, what) in [
        (
            "subprocess.run(['sh', 'a.sh', '-c', 'true'])",
            "permission denied: sh",
        ),
        (
            "subprocess.run(['sh', '--rcfile', 'a.sh', '-i', '-c', 'true'])",
            "permission denied: sh",
        ),
        (
            "subprocess.run(['sh', '-c', 'true'], env={'PATH': os.environ['PATH'], 'BASH_ENV': 'a.sh'})",
            "permission denied: a program started with BASH_ENV set",
        ),
        (
            "subprocess.run(['true'], env={'PATH': os.environ['PATH'], 'LD_PRELOAD': 'x.so'})",
            "permission denied: a program started with LD_PRELOAD set",
        ),
        (
            "os.environ['BASH_ENV'] = 'a.sh'",
            "permission denied: setting BASH_ENV",
        ),
    ] {
        let finish = fixture.run(&script(
            &format!(
                "open('a.sh', 'w').write('touch ran\\n')\n\
                 try:\n    {body}\nexcept PermissionError:\n    pass"
            ),
            "'done'",
        ));
        assert_eq!(says(failure(&finish)), what, "{body}");
        assert!(!marker.exists(), "{body}");
    }
}

#[test]
fn every_way_to_fork_exec_is_checked() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "import multiprocessing.util, shutil\n\
         import _posixsubprocess\n\
         assert sys.modules['_posixsubprocess'] is _posixsubprocess\n\
         assert getattr(subprocess, '_fork_exec', _posixsubprocess.fork_exec) is _posixsubprocess.fork_exec\n\
         subprocess.run(['true'], cwd=os.getcwd(), check=True)\n\
         try:\n    multiprocessing.util.spawnv_passfds(shutil.which('id'), ['id'], ())\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(says(failure(&finish)), "permission denied: id");
}

#[test]
fn an_object_whose_class_runs_code_is_refused_unread() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "class Sneaky:\n    @property\n    def __class__(self):\n        \
         open('ran', 'w').write('x')\n        return str\n\
         try:\n    subprocess.Popen([Sneaky()])\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(
        says(failure(&finish)),
        "permission denied: a program bongos cannot check"
    );
    assert!(!fixture.path("run-worktree/ran").exists());
}

#[test]
fn fifos_and_changes_through_read_only_descriptors_are_checked() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(target_os = "linux")) {
        return;
    }
    let task = fixture.task_worktree();
    let target = task.join("target");
    std::fs::write(&target, "x").unwrap();
    let task = task.to_string_lossy().into_owned();
    let finish = fixture.run(&script(
        &format!(
            "os.mkfifo('own-fifo')\n\
             try:\n    os.mkfifo({fifo:?})\nexcept PermissionError:\n    pass",
            fifo = format!("{task}/fifo"),
        ),
        "'done'",
    ));
    let detail = says(failure(&finish));
    assert!(
        detail.starts_with(&format!("permission denied: {RULE}: ")),
        "{detail}"
    );
    assert!(detail.ends_with("task-worktree/fifo"), "{detail}");
    assert!(!Path::new(&task).join("fifo").exists());
    assert!(fixture.path("run-worktree/own-fifo").exists());

    let finish = fixture.run(&script(
        &format!(
            "fd = os.open({target:?}, os.O_RDONLY)\n\
             try:\n    os.fchmod(fd, 0o777)\nexcept PermissionError:\n    pass",
            target = target.to_string_lossy(),
        ),
        "'done'",
    ));
    let detail = says(failure(&finish));
    assert!(
        detail.starts_with(&format!("permission denied: {RULE}: ")),
        "{detail}"
    );
    assert!(detail.ends_with("task-worktree/target"), "{detail}");
}

/// Runs host git in `dir` with a clean configuration; its output, or `None` when it failed.
#[allow(
    clippy::disallowed_methods,
    reason = "a test fixture runs git to make a real repository"
)]
fn git(home: &Path, dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A real repository `repo` (the main worktree) with the linked worktrees `run-worktree` (the
/// node's) and `other-worktree`: its common git directory, or `None` (the test skips) when git
/// is not installed.
fn git_repo(fixture: &Fixture) -> Option<PathBuf> {
    let home = fixture.root.path();
    let repo = fixture.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if git(home, &repo, &["init", "-q", "-b", "main"]).is_none() {
        eprintln!("skipped: git is not installed");
        return None;
    }
    git(
        home,
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "start"],
    )
    .unwrap();
    for name in ["run", "other"] {
        let path = fixture.path(&format!("{name}-worktree"));
        git(
            home,
            &repo,
            &["worktree", "add", "-q", "-b", name, &path.to_string_lossy()],
        )
        .unwrap();
    }
    Some(repo.join(".git"))
}

/// The git metadata no script may write: the common git directory's own files, another
/// worktree's git directory and the node's own pointers.
fn git_metadata_targets(fixture: &Fixture, common: &Path) -> Vec<PathBuf> {
    let own = common.join("worktrees/run-worktree");
    let other = common.join("worktrees/other-worktree");
    vec![
        common.join("commondir"),
        other.join("commondir"),
        other.join("config.worktree"),
        common.join("hooks/pre-commit"),
        common.join("config"),
        own.join("commondir"),
        own.join("config.worktree"),
        fixture.path("run-worktree/.git"),
    ]
}

#[test]
fn the_shim_refuses_git_metadata_outside_the_writable_roots() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    for target in git_metadata_targets(&fixture, &common) {
        let before = std::fs::read(&target).ok();
        let mut node = fixture.node(&script(
            &format!(
                "try:\n    open({target:?}, 'a').write('x')\nexcept PermissionError:\n    pass",
                target = target.to_string_lossy()
            ),
            "'done'",
        ));
        node.host_git.git_dirs = vec![common.clone()];
        let finish = fixture.run_node(node);
        let detail = says(failure(&finish));
        let tail = target.strip_prefix(fixture.root.path()).unwrap();
        assert!(
            detail.starts_with(&format!("permission denied: {RULE}: ")),
            "{detail}"
        );
        assert!(detail.ends_with(&*tail.to_string_lossy()), "{detail}");
        assert_eq!(std::fs::read(&target).ok(), before, "{}", target.display());
    }
}

/// The message the shim gives for a write to `path` (already canonical).
fn refusal(path: &Path) -> String {
    format!("permission denied: {RULE}: {}", path.display())
}

#[test]
fn a_write_outside_the_roots_names_the_rule() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = std::fs::canonicalize(&outside).unwrap().join("x.txt");
    let finish = fixture.run(&script(
        &format!(
            "try:\n    open({target:?}, 'w').write('theirs')\n\
             except PermissionError as error:\n    \
             assert str(error) == {message:?}, error",
            target = target.to_string_lossy(),
            message = refusal(&target),
        ),
        "'done'",
    ));
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(says(failure), refusal(&target));
    assert!(!target.exists());
    let denials = std::fs::read_to_string(fixture.path(TRY_DIR).join(DENIALS_FILE)).unwrap();
    let lines: Vec<&str> = denials.lines().collect();
    assert_eq!(lines.len(), 2, "{denials}");
    assert_eq!(
        serde_json::from_str::<Value>(lines[0]).unwrap(),
        json!({ "what": format!("{RULE}: {}", target.display()) })
    );
    assert_eq!(lines[1], DENIALS_END);
}

#[test]
fn a_caught_denial_still_fails_the_node() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let target = fixture.path("outside.txt");
    let finish = fixture.run(&script(
        &format!(
            "try:\n    open({target:?}, 'w')\nexcept PermissionError:\n    pass\n\
             open('own.txt', 'w').write('mine')",
            target = target.to_string_lossy()
        ),
        "'done'",
    ));
    // The script carried on and wrote its result, and the node failed all the same.
    assert_eq!(failure(&finish).reason, FailureReason::PermissionDenied);
    assert!(failure(&finish).detail.starts_with(&format!("{RULE}: ")));
    assert_eq!(
        std::fs::read_to_string(fixture.path("run-worktree/own.txt")).unwrap(),
        "mine"
    );
    assert!(!target.exists());
}

#[test]
fn an_empty_mask_list_is_valid() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script(
        "os.makedirs('sub/deep')\nopen('sub/deep/f', 'w').write('mine')",
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    let policy = std::fs::read_to_string(fixture.path(TRY_DIR).join(POLICY_FILE)).unwrap();
    let policy: Value = serde_json::from_str(&policy).unwrap();
    assert_eq!(policy["masks"], json!([]));
    assert!(
        policy["writable"]
            .as_array()
            .is_some_and(|roots| !roots.is_empty())
    );
    assert!(policy.get("git").is_none() && policy.get("protected").is_none());
}

#[test]
fn a_write_to_a_mask_names_the_rule() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let pointer = std::fs::canonicalize(fixture.path("run-worktree"))
        .unwrap()
        .join(".git");
    let before = std::fs::read(&pointer).unwrap();
    let node = private_node(
        &fixture,
        &common,
        &script(
            "open('own.txt', 'w').write('mine')\n\
             try:\n    open('.git', 'a').write('x')\nexcept PermissionError:\n    pass",
            "'done'",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    assert_eq!(says(failure(&finish)), refusal(&pointer));
    assert_eq!(std::fs::read(&pointer).unwrap(), before);
    let policy = std::fs::read_to_string(fixture.path(TRY_DIR).join(POLICY_FILE)).unwrap();
    let policy: Value = serde_json::from_str(&policy).unwrap();
    assert!(
        policy["masks"]
            .as_array()
            .unwrap()
            .contains(&json!(pointer.to_string_lossy())),
        "{policy}"
    );
    // Inside the roots and outside the mask, a write is fine.
    assert_eq!(
        std::fs::read_to_string(fixture.path("run-worktree/own.txt")).unwrap(),
        "mine"
    );
}

#[test]
fn a_rename_and_a_hard_link_out_of_the_roots_are_denied() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = std::fs::canonicalize(&outside).unwrap();
    // A rename and a link inside the roots are allowed.
    let finish = fixture.run(&script(
        "open('src.txt', 'w').write('mine')\n\
         os.link('src.txt', 'linked.txt')\nos.rename('linked.txt', 'moved.txt')",
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    std::fs::remove_file(fixture.path("run-worktree/moved.txt")).unwrap();
    let source = std::fs::canonicalize(fixture.path("run-worktree/src.txt")).unwrap();

    for (call, name) in [("rename", "moved.txt"), ("link", "linked.txt")] {
        let target = outside.join(name);
        let finish = fixture.run(&script(
            &format!(
                "open('src.txt', 'w').write('mine')\n\
                 try:\n    os.{call}('src.txt', {target:?})\nexcept PermissionError:\n    pass",
                target = target.to_string_lossy(),
            ),
            "'done'",
        ));
        assert_eq!(says(failure(&finish)), refusal(&target), "{call}");
        assert!(!target.exists(), "{call}");
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "mine");

    // The source of a rename is checked too: nothing is taken out of a read-only place.
    let held = outside.join("held.txt");
    std::fs::write(&held, "theirs").unwrap();
    let finish = fixture.run(&script(
        &format!(
            "try:\n    os.rename({held:?}, 'taken.txt')\nexcept PermissionError:\n    pass",
            held = held.to_string_lossy(),
        ),
        "'done'",
    ));
    assert_eq!(says(failure(&finish)), refusal(&held));
    assert_eq!(std::fs::read_to_string(&held).unwrap(), "theirs");
}

#[test]
fn a_write_through_a_symlink_is_checked_where_it_leads() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = std::fs::canonicalize(&outside).unwrap();
    let target = outside.join("x.txt");
    // Making the symlink is allowed inside the roots; writing through it is not.
    let finish = fixture.run(&script(
        &format!(
            "os.symlink({dir:?}, 'to-outside')\n\
             try:\n    open('to-outside/x.txt', 'w').write('theirs')\nexcept PermissionError:\n    pass",
            dir = outside.to_string_lossy(),
        ),
        "'done'",
    ));
    assert_eq!(says(failure(&finish)), refusal(&target));
    assert!(!target.exists());
    assert!(std::fs::symlink_metadata(fixture.path("run-worktree/to-outside")).is_ok());
}

#[test]
fn a_symlink_leading_outside_can_be_removed_and_renamed_but_not_written_through() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = std::fs::canonicalize(&outside).unwrap();
    let held = outside.join("held.txt");
    std::fs::write(&held, "theirs").unwrap();
    // The links themselves are inside the writable root, so removing or renaming them is fine,
    // whatever they lead to; a directory of links goes with shutil.rmtree.
    let finish = fixture.run(&script(
        &format!(
            "import shutil\nos.symlink({held:?}, 'file-link')\nos.symlink({held:?}, 'other-link')\n\
             os.symlink({dir:?}, 'dir-link')\n\
             os.rename('file-link', 'renamed-link')\nos.remove('renamed-link')\n\
             os.unlink('dir-link')\n\
             os.makedirs('venv/bin')\nos.symlink({held:?}, 'venv/bin/python')\n\
             shutil.rmtree('venv')\n\
             os.rename('other-link', 'final-link')",
            held = held.to_string_lossy(),
            dir = outside.to_string_lossy(),
        ),
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    let tree = fixture.path("run-worktree");
    for gone in [
        "file-link",
        "renamed-link",
        "dir-link",
        "venv",
        "other-link",
    ] {
        assert!(
            std::fs::symlink_metadata(tree.join(gone)).is_err(),
            "{gone}"
        );
    }
    assert!(std::fs::symlink_metadata(tree.join("final-link")).is_ok());
    assert_eq!(std::fs::read_to_string(&held).unwrap(), "theirs");

    // Writing through the link is still refused.
    let finish = fixture.run(&script(
        &format!(
            "os.symlink({held:?}, 'through')\n\
             try:\n    open('through', 'w').write('mine')\nexcept PermissionError:\n    pass",
            held = held.to_string_lossy(),
        ),
        "'done'",
    ));
    assert_eq!(says(failure(&finish)), refusal(&held));
    assert_eq!(std::fs::read_to_string(&held).unwrap(), "theirs");
}

#[test]
fn a_directory_holding_a_mask_cannot_be_renamed_or_removed() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let tree = std::fs::canonicalize(fixture.path("run-worktree")).unwrap();
    let moved = tree.with_file_name("run-worktree-moved");
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "try:\n    os.rename({tree:?}, {moved:?})\nexcept PermissionError:\n    pass",
                tree = tree.to_string_lossy(),
                moved = moved.to_string_lossy(),
            ),
            "'done'",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    assert_eq!(says(failure(&finish)), refusal(&tree));
    assert!(tree.join(".git").exists());
    assert!(!moved.exists());
}

#[test]
fn git_config_from_the_environment_is_appended_to() {
    let mut env = BTreeMap::new();
    add_git_config(&mut env, "core.logAllRefUpdates", "false");
    let expected: BTreeMap<String, String> = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "core.logAllRefUpdates"),
        ("GIT_CONFIG_VALUE_0", "false"),
    ]
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .into();
    assert_eq!(env, expected);

    let mut env: BTreeMap<String, String> = [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "safe.directory"),
        ("GIT_CONFIG_VALUE_0", "*"),
        ("GIT_CONFIG_KEY_1", "user.name"),
        ("GIT_CONFIG_VALUE_1", "someone"),
    ]
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .into();
    add_git_config(&mut env, "core.logAllRefUpdates", "false");
    assert_eq!(env["GIT_CONFIG_COUNT"], "3");
    assert_eq!(env["GIT_CONFIG_KEY_0"], "safe.directory");
    assert_eq!(env["GIT_CONFIG_VALUE_1"], "someone");
    assert_eq!(env["GIT_CONFIG_KEY_2"], "core.logAllRefUpdates");
    assert_eq!(env["GIT_CONFIG_VALUE_2"], "false");
}

#[test]
fn a_symlink_in_the_worktree_or_the_scratch_directory_is_allowed() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let inside = fixture.path("run-worktree/inside.txt");
    std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
    std::fs::write(&inside, "kept\n").unwrap();
    // `python -m venv` makes symlinks; writing through one inside the writable roots is fine.
    let finish = fixture.run(&script(
        "os.symlink('inside.txt', 'link-in-worktree')\n\
         open('link-in-worktree', 'a').write('more\\n')\n\
         os.symlink(sys.executable, os.path.join(os.environ['TMPDIR'], 'python'))",
        "'done'",
    ));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert!(
        std::fs::symlink_metadata(fixture.path("run-worktree/link-in-worktree"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "kept\nmore\n");
}

/// The git commands a script runs to commit, with an identity and no user configuration.
const SCRIPT_GIT: &str =
    "git = ['git', '-c', 'user.name=test', '-c', 'user.email=test@example.invalid']";

/// `fixture`'s node running `script` in `run-worktree`, a linked worktree of the repository
/// whose common git directory is `common`, with private git, the host's record of its git
/// directories and `git` (with `extra`) listed.
fn private_node(fixture: &Fixture, common: &Path, script: &str, extra: &[&str]) -> ProgramNode {
    let mut node = fixture.node(script);
    let linked = linked_git_dir_from_record(&node.worktree, common).unwrap();
    node.tree_git = TreeGit::Private;
    node.host_git = HostGit {
        git_dirs: vec![common.to_path_buf(), linked],
        records_dir: Some(fixture.path("record")),
    };
    std::fs::create_dir_all(fixture.path("record")).unwrap();
    node.permissions = Permissions {
        commands: std::iter::once("git")
            .chain(extra.iter().copied())
            .map(String::from)
            .collect(),
        ..Permissions::default()
    };
    node
}

/// The subject of the tip of `branch` in `fixture`'s repository, or `None` when it has none.
fn tip_subject(fixture: &Fixture, branch: &str) -> Option<String> {
    git(
        fixture.root.path(),
        &fixture.path("repo"),
        &["log", "-1", "--format=%s", branch, "--"],
    )
}

/// Puts `fixture`'s runner on the real OS sandbox; `false` (the test skips) when the sandbox
/// cannot be created here.
fn use_real_sandbox(fixture: &mut Fixture) -> bool {
    // Bubblewrap hides the host's /tmp behind its own, so a fixture there cannot be sandboxed.
    if fixture.root.path().starts_with("/tmp") {
        eprintln!("skipped: the fixture is under /tmp");
        return false;
    }
    let policy = SandboxPolicy {
        fs: SandboxFsPolicy::Restricted {
            writable_paths: vec![fixture.root.path().to_path_buf()],
        },
        network: SandboxNetPolicy::Blocked,
        git: GitSandboxPolicy::default(),
        read_only_masks: Vec::new(),
    };
    if let Err(reason) = (os_sandbox_probe())(&policy, fixture.root.path()) {
        eprintln!("skipped: {reason}");
        return false;
    }
    let interpreter = fixture.runner.interpreter().clone();
    fixture.runner = ProgramRunner::new(interpreter, base_env(fixture.root.path()))
        .with_clock(fixture.clock.clone())
        .with_sandbox_probe(os_sandbox_probe());
    true
}

#[test]
fn a_commit_without_the_sandbox_reaches_the_shared_branch() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let dot_git = std::fs::read(fixture.path("run-worktree/.git")).unwrap();
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)\n\
                 branch = subprocess.run(['git', 'branch', '--show-current'], check=True, \
                 capture_output=True, text=True).stdout.strip()"
            ),
            "'done' if branch == 'run' else branch",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    let summary = fixture.summary(TRY_DIR);
    // The fixture's setting allows a run without the sandbox, and the note says so.
    assert_eq!(summary["sandbox"], "unsandboxed (allowed by setting)");
    assert_eq!(summary["git_imported"], json!(["refs/heads/run"]));
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("inside"));
    // The worktree is back on shared git and the private directory is gone.
    assert_eq!(
        std::fs::read(fixture.path("run-worktree/.git")).unwrap(),
        dot_git
    );
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
    assert_eq!(tip_subject(&fixture, "other").as_deref(), Some("start"));
}

#[test]
fn a_write_to_the_common_git_directory_fails_the_node_with_the_rule() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    if !use_real_sandbox(&mut fixture) {
        return;
    }
    let declared = fixture.path("declared");
    let undeclared = fixture.path("undeclared");
    std::fs::create_dir_all(&declared).unwrap();
    std::fs::create_dir_all(&undeclared).unwrap();
    let refused = [
        common.join("config"),
        common.join("refs/heads/planted"),
        common.join("hooks/pre-commit"),
        common.join("worktrees/other-worktree/HEAD"),
        undeclared.join("x.txt"),
    ];
    let linked_ref = common.join("refs/heads/linked");
    let before: Vec<_> = refused
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect();
    let names: Vec<String> = refused
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    // `cp` and `ln` are listed, so their writes pass the shim: only the OS sandbox stops them.
    let mut node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 open('evil', 'w').write('evil\\n')\n\
                 for target in {names:?}:\n    \
                 copied = subprocess.run(['cp', 'evil', target])\n    \
                 assert copied.returncode != 0, target\n\
                 linked = subprocess.run(['ln', '-s', 'evil', {linked:?}])\n\
                 assert linked.returncode != 0\n\
                 subprocess.run(['cp', 'evil', {allowed:?}], check=True)\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)",
                linked = linked_ref.to_string_lossy(),
                allowed = declared.join("x.txt").to_string_lossy(),
            ),
            "'done'",
        ),
        &["cp", "ln"],
    );
    node.declared_dirs = vec![declared.clone()];
    let finish = fixture.run_node(node);
    let stderr =
        std::fs::read_to_string(fixture.path(TRY_DIR).join("stderr.log")).unwrap_or_default();
    assert_eq!(fixture.summary(TRY_DIR)["sandbox"], "sandboxed");
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied, "{stderr}");
    assert_eq!(
        failure.detail,
        format!("bongos: {RULE}: {}", refused[0].display()),
        "{stderr}"
    );
    let after: Vec<_> = refused
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect();
    assert_eq!(after, before);
    assert!(std::fs::symlink_metadata(&linked_ref).is_err());
    assert_eq!(
        std::fs::read_to_string(declared.join("x.txt")).unwrap(),
        "evil\n"
    );
    // The commit made in the sandbox still reaches the shared branch.
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("inside"));
}

#[test]
fn declared_directories_are_writable_and_undeclared_ones_are_not() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let declared = fixture.path("declared");
    let undeclared = fixture.path("undeclared");
    std::fs::create_dir_all(&declared).unwrap();
    std::fs::create_dir_all(&undeclared).unwrap();
    let mut node = fixture.node(&script(
        &format!(
            "open({allowed:?}, 'w').write('mine')\n\
             try:\n    open({refused:?}, 'w').write('theirs')\nexcept PermissionError:\n    pass",
            allowed = declared.join("x.txt").to_string_lossy(),
            refused = undeclared.join("x.txt").to_string_lossy(),
        ),
        "'done'",
    ));
    node.declared_dirs = vec![declared.clone()];
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.starts_with(&format!("{RULE}: ")),
        "{}",
        failure.detail
    );
    assert!(
        failure.detail.ends_with("undeclared/x.txt"),
        "{}",
        failure.detail
    );
    assert_eq!(
        std::fs::read_to_string(declared.join("x.txt")).unwrap(),
        "mine"
    );
    assert!(!undeclared.join("x.txt").exists());
}

#[test]
fn a_read_only_tree_refuses_a_write() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node(&script(
        "try:\n    open('own.txt', 'w').write('mine')\nexcept PermissionError:\n    pass",
        "'done'",
    ));
    node.tree_access = TreeAccess::ReadOnly;
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.ends_with("run-worktree/own.txt"),
        "{}",
        failure.detail
    );
    assert!(!fixture.path("run-worktree/own.txt").exists());

    // A read-only tree cannot have private git: the confinement refuses the start.
    let mut node = fixture.node(&script("", "'done'"));
    node.tree_access = TreeAccess::ReadOnly;
    node.tree_git = TreeGit::Private;
    let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure
            .detail
            .starts_with("a read-only tree cannot have private git"),
        "{}",
        failure.detail
    );
    assert!(fixture.runner.in_flight().is_empty());
}

#[test]
fn a_tree_with_no_git_runs_with_no_git_work() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let finish = fixture.run(&script("open('own.txt', 'w').write('mine')", "'done'"));
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert!(std::fs::symlink_metadata(fixture.path("run-worktree/.git")).is_err());
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
    let summary = fixture.summary(TRY_DIR);
    assert_eq!(summary["git_imported"], json!([]));
    assert_eq!(summary["git_refused"], json!([]));
}

#[test]
fn refs_outside_the_job_branch_are_refused_and_named() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let private = fixture.path("scratch/check/git");
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)\n\
                 subprocess.run(git + ['tag', 'v1'], check=True)\n\
                 subprocess.run(git + ['branch', '-f', 'other', 'HEAD'], check=True)\n\
                 os.symlink({head:?}, {linked:?})",
                head = private.join("HEAD").to_string_lossy(),
                linked = private.join("refs/heads/linked").to_string_lossy(),
            ),
            "'done'",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    for named in ["v1", "other", "linked"] {
        assert!(
            failure.detail.contains(named),
            "{named}: {}",
            failure.detail
        );
    }
    assert!(
        failure.detail.ends_with(&format!(
            "the node's git directory {} is kept",
            std::fs::canonicalize(&private).unwrap().display()
        )),
        "{}",
        failure.detail
    );
    // The own branch is imported all the same; nothing else moved in the shared repository.
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("inside"));
    assert_eq!(tip_subject(&fixture, "other").as_deref(), Some("start"));
    let home = fixture.root.path();
    let repo = fixture.path("repo");
    assert!(
        git(
            home,
            &repo,
            &["rev-parse", "--verify", "-q", "refs/tags/v1"]
        )
        .is_none()
    );
    assert!(
        git(
            home,
            &repo,
            &["rev-parse", "--verify", "-q", "refs/heads/linked"]
        )
        .is_none()
    );
    assert!(private.join("objects").is_dir());
}

#[test]
fn a_rewritten_dot_git_is_put_back() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let gitlink = fixture.path("run-worktree/.git");
    let dot_git = std::fs::read(&gitlink).unwrap();
    // `cp` is listed, so its write passes the shim (there is no sandbox here); the shim refuses
    // the script's own write to the `.git` mask.
    let node = private_node(
        &fixture,
        &common,
        &script(
            "open('fake', 'w').write('gitdir: /elsewhere\\n')\n\
             subprocess.run(['cp', 'fake', '.git'], check=True)\n\
             try:\n    open('.git', 'w').write('gitdir: /elsewhere\\n')\n\
             except PermissionError:\n    pass",
            "'done'",
        ),
        &["cp"],
    );
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.ends_with("run-worktree/.git"),
        "{}",
        failure.detail
    );
    assert_eq!(std::fs::read(&gitlink).unwrap(), dot_git);
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("start"));
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
}

#[test]
fn each_polling_pass_is_prepared_and_imported_on_its_own() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 marker = os.path.join(os.environ['TMPDIR'], 'first-done')\n\
                 first = not os.path.exists(marker)\n\
                 open(marker, 'w').write('x')\n\
                 message = 'first' if first else 'second'\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', message], check=True)"
            ),
            "'waiting' if first else 'done'",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    assert_eq!(finish.outcome, Ok(json!({ "status": "waiting" })));
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("first"));
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());

    fixture.clock.advance(Duration::from_secs(5 * 60));
    assert!(fixture.runner.poll().is_empty());
    let finish = fixture.wait();
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("second"));
    assert_eq!(tip_subject(&fixture, "run~1").as_deref(), Some("first"));
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
}

#[test]
fn a_second_job_in_a_tree_with_private_git_is_refused_while_the_first_runs() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let gate = fixture.path("scratch/check/gate");
    std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
    let fifo = std::ffi::CString::new(gate.to_string_lossy().into_owned()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    // Held open for reading and writing, which never blocks: the first job's read waits for the
    // test's write, and if the test fails first, dropping it ends that read.
    let mut gate_writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate)
        .unwrap();
    // The first job waits on the gate until the test writes to it.
    let first = private_node(
        &fixture,
        &common,
        &script(
            &format!("open({:?}).read(2)", gate.to_string_lossy()),
            "'done'",
        ),
        &[],
    );
    let mut second = private_node(&fixture, &common, &script("", "'done'"), &[]);
    second.scratch = fixture.path("scratch/second");
    second.try_dir = fixture.path("record/nodes/check/2-1");
    fixture.start(first);
    let failure = fixture
        .runner
        .start("run-1", &NodeKey::new("check", 2), 1, second)
        .unwrap_err();
    assert_eq!(failure.reason, FailureReason::WorktreeUnavailable);
    assert!(
        failure.detail.contains(bongos_confine::private_git::BUSY),
        "{}",
        failure.detail
    );
    gate_writer.write_all(b"go").unwrap();
    drop(gate_writer);
    let finish = fixture.wait();
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert!(std::fs::symlink_metadata(fixture.path("scratch/second/git")).is_err());
}

/// A pid no process has (above Linux's largest `pid_max`).
const DEAD_PID: u32 = 2_000_000_000;

/// The run's binding with `run-worktree` as the run worktree and its linked git directory as the
/// recorded one.
fn bound(fixture: &Fixture, common: &Path) -> crate::runtime::RunBinding {
    let mut binding = crate::dispatch_tests::binding(fixture.root.path(), "check");
    binding.worktree.path = fixture.path("run-worktree");
    binding.worktree.git_dir = common.join("worktrees/run-worktree");
    binding
}

/// Makes the owner of the marker in `linked` a process that is gone, as when the app crashed.
fn kill_marker_owner(linked: &Path) {
    let path = linked.join(bongos_confine::private_git::MARKER);
    let text = std::fs::read_to_string(&path).unwrap();
    let own = format!("\npid {}\n", std::process::id());
    assert!(text.contains(&own), "{text}");
    std::fs::write(
        &path,
        text.replacen(&own, &format!("\npid {DEAD_PID}\n"), 1),
    )
    .unwrap();
}

/// Waits until `path` exists, with a generous hang guard.
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while std::fs::symlink_metadata(path).is_err() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_crash_mid_pass_is_recovered_by_the_next_start() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let home = fixture.root.path().to_path_buf();
    let binding = bound(&fixture, &common);
    // The run worktree, then a branch worktree (found under `worktrees/`, not in the record).
    for name in ["run", "other"] {
        let tree = fixture.path(&format!("{name}-worktree"));
        let linked = common.join(format!("worktrees/{name}-worktree"));
        let dot_git = std::fs::read(tree.join(".git")).unwrap();
        let scratch = fixture.path(&format!("scratch/dead-{name}"));
        std::fs::create_dir_all(&scratch).unwrap();
        let private = futures::executor::block_on(bongos_confine::private_git::prepare(
            &tree, &linked, &scratch,
        ))
        .unwrap();
        git(
            &home,
            &tree,
            &["commit", "-q", "--allow-empty", "-m", "dead"],
        )
        .unwrap();
        // The app dies mid-pass: the tree still points at the private directory.
        std::mem::forget(private);
        assert_ne!(std::fs::read(tree.join(".git")).unwrap(), dot_git);

        // While the owner lives the tree is busy, and nothing changes.
        let busy = crate::dispatch_program::host_git(&binding, &tree).unwrap_err();
        assert!(busy.contains(bongos_confine::private_git::BUSY), "{busy}");
        assert_ne!(std::fs::read(tree.join(".git")).unwrap(), dot_git);

        kill_marker_owner(&linked);
        let (host, tree_git) = crate::dispatch_program::host_git(&binding, &tree).unwrap();
        assert_eq!(tree_git, TreeGit::Private);
        assert_eq!(
            host.git_dirs,
            vec![
                common.canonicalize().unwrap(),
                linked.canonicalize().unwrap()
            ]
        );
        assert_eq!(std::fs::read(tree.join(".git")).unwrap(), dot_git);
        assert!(std::fs::symlink_metadata(scratch.join("git")).is_err());
        assert!(
            std::fs::symlink_metadata(linked.join(bongos_confine::private_git::MARKER)).is_err()
        );
        // The dead pass's commit is dropped.
        assert_eq!(tip_subject(&fixture, name).as_deref(), Some("start"));
    }

    // The next start in the recovered run worktree runs and imports.
    let (host, _) =
        crate::dispatch_program::host_git(&binding, &fixture.path("run-worktree")).unwrap();
    let mut node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)"
            ),
            "'done'",
        ),
        &[],
    );
    node.host_git = HostGit {
        records_dir: node.host_git.records_dir.clone(),
        ..host
    };
    let finish = fixture.run_node(node);
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("inside"));
}

#[test]
fn host_git_refuses_a_dot_git_it_does_not_trust() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let binding = bound(&fixture, &common);
    let run = fixture.path("run-worktree");
    let gitlink = run.join(".git");
    let dot_git = std::fs::read(&gitlink).unwrap();
    let other_linked = common.join("worktrees/other-worktree");
    let untrusted = |tree: &Path| {
        let error = crate::dispatch_program::host_git(&binding, tree).unwrap_err();
        assert!(
            error.starts_with("the git directory of the node's worktree is not trusted"),
            "{error}"
        );
    };

    // A pointer at another worktree's git directory, or outside the repository.
    for target in [other_linked, fixture.path("elsewhere")] {
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(&gitlink, format!("gitdir: {}\n", target.display())).unwrap();
        untrusted(&run);
    }
    // A symlink to another worktree's `.git`.
    std::fs::remove_file(&gitlink).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(fixture.path("other-worktree/.git"), &gitlink).unwrap();
        untrusted(&run);
        std::fs::remove_file(&gitlink).unwrap();
    }
    // A `.git` directory.
    std::fs::create_dir(&gitlink).unwrap();
    untrusted(&run);
    std::fs::remove_dir(&gitlink).unwrap();

    // A tree the record names with another worktree's git directory.
    std::fs::write(&gitlink, &dot_git).unwrap();
    let mut swapped = binding.clone();
    swapped.worktree.path = fixture.path("other-worktree");
    let error = crate::dispatch_program::host_git(&swapped, &swapped.worktree.path).unwrap_err();
    assert!(error.contains("not the recorded"), "{error}");

    // The run worktree as git made it is trusted; a worktree with no `.git` gets no git.
    let (_, tree_git) = crate::dispatch_program::host_git(&binding, &run).unwrap();
    assert_eq!(tree_git, TreeGit::Private);
    let bare = fixture.path("no-git");
    std::fs::create_dir_all(&bare).unwrap();
    let (_, tree_git) = crate::dispatch_program::host_git(&binding, &bare).unwrap();
    assert_eq!(tree_git, TreeGit::None);
}

#[test]
fn a_stopped_pass_is_imported() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let dot_git = std::fs::read(fixture.path("run-worktree/.git")).unwrap();
    let gate = fixture.path("scratch/check/gate");
    let committed = fixture.path("scratch/check/committed");
    std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
    let fifo = std::ffi::CString::new(gate.to_string_lossy().into_owned()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    // Held open for reading and writing, so the script's read waits until it is stopped.
    let gate_holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate)
        .unwrap();
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)\n\
                 open({committed:?}, 'w').write('x')\n\
                 open({gate:?}).read(2)",
                committed = committed.to_string_lossy(),
                gate = gate.to_string_lossy(),
            ),
            "'done'",
        ),
        &[],
    );
    fixture.start(node);
    wait_for_file(&committed);
    fixture.runner.stop(&[("run-1".to_string(), key())]);
    drop(gate_holder);
    assert!(fixture.runner.in_flight().is_empty());
    assert!(fixture.runner.poll().is_empty());
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("inside"));
    assert_eq!(
        std::fs::read(fixture.path("run-worktree/.git")).unwrap(),
        dot_git
    );
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
}

#[test]
fn a_failed_import_fails_the_node_and_keeps_the_commits() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let dot_git = std::fs::read(fixture.path("run-worktree/.git")).unwrap();
    let private = fixture.path("scratch/check/git");
    // A file in the private object directory that is neither a loose object nor a plain pack
    // fails the import.
    let node = private_node(
        &fixture,
        &common,
        &script(
            &format!(
                "{SCRIPT_GIT}\n\
                 subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)\n\
                 open({stray:?}, 'w').write('x')",
                stray = private.join("objects/stray").to_string_lossy(),
            ),
            "'done'",
        ),
        &[],
    );
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(
        failure.reason,
        FailureReason::WorktreeUnavailable,
        "{}",
        says(failure)
    );
    assert!(
        failure
            .detail
            .starts_with("could not import the node's commits"),
        "{}",
        failure.detail
    );
    // Nothing moved; the tree is back on shared git and the commit stays in the private directory.
    assert_eq!(tip_subject(&fixture, "run").as_deref(), Some("start"));
    assert_eq!(
        std::fs::read(fixture.path("run-worktree/.git")).unwrap(),
        dot_git
    );
    let kept = git(
        fixture.root.path(),
        fixture.root.path(),
        &[
            "--git-dir",
            &private.to_string_lossy(),
            "log",
            "-1",
            "--format=%s",
            "refs/heads/run",
        ],
    );
    assert_eq!(kept.as_deref(), Some("inside"));
}

#[test]
fn a_failed_prepare_refuses_the_start_and_changes_nothing() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let tree = fixture.path("run-worktree");
    git(fixture.root.path(), &tree, &["checkout", "-q", "--detach"]).unwrap();
    let dot_git = std::fs::read(tree.join(".git")).unwrap();
    let node = private_node(&fixture, &common, &script("", "'done'"), &[]);
    let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
    assert_eq!(failure.reason, FailureReason::WorktreeUnavailable);
    assert!(
        failure
            .detail
            .contains(bongos_confine::private_git::DETACHED),
        "{}",
        failure.detail
    );
    assert!(fixture.runner.in_flight().is_empty());
    assert_eq!(std::fs::read(tree.join(".git")).unwrap(), dot_git);
    assert!(std::fs::symlink_metadata(fixture.path("scratch/check/git")).is_err());
    let marker = common
        .join("worktrees/run-worktree")
        .join(bongos_confine::private_git::MARKER);
    assert!(std::fs::symlink_metadata(marker).is_err());
}

/// A fixture whose interpreter does not exist, so nothing it starts can run: a start that gets
/// past the sandbox decision fails at the spawn with `program interpreter missing`, and one that
/// does not never reaches it. Needs no Python.
pub(crate) fn without_python(probe: SandboxProbe) -> Fixture {
    let root = TempDir::new().unwrap();
    let clock = FakeClock::new();
    let interpreter = Interpreter {
        program: root.path().join("no-such-python"),
        version: (3, 12),
    };
    let runner = ProgramRunner::new(interpreter, base_env(root.path()))
        .with_clock(clock.clone())
        .with_sandbox_probe(probe);
    Fixture {
        root,
        runner,
        clock,
    }
}

/// The names of the files in the try directory, sorted.
fn written(fixture: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixture.path(TRY_DIR))
        .map(|read| {
            read.flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn start_with(fixture: &mut Fixture, permit: Permit, mode: Mode) -> Result<(), Failure> {
    let mut node = fixture.node("raise SystemExit(0)\n");
    node.permit = permit;
    node.mode = mode;
    fixture.runner.start("run-1", &key(), 1, node).map(|_| ())
}

#[test]
fn a_failing_probe_runs_only_under_allow_and_otherwise_asks_or_refuses() {
    // Allow runs past the decision to the spawn, saying who allowed it.
    let mut fixture = without_python(no_sandbox());
    let failure = start_with(&mut fixture, Permit::Ask, Mode::Allow).unwrap_err();
    assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
    assert_eq!(
        fixture.summary(TRY_DIR)["sandbox"],
        "unsandboxed (allowed by setting)"
    );

    // Ask needs an answer and refuse refuses: nothing is spawned and only the summary is written.
    for (mode, reason, detail) in [
        (
            Mode::Ask,
            FailureReason::SandboxRequired,
            format!("no OS sandbox is available ({NO_SANDBOX})"),
        ),
        (
            Mode::Refuse,
            FailureReason::SandboxRefused,
            format!("{REFUSED_TEXT} ({NO_SANDBOX})"),
        ),
    ] {
        let mut fixture = without_python(no_sandbox());
        let failure = start_with(&mut fixture, Permit::Ask, mode).unwrap_err();
        assert_eq!(failure.reason, reason, "{mode:?}");
        assert_eq!(failure.detail, detail, "{mode:?}");
        assert_eq!(failure.node, Some(key()));
        assert_eq!(written(&fixture), [SUMMARY_FILE], "{mode:?}");
        assert_eq!(
            fixture.summary(TRY_DIR),
            json!({ "pass": 1, "sandbox": detail })
        );
        assert!(fixture.runner.in_flight().is_empty(), "{mode:?}");
    }
}

#[test]
fn the_hard_refusals_ignore_an_allowed_permit() {
    let allowed = Permit::Allowed(AllowedBy::RunConsent {
        at: "2026-01-02T03:04:05Z".to_string(),
        by: "person".to_string(),
    });
    for snapshot in [true, false] {
        for mode in [Mode::Allow, Mode::Ask] {
            let mut fixture = without_python(no_sandbox());
            let mut node = fixture.node("raise SystemExit(0)\n");
            // A snapshot is a read-only tree that needs the sandbox, as the dispatcher builds it
            // (`needs_sandbox` is `setup.branchless()`, which covers snapshots and work dirs).
            node.tree_access = if snapshot {
                TreeAccess::ReadOnly
            } else {
                TreeAccess::Writable
            };
            node.needs_sandbox = true;
            node.permit = allowed.clone();
            node.mode = mode;
            let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
            assert_eq!(failure.reason, FailureReason::PermissionDenied, "{mode:?}");
            let expected = if snapshot {
                SNAPSHOT_NEEDS_SANDBOX
            } else {
                WORK_DIR_NEEDS_SANDBOX
            };
            assert_eq!(failure.detail, format!("{expected}: {NO_SANDBOX}"));
            assert!(written(&fixture).is_empty(), "{:?}", written(&fixture));
        }
    }
}

#[test]
fn an_allowed_permit_runs_unsandboxed_and_says_who_allowed_it() {
    for (by, note) in [
        (
            AllowedBy::RunConsent {
                at: "2026-01-02T03:04:05Z".to_string(),
                by: "person".to_string(),
            },
            "unsandboxed (approved 2026-01-02T03:04:05Z)",
        ),
        (AllowedBy::Setting, "unsandboxed (allowed by setting)"),
    ] {
        let mut fixture = without_python(no_sandbox());
        let failure = start_with(&mut fixture, Permit::Allowed(by), Mode::Ask).unwrap_err();
        assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
        assert_eq!(
            fixture.summary(TRY_DIR),
            json!({ "pass": 1, "sandbox": note })
        );
    }
}

#[test]
fn a_program_node_with_the_default_permit_refuses() {
    for mode in [Mode::Ask, Mode::Allow, Mode::Refuse] {
        let mut fixture = without_python(no_sandbox());
        let mut node = fixture.node("raise SystemExit(0)\n");
        assert_eq!(Permit::default(), Permit::Refuse);
        node.permit = Permit::default();
        node.mode = mode;
        let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
        assert_eq!(failure.reason, FailureReason::SandboxRefused, "{mode:?}");
        assert_eq!(written(&fixture), [SUMMARY_FILE]);
    }
}

#[test]
fn each_start_probes_for_itself() {
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = probes.clone();
    let probe: SandboxProbe = Arc::new(move |_, _| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(NO_SANDBOX.to_string())
    });
    let mut fixture = without_python(probe);
    for (index, node_key) in [NodeKey::new("check", 1), NodeKey::new("check", 2)]
        .into_iter()
        .enumerate()
    {
        let mut node = fixture.node("raise SystemExit(0)\n");
        node.try_dir = fixture.path(&format!("record/nodes/check/{}-1", index + 1));
        node.permit = Permit::Ask;
        node.mode = Mode::Ask;
        let failure = fixture
            .runner
            .start("run-1", &node_key, 1, node)
            .unwrap_err();
        assert_eq!(failure.reason, FailureReason::SandboxRequired);
        assert_eq!(failure.node, Some(node_key));
        assert_eq!(
            probes.load(std::sync::atomic::Ordering::SeqCst),
            index + 1,
            "no answer is remembered between starts"
        );
    }
}

#[test]
fn a_bad_script_result_gets_the_one_validation_message() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let node = fixture.node("pass");
    let outcome = crate::runner_program::classify(
        &key(),
        &node,
        crate::runner_program::Exit::Code(0),
        false,
        &crate::runner_program::DenialLog::ended(Vec::new()),
        crate::runner_program::ResultFile::Text("{\"status\": \"maybe\"}".to_string()),
        "",
    );
    // The same text a Claude result gets: both go through `schema::check`.
    assert_eq!(
        outcome.unwrap_err().detail,
        "Result failed validation at `/status`: expected one of \"one-shot\", \"planned\", \"waiting\", \"done\", got \"maybe\""
    );
}

/// Starts `node` and returns the refusal, checking that nothing of the try was written.
fn refused_start(fixture: &mut Fixture, node: ProgramNode) -> Failure {
    let try_dir = node.try_dir.clone();
    let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
    for name in [POLICY_FILE, STATE_FILE, SUMMARY_FILE] {
        assert!(!try_dir.join(name).exists(), "{name} was written");
    }
    failure
}

#[test]
fn a_lane_directory_holding_the_run_records_refuses_the_start() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node("pass");
    let lane = fixture.path("record");
    std::fs::create_dir_all(&lane).unwrap();
    node.declared_dirs = vec![lane.clone()];
    let failure = refused_start(&mut fixture, node);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(
        failure.detail,
        format!(
            "a writable directory of this node contains its own policy file: {}",
            lane.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_writable_directory_pointing_at_the_try_directory_refuses_the_start() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node("pass");
    std::fs::create_dir_all(&node.try_dir).unwrap();
    let link = fixture.path("lane-link");
    std::os::unix::fs::symlink(&node.try_dir, &link).unwrap();
    node.declared_dirs = vec![link.clone()];
    let failure = refused_start(&mut fixture, node);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(
        failure.detail,
        format!(
            "a writable directory of this node contains its own policy file: {}",
            link.display()
        )
    );
}

#[test]
fn a_node_whose_policy_is_outside_its_writable_directories_passes_the_check() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node("pass");
    let lane = fixture.path("lane");
    std::fs::create_dir_all(&lane).unwrap();
    std::fs::create_dir_all(&node.scratch).unwrap();
    std::fs::create_dir_all(&node.try_dir).unwrap();
    node.declared_dirs = vec![lane];
    assert_eq!(check_policy_outside_writable(&node, &node.try_dir), Ok(()));
}

#[test]
fn a_missing_try_directory_is_made_before_the_check_and_the_start_passes() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let node = fixture.node(&script("", "'done'"));
    let try_dir = node.try_dir.clone();
    assert!(!try_dir.exists());
    fixture.start(node);
    assert!(try_dir.join(POLICY_FILE).is_file());
    assert_eq!(fixture.wait().outcome, Ok(json!({ "status": "done" })));
}

#[test]
fn a_writable_directory_that_cannot_be_resolved_refuses_the_start_and_is_named() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let mut node = fixture.node("pass");
    let missing = fixture.path("missing-lane");
    node.declared_dirs = vec![missing.clone()];
    let failure = refused_start(&mut fixture, node);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.contains(&format!("{}: ", missing.display())),
        "{}",
        failure.detail
    );
}
