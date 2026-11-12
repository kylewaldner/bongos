//! Tests of the program runner: the state document, the permission checks of the shim (commands,
//! writes, network), the output, time and result caps, polling, the MCP refusal, the interpreter
//! check and the sandbox report. Each test skips when no Python 3.8+ is on `PATH`. The OS sandbox
//! is replaced by a probe that says it is unavailable, so the shim's checks are what is tested and
//! no test needs `bwrap`; one test uses the real probe and accepts either answer. Nothing touches
//! the real `HOME` or the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_workflow_model::definition::Permissions;
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::state_doc::{self, Paths, RunIdentity};
use bongos_workflow_model::{Definition, Event, Failure, FailureReason, NodeKey, parse, replay};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::runner_program::{
    Clock, DENIALS_END, DENIALS_FILE, GitGuard, INTERPRETER_FIX, Interpreter, MCP_REFUSAL,
    PollPlan, ProgramCaps, ProgramNode, ProgramRunner, ProgramSettings, SUMMARY_FILE, SandboxProbe,
    git_metadata_needed, hide_in_bubblewrap, os_sandbox_probe, remove_symlinks,
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

const NO_SANDBOX: &str = "bwrap not found (test)";

fn no_sandbox() -> SandboxProbe {
    Arc::new(|_, _| Err(NO_SANDBOX.to_string()))
}

struct Fixture {
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
    fn node(&self, script: &str) -> ProgramNode {
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
            },
        ]);
        let state = state_doc::build(
            &[],
            &state,
            RunIdentity {
                id: "run-1".to_string(),
                workflow: "check".to_string(),
                todo: TodoRef {
                    index: 0,
                    name: "check".to_string(),
                },
                task_id: "task-1".to_string(),
                task_dir: self.path("task"),
                branch: "feature/check".to_string(),
                run_branch: "run/feature/check/1".to_string(),
            },
            &declared.with,
            Paths {
                worktree: worktree.clone(),
                scratch: scratch.clone(),
            },
        );
        let settings = ProgramSettings::default();
        let try_dir = self.path("record/nodes/check/1-1");
        ProgramNode {
            script: script_path,
            output: declared.output.clone(),
            permissions: declared.permissions.clone().unwrap_or_default(),
            caps: ProgramCaps::resolve(declared, &settings),
            polls: declared
                .polls
                .as_ref()
                .map(|polls| PollPlan::resolve(polls, &settings)),
            state,
            worktree,
            scratch,
            git_common_dir: None,
            try_dir: try_dir.clone(),
            task_id: "task-1".to_string(),
            task_dir: self.path("task"),
            node_result: try_dir.join("node-result.json"),
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

fn key() -> NodeKey {
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
    assert!(failure.detail.starts_with("write "), "{}", failure.detail);
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
    assert_eq!(summary["sandbox"], format!("not sandboxed: {NO_SANDBOX}"));
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
        note == "sandboxed" || note.starts_with("not sandboxed: "),
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
        "permission denied: write /bongos-no-such-file"
    );

    let finish = fixture.run(&script(
        "fd = os.open('/', os.O_RDONLY)\n\
         try:\n    os.open('bongos-x', os.O_WRONLY | os.O_CREAT, dir_fd=fd)\n\
         except PermissionError:\n    pass",
        "'done'",
    ));
    assert_eq!(
        says(failure(&finish)),
        "permission denied: write relative to a directory descriptor: bongos-x"
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
fn git_hooks_and_config_are_protected() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let common = fixture.path("repo.git");
    std::fs::create_dir_all(common.join("hooks")).unwrap();
    std::fs::create_dir_all(common.join("refs/heads")).unwrap();
    std::fs::write(common.join("config"), "").unwrap();
    let mut node = fixture.node(&script(
        &format!(
            "open({ref_:?}, 'w').write('x')\n\
             try:\n    open({hook:?}, 'w').write('x')\nexcept PermissionError:\n    pass",
            ref_ = common.join("refs/heads/run").to_string_lossy(),
            hook = common.join("hooks/pre-commit").to_string_lossy(),
        ),
        "'done'",
    ));
    node.git_common_dir = Some(common.clone());
    let policy = node.sandbox_policy();
    let sandbox::SandboxFsPolicy::Restricted { writable_paths } = &policy.fs else {
        panic!("restricted");
    };
    assert!(!writable_paths.contains(&common));
    for name in ["objects", "refs", "logs"] {
        assert!(writable_paths.contains(&common.join(name)), "{name}");
    }
    // No worktree git directory, so only the replace refs and alternates are bound read-only.
    let sealed = if cfg!(target_os = "linux") {
        node.git_forbidden_paths()
    } else {
        Vec::new()
    };
    assert_eq!(sealed.len(), if cfg!(target_os = "linux") { 3 } else { 0 });
    assert_eq!(
        policy.git,
        sandbox::GitSandboxPolicy::Denied { git_dirs: sealed }
    );
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.ends_with("hooks/pre-commit"),
        "{}",
        failure.detail
    );
    assert!(common.join("refs/heads/run").exists());
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
fn a_program_that_changes_git_metadata_fails_and_it_is_put_back() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let common = fixture.path("repo.git");
    let git_dir = common.join("worktrees/run");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(common.join("config"), "[core]\n").unwrap();
    std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    std::fs::write(git_dir.join("gitdir"), "run-worktree/.git\n").unwrap();
    let worktree = fixture.path("run-worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let gitlink = format!("gitdir: {}\n", git_dir.to_string_lossy());
    std::fs::write(worktree.join(".git"), &gitlink).unwrap();

    // `cp` is a listed command, so its writes pass the shim; the runner still sees them.
    let mut node = fixture.node(&script(
        &format!(
            "open('fake', 'w').write('gitdir: /elsewhere\\n')\n\
             subprocess.run(['cp', 'fake', '.git'], check=True)\n\
             subprocess.run(['cp', 'fake', {commondir:?}], check=True)",
            commondir = git_dir.join("commondir").to_string_lossy(),
        ),
        "'done'",
    ));
    node.permissions = Permissions {
        commands: vec!["cp".to_string()],
        ..Permissions::default()
    };
    node.git_common_dir = Some(common);
    let protected = node.read_only_paths();
    for path in [
        worktree.join(".git"),
        git_dir.join("gitdir"),
        git_dir.join("commondir"),
        git_dir.join("config.worktree"),
    ] {
        assert!(protected.contains(&path), "{protected:?}");
    }
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.starts_with("program changed git metadata: "),
        "{}",
        failure.detail
    );
    assert!(failure.detail.contains("commondir"), "{}", failure.detail);
    assert_eq!(
        std::fs::read_to_string(worktree.join(".git")).unwrap(),
        gitlink
    );
    assert_eq!(
        std::fs::read_to_string(git_dir.join("commondir")).unwrap(),
        "../..\n"
    );
    // A missing `config.worktree` is made before the script, so the sandbox can bind it.
    assert_eq!(
        std::fs::read_to_string(git_dir.join("config.worktree")).unwrap(),
        ""
    );
    let summary = fixture.summary(TRY_DIR);
    assert_eq!(summary["git_metadata_changed"].as_array().unwrap().len(), 2);
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
    assert!(detail.starts_with("permission denied: write "), "{detail}");
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
    assert!(detail.starts_with("permission denied: write "), "{detail}");
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
fn the_writable_git_paths_are_an_allowlist() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let mut node = fixture.node("pass\n");
    node.git_common_dir = Some(common.clone());
    // git writes the real path into the worktree's `.git` file.
    let own = std::fs::canonicalize(common.join("worktrees/run-worktree")).unwrap();
    assert_eq!(
        node.writable_paths(),
        vec![
            node.worktree.clone(),
            node.scratch.clone(),
            common.join("objects"),
            common.join("refs"),
            common.join("logs"),
            own.clone(),
        ]
    );
    assert_eq!(
        node.read_only_paths(),
        vec![
            node.worktree.join(".git"),
            own.join("gitdir"),
            own.join("commondir"),
            own.join("config.worktree"),
        ]
    );

    // In the main worktree the git directory is the common one: read-only as a whole.
    node.worktree = fixture.path("repo");
    assert_eq!(
        node.writable_paths(),
        vec![node.worktree.clone(), node.scratch.clone()]
    );
    assert_eq!(node.read_only_paths(), vec![common]);
}

