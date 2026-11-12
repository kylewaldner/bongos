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
    Clock, DENIALS_FILE, INTERPRETER_FIX, Interpreter, MCP_REFUSAL, PollPlan, ProgramCaps,
    ProgramNode, ProgramRunner, ProgramSettings, SUMMARY_FILE, SandboxProbe, os_sandbox_probe,
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
    assert_eq!(denials.trim(), r#"{"what": "curl"}"#);
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
            &[],
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
