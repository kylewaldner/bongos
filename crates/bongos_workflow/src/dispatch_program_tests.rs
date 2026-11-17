//! Tests of the program node kind: a script run through [`ProgramNodes`] reads its state document
//! and routes by its result, and `@remote` in a node's network list becomes the host of the task
//! repository's push remote. Each run test skips when no Python 3.8+ is on `PATH`. The OS sandbox
//! is replaced by a probe that says it is unavailable; the clock is fake; every directory is a
//! temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_workflow_model::inputs::{Input, InputValue};
use bongos_workflow_model::{Failure, FailureReason, NodeKey, RunState, parse};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::dispatch_program::{NO_ORIGIN, ProgramNodes, remote_hosts, task_remote_hosts};
use crate::dispatch_tests::binding;
use crate::runner_program::{Clock, Interpreter, SandboxProbe};
use crate::runtime::{NodeStart, RunBinding};
use crate::worktrees::Git;

const FLOW: &str = r#"
schema = 1
name = "flow"
start = "check"

[nodes.check]
kind = "program"
script = "check.py"
with = { greeting = "hi" }
permissions = { network = ["@remote", "example.org"] }
output = { status = { type = "choice", values = ["ready", "waiting"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const SCRIPT: &str = "import json, os\n\
state = json.load(open(os.environ['BONGOS_STATE']))\n\
assert state['settings'] == {'greeting': 'hi'}, state['settings']\n\
assert state['inputs']['topic'] == 'the login bug', state['inputs']\n\
assert state['run']['id'] == 'run-1'\n\
with open(os.environ['BONGOS_RESULT'], 'w') as result:\n    \
json.dump({'status': 'ready'}, result)\n";

struct FakeClock(Mutex<Instant>);

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

fn base_env(home: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ])
}

/// A git that ignores the user's configuration.
fn git(home: &Path) -> Git {
    Git::new("git")
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

fn git_run(git: &Git, dir: &Path, args: &[&str]) {
    futures::executor::block_on(git.run(dir, args)).unwrap();
}

/// A repository in `root/repo`, with an `origin` remote at `origin` when given.
fn repository(root: &Path, origin: Option<&str>) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = git(root);
    git_run(&git, &repo, &["init", "-q"]);
    if let Some(url) = origin {
        git_run(&git, &repo, &["remote", "add", "origin", url]);
    }
    repo
}

fn bound(root: &Path, repo: &Path) -> RunBinding {
    let mut binding = binding(root, "flow");
    binding.checkout.worktree = Some(repo.to_path_buf());
    fs::create_dir_all(&binding.worktree.path).unwrap();
    let definition = binding.attempt_dir.join("definition/0");
    fs::create_dir_all(&definition).unwrap();
    fs::write(definition.join("check.py"), SCRIPT).unwrap();
    binding.inputs.push(Input {
        name: "topic".to_string(),
        value: Some(InputValue::Text("the login bug".to_string())),
        source: "literal:the login bug".to_string(),
        missing: false,
    });
    binding
}

fn start(
    kind: &mut ProgramNodes,
    binding: &RunBinding,
    snapshot: &RunnerSnapshot,
) -> Result<(), Failure> {
    let definition = parse(FLOW).unwrap();
    let key = NodeKey::new("check", 1);
    let state = RunState::default();
    kind.start(
        &NodeStart {
            run: "run-1",
            definition: &definition,
            key: &key,
            try_number: 1,
            state: &state,
            budget_left_usd: None,
            binding: Some(binding),
        },
        binding,
        snapshot,
    )
    .map(|_| ())
}

fn kind() -> ProgramNodes {
    let probe: SandboxProbe = Arc::new(|_, _| Err("no sandbox (test)".to_string()));
    ProgramNodes::with_clock_and_probe(Arc::new(FakeClock(Mutex::new(Instant::now()))), probe)
}

fn snapshot(root: &Path) -> Option<RunnerSnapshot> {
    let env = base_env(root);
    match Interpreter::find(None, &env) {
        Ok(interpreter) => Some(RunnerSnapshot {
            interpreter: Some(Ok(interpreter)),
            base_env: env,
            ..RunnerSnapshot::default()
        }),
        Err(failure) => {
            eprintln!("skipped: {failure}");
            None
        }
    }
}

#[test]
fn a_script_reads_its_state_document_and_its_result_routes() {
    let root = TempDir::new().unwrap();
    let Some(snapshot) = snapshot(root.path()) else {
        return;
    };
    let repo = repository(root.path(), Some("https://github.com/o/r.git"));
    let binding = bound(root.path(), &repo);
    let mut kind = kind();
    start(&mut kind, &binding, &snapshot).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let finish = loop {
        if let Some(finish) = kind.poll().pop() {
            break finish;
        }
        assert!(Instant::now() < deadline, "the program did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(finish.outcome, Ok(json!({ "status": "ready" })));
    assert_eq!(finish.key, NodeKey::new("check", 1));

    // The node's network list carries the resolved host in place of `@remote`.
    let policy = binding
        .attempt_dir
        .join("nodes/check/1-1")
        .join(crate::runner_program::POLICY_FILE);
    let policy: Value = serde_json::from_str(&fs::read_to_string(policy).unwrap()).unwrap();
    assert_eq!(
        policy["network"],
        json!(["github.com", "*.github.com", "example.org"])
    );
}

#[test]
fn a_missing_interpreter_fails_the_node() {
    let root = TempDir::new().unwrap();
    let repo = repository(root.path(), None);
    let binding = bound(root.path(), &repo);
    let failure = start(&mut kind(), &binding, &RunnerSnapshot::default()).unwrap_err();
    assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
}

#[test]
fn remote_resolves_https_and_scp_style_origins_to_host_and_subdomains() {
    let github = Some(vec!["github.com".to_string(), "*.github.com".to_string()]);
    assert_eq!(remote_hosts("https://github.com/o/r.git"), github);
    assert_eq!(remote_hosts("https://user@GitHub.com:8443/o/r"), github);
    assert_eq!(remote_hosts("git@github.com:o/r.git"), github);
    assert_eq!(remote_hosts("ssh://git@github.com:22/o/r.git"), github);
    assert_eq!(remote_hosts("/srv/git/r.git"), None);
    assert_eq!(remote_hosts("file:///srv/git/r.git"), None);
    assert_eq!(remote_hosts(""), None);

    let root = TempDir::new().unwrap();
    for url in ["https://github.com/o/r.git", "git@github.com:o/r.git"] {
        let dir = root.path().join(url.len().to_string());
        fs::create_dir_all(&dir).unwrap();
        let repo = repository(&dir, Some(url));
        let binding = bound(&dir, &repo);
        let hosts = task_remote_hosts(&git(&dir), &binding).unwrap();
        assert_eq!(Some(hosts), github, "{url}");
    }
}

#[test]
fn a_task_repository_with_no_origin_fails_the_node() {
    let root = TempDir::new().unwrap();
    let repo = repository(root.path(), None);
    let binding = bound(root.path(), &repo);
    assert_eq!(
        task_remote_hosts(&git(root.path()), &binding),
        Err(NO_ORIGIN.to_string())
    );
    assert_eq!(
        NO_ORIGIN,
        "the task repository has no origin remote to take hosts from"
    );
    // Through the kind, the failure names the node; the interpreter check comes first, so give
    // it a snapshot with the interpreter whatever Python is there (the start fails before use).
    let snapshot = RunnerSnapshot {
        interpreter: Some(Ok(Interpreter {
            program: "python3".into(),
            version: (3, 8),
        })),
        base_env: base_env(root.path()),
        ..RunnerSnapshot::default()
    };
    let failure = start(&mut kind(), &binding, &snapshot).unwrap_err();
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert_eq!(failure.detail, NO_ORIGIN);
    assert_eq!(failure.node, Some(NodeKey::new("check", 1)));
}