#[test]
fn the_shim_refuses_git_metadata_outside_the_allowlist() {
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
        node.git_common_dir = Some(common.clone());
        let finish = fixture.run_node(node);
        let detail = says(failure(&finish));
        let tail = target.strip_prefix(fixture.root.path()).unwrap();
        assert!(detail.starts_with("permission denied: write "), "{detail}");
        assert!(detail.ends_with(&*tail.to_string_lossy()), "{detail}");
        assert_eq!(std::fs::read(&target).ok(), before, "{}", target.display());
    }
}

#[test]
fn the_sandbox_keeps_git_metadata_read_only_and_a_commit_still_works() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    // Bubblewrap hides the host's /tmp behind its own, so a fixture there cannot be sandboxed.
    if fixture.root.path().starts_with("/tmp") {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let targets = git_metadata_targets(&fixture, &common);
    let own = std::fs::canonicalize(common.join("worktrees/run-worktree")).unwrap();
    // The reflogs, which a host git appends to while the script runs, and the replace refs and
    // alternates, which change what every git sees: none writable inside the sandbox.
    let reflogs = [common.join("logs/refs/heads/run"), own.join("logs/HEAD")];
    let sealed = [
        common.join("objects/info/alternates"),
        common.join("objects/info/http-alternates"),
        common.join("refs/replace/0123"),
    ];
    let names: Vec<String> = targets
        .iter()
        .chain(&reflogs)
        .chain(&sealed)
        .map(|target| target.to_string_lossy().into_owned())
        .collect();
    let bashrc = fixture.path("bashrc");
    std::fs::write(&bashrc, "safe\n").unwrap();
    let bashrc = bashrc.to_string_lossy().into_owned();
    // `cp`, `ln` and `git` are listed, so their writes pass the shim: only the OS sandbox stops
    // them.
    let mut node = fixture.node(&script(
        &format!(
            "open('fake', 'w').write('gitdir: /elsewhere\\n')\n\
             for target in {names:?}:\n    \
             copied = subprocess.run(['cp', 'fake', target], stderr=subprocess.DEVNULL)\n    \
             assert copied.returncode != 0, target\n    \
             linked = subprocess.run(['ln', '-sf', {bashrc:?}, target], stderr=subprocess.DEVNULL)\n    \
             assert linked.returncode != 0, target\n\
             git = ['git', '-c', 'user.name=test', '-c', 'user.email=test@example.invalid']\n\
             subprocess.run(git + ['commit', '-q', '--allow-empty', '-m', 'inside'], check=True)\n\
             subprocess.run(git + ['branch', 'side'], check=True)"
        ),
        "'done'",
    ));
    node.permissions = Permissions {
        commands: vec!["cp".to_string(), "ln".to_string(), "git".to_string()],
        ..Permissions::default()
    };
    let reflogs_before: Vec<_> = reflogs
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    node.git_common_dir = Some(common);
    if let Err(reason) = (os_sandbox_probe())(&node.sandbox_policy(), &node.worktree) {
        eprintln!("skipped: {reason}");
        return;
    }
    // `config.worktree` is made before the script starts; the rest must stay as it is now.
    let mut before: Vec<_> = targets
        .iter()
        .map(|target| std::fs::read(target).ok())
        .collect();
    before[6] = Some(Vec::new());
    let interpreter = fixture.runner.interpreter().clone();
    fixture.runner = ProgramRunner::new(interpreter, base_env(fixture.root.path()))
        .with_clock(fixture.clock.clone())
        .with_sandbox_probe(os_sandbox_probe());
    let finish = fixture.run_node(node);
    let stderr =
        std::fs::read_to_string(fixture.path(TRY_DIR).join("stderr.log")).unwrap_or_default();
    assert_eq!(fixture.summary(TRY_DIR)["sandbox"], "sandboxed");
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })), "{stderr}");
    let after: Vec<_> = targets
        .iter()
        .map(|target| std::fs::read(target).ok())
        .collect();
    assert_eq!(after, before);
    // The commit wrote no reflog; the host's are as they were.
    let reflogs_after: Vec<_> = reflogs
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(reflogs_after, reflogs_before);
    assert_eq!(std::fs::read_to_string(&sealed[0]).unwrap(), "");
    assert_eq!(std::fs::read_to_string(&sealed[1]).unwrap(), "");
    assert!(std::fs::symlink_metadata(&sealed[2]).is_err());
    let home = fixture.root.path();
    let repo = fixture.path("repo");
    assert_eq!(
        git(home, &repo, &["log", "-1", "--format=%s", "run"]).as_deref(),
        Some("inside")
    );
    assert!(git(home, &repo, &["rev-parse", "--verify", "-q", "side"]).is_some());
}

#[test]
fn the_shim_refuses_symlinks_and_writes_through_them_in_git() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let worktree = fixture.path("run-worktree");
    let inside = worktree.join("inside.txt");
    std::fs::write(&inside, "kept\n").unwrap();
    let planted = common.join("refs/heads/planted");
    let moved = common.join("logs/refs/heads/moved");
    let show = |path: &Path| path.to_string_lossy().into_owned();
    let cases = [
        (
            format!("os.symlink({:?}, {:?})", show(&inside), show(&planted)),
            "create a symlink in git ",
        ),
        (
            format!(
                "os.symlink({:?}, {:?})",
                show(&inside),
                show(&common.join("objects/../refs/heads/dotted"))
            ),
            "create a symlink in git ",
        ),
        (
            format!("open({:?}, 'a').write('x')", show(&planted)),
            "write through a symlink in git ",
        ),
        (
            format!("os.replace('to-move', {:?})", show(&moved)),
            "move a symlink into git ",
        ),
        (
            format!(
                "os.link('inside.txt', {:?})",
                show(&common.join("refs/heads/hard"))
            ),
            "hard link into git ",
        ),
    ];
    for (body, denied) in cases {
        // Planted by the host, as a listed command could: the shim must not write through it.
        std::os::unix::fs::symlink(&inside, &planted).ok();
        std::os::unix::fs::symlink(&inside, worktree.join("to-move")).ok();
        let mut node = fixture.node(&script(
            &format!("try:\n    {body}\nexcept PermissionError:\n    pass"),
            "'done'",
        ));
        node.git_common_dir = Some(common.clone());
        let finish = fixture.run_node(node);
        assert!(finish.outcome.is_err(), "{body}");
        let denials = std::fs::read_to_string(fixture.path(TRY_DIR).join(DENIALS_FILE)).unwrap();
        assert!(denials.contains(denied), "{body}: {denials}");
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "kept\n");
        assert!(std::fs::symlink_metadata(&moved).is_err(), "{body}");
        assert!(std::fs::symlink_metadata(common.join("refs/heads/dotted")).is_err());
        std::fs::remove_file(worktree.join("to-move")).ok();
    }

    // A symlink in the worktree or the scratch directory is allowed (`python -m venv` makes
    // some), and so is writing through one there.
    std::fs::remove_file(&planted).ok();
    let mut node = fixture.node(&script(
        "os.symlink('inside.txt', 'link-in-worktree')\n\
         open('link-in-worktree', 'a').write('more\\n')\n\
         os.symlink(sys.executable, os.path.join(os.environ['TMPDIR'], 'python'))",
        "'done'",
    ));
    node.git_common_dir = Some(common);
    let finish = fixture.run_node(node);
    assert_eq!(finish.outcome, Ok(json!({ "status": "done" })));
    assert!(
        std::fs::symlink_metadata(worktree.join("link-in-worktree"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "kept\nmore\n");
}

#[test]
fn a_git_path_that_is_already_a_symlink_refuses_the_start() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let moved = fixture.path("logs-elsewhere");
    std::fs::rename(common.join("logs"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, common.join("logs")).unwrap();
    let mut node = fixture.node(&script("pass", "'done'"));
    node.git_common_dir = Some(common.clone());
    assert_eq!(node.symlinked_git_paths(), vec![common.join("logs")]);
    let failure = fixture.runner.start("run-1", &key(), 1, node).unwrap_err();
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(
        failure.detail,
        format!(
            "git paths are symlinks, so bongos will not make them writable: {}",
            common.join("logs").display()
        )
    );
    assert!(fixture.runner.in_flight().is_empty());
}

#[test]
fn bubblewrap_covers_the_reflogs_before_the_end_of_its_options() {
    let root = TempDir::new().unwrap();
    let logs = root.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let logs = std::fs::canonicalize(&logs)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut args: Vec<String> = ["--ro-bind", "/", "/", "--", "python3", "--"]
        .map(String::from)
        .to_vec();
    hide_in_bubblewrap(&mut args, &[root.path().join("logs")]).unwrap();
    let expected: Vec<String> = [
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        &logs,
        "--remount-ro",
        &logs,
        "--",
        "python3",
        "--",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(args, expected);
    let mut no_end = vec!["--ro-bind".to_string()];
    assert!(hide_in_bubblewrap(&mut no_end, &[root.path().join("logs")]).is_err());
    assert!(hide_in_bubblewrap(&mut no_end, &[]).is_ok());
}

#[test]
fn the_walk_removes_every_symlink_without_following_one() {
    if cfg!(not(unix)) {
        return;
    }
    let root = TempDir::new().unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir_all(outside.join("deep")).unwrap();
    std::fs::write(outside.join("deep/file"), "x").unwrap();
    let walked = root.path().join("refs");
    std::fs::create_dir_all(walked.join("heads/nested")).unwrap();
    std::fs::write(walked.join("heads/main"), "abc\n").unwrap();
    let file_link = walked.join("heads/nested/evil");
    let dir_link = walked.join("heads/to-outside");
    std::os::unix::fs::symlink(outside.join("deep/file"), &file_link).unwrap();
    std::os::unix::fs::symlink(&outside, &dir_link).unwrap();

    let mut expected = vec![file_link, dir_link];
    expected.sort();
    assert_eq!(remove_symlinks(&walked), expected);
    assert_eq!(
        std::fs::read_to_string(walked.join("heads/main")).unwrap(),
        "abc\n"
    );
    // The targets are left alone.
    assert_eq!(
        std::fs::read_to_string(outside.join("deep/file")).unwrap(),
        "x"
    );
    assert!(remove_symlinks(&walked).is_empty());
    // A root that is itself a symlink is not walked.
    let root_link = root.path().join("root-link");
    std::os::unix::fs::symlink(&outside, &root_link).unwrap();
    std::os::unix::fs::symlink(outside.join("deep/file"), outside.join("deep/link")).unwrap();
    assert!(remove_symlinks(&root_link).is_empty());
    assert!(std::fs::symlink_metadata(outside.join("deep/link")).is_ok());
}

#[test]
fn the_git_guard_undoes_symlinks_replace_refs_and_alternates() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let mut node = fixture.node("pass\n");
    node.git_common_dir = Some(common.clone());
    assert_eq!(
        node.git_forbidden_paths(),
        vec![
            common.join("refs/replace"),
            common.join("objects/info/alternates"),
            common.join("objects/info/http-alternates"),
        ]
    );
    let bashrc = fixture.path("bashrc");
    std::fs::write(&bashrc, "safe\n").unwrap();
    assert_eq!(
        node.hidden_git_paths(),
        vec![
            common.join("logs"),
            std::fs::canonicalize(common.join("worktrees/run-worktree"))
                .unwrap()
                .join("logs"),
        ]
    );
    let own = std::fs::canonicalize(common.join("worktrees/run-worktree")).unwrap();
    // Without Bubblewrap the replace refs and alternates are snapshotted and put back; under it
    // (on Linux) the sandbox keeps them read-only and only the walk runs.
    let sandboxed = GitGuard::take(&node, true);
    let guard = GitGuard::take(&node, false);

    let reflog = common.join("logs/refs/heads/main");
    std::fs::remove_file(&reflog).unwrap();
    std::os::unix::fs::symlink(&bashrc, &reflog).unwrap();
    std::os::unix::fs::symlink(&bashrc, own.join("planted")).unwrap();
    std::fs::create_dir_all(common.join("refs/replace")).unwrap();
    std::fs::write(common.join("refs/replace/0123"), "4567\n").unwrap();
    std::fs::create_dir_all(common.join("objects/info")).unwrap();
    std::fs::write(common.join("objects/info/alternates"), "/elsewhere\n").unwrap();

    if cfg!(target_os = "linux") {
        let walked = sandboxed.check_and_restore();
        assert!(walked.metadata.is_empty());
        assert_eq!(walked.symlinks.len(), 2);
        std::os::unix::fs::symlink(&bashrc, &reflog).unwrap();
        std::os::unix::fs::symlink(&bashrc, own.join("planted")).unwrap();
    }
    let changes = guard.check_and_restore();
    let mut symlinks = vec![reflog.clone(), own.join("planted")];
    symlinks.sort();
    let mut found = changes.symlinks.clone();
    found.sort();
    assert_eq!(found, symlinks);
    assert_eq!(
        changes.metadata,
        vec![
            common.join("refs/replace"),
            common.join("objects/info/alternates")
        ]
    );
    assert!(std::fs::symlink_metadata(&reflog).is_err());
    assert!(std::fs::symlink_metadata(common.join("refs/replace")).is_err());
    assert!(std::fs::symlink_metadata(common.join("objects/info/alternates")).is_err());
    assert_eq!(std::fs::read_to_string(&bashrc).unwrap(), "safe\n");
    assert!(guard.check_and_restore().is_empty());
}

#[test]
fn a_program_that_plants_a_symlink_or_alternates_in_git_fails() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    if cfg!(not(unix)) {
        return;
    }
    let Some(common) = git_repo(&fixture) else {
        return;
    };
    let reflog = common.join("logs/refs/heads/run");
    let alternates = common.join("objects/info/alternates");
    let bashrc = fixture.path("bashrc");
    std::fs::write(&bashrc, "safe\n").unwrap();
    // `ln` and `cp` are listed, so their writes pass the shim; the runner still sees them.
    let mut node = fixture.node(&script(
        &format!(
            "open('alternates', 'w').write('/elsewhere\\n')\n\
             subprocess.run(['ln', '-sf', {bashrc:?}, {reflog:?}], check=True)\n\
             subprocess.run(['cp', 'alternates', {alternates:?}], check=True)",
            bashrc = bashrc.to_string_lossy(),
            reflog = reflog.to_string_lossy(),
            alternates = alternates.to_string_lossy(),
        ),
        "'done'",
    ));
    node.permissions = Permissions {
        commands: vec!["ln".to_string(), "cp".to_string()],
        ..Permissions::default()
    };
    node.git_common_dir = Some(common.clone());
    let finish = fixture.run_node(node);
    let failure = failure(&finish);
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure
            .detail
            .starts_with("program planted symlinks in git (removed): "),
        "{}",
        failure.detail
    );
    assert!(
        failure.detail.contains("; program changed git metadata: "),
        "{}",
        failure.detail
    );
    assert!(std::fs::symlink_metadata(&reflog).is_err());
    // On Linux an empty alternates file is made before the pass (so Bubblewrap can bind it
    // read-only), and that is what is put back.
    if cfg!(target_os = "linux") {
        assert_eq!(std::fs::read_to_string(&alternates).unwrap(), "");
        assert!(common.join("refs/replace").is_dir());
    } else {
        assert!(std::fs::symlink_metadata(&alternates).is_err());
    }
    assert_eq!(std::fs::read_to_string(&bashrc).unwrap(), "safe\n");
    let summary = fixture.summary(TRY_DIR);
    assert_eq!(summary["git_symlinks_removed"].as_array().unwrap().len(), 1);
}

#[test]
fn git_metadata_is_checked_unless_bubblewrap_sandboxes_the_run() {
    assert!(git_metadata_needed(false, true));
    assert!(git_metadata_needed(false, false));
    assert!(git_metadata_needed(true, false));
    assert!(!git_metadata_needed(true, true));
}
