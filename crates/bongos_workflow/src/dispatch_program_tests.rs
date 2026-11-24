//! Tests of the program node kind: a script run through [`ProgramNodes`] reads its state document
//! and routes by its result, `@remote` in a node's network list becomes the host of the task
//! repository's push remote, a program of a snapshot run never runs without the OS sandbox, and a
//! program of a lane run gets the lane's variables and may write its named directories, and the
//! run's facts and the mode decide a start without the sandbox. Each run test that spawns a script
//! skips when no Python 3.8+ is on `PATH`. The OS sandbox is replaced by a probe that says
//! it is unavailable; the clock is fake; every directory is a
//! temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_workflow_model::inputs::{Input, InputValue};
use bongos_workflow_model::record::StartedBy;
use bongos_workflow_model::unsandboxed::{AllowedBy, Mode, Permit};
use bongos_workflow_model::{EnvEdit, Failure, FailureReason, NodeKey, RunState, parse};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::consent::Grant;
use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::dispatch_program::{
    NO_ORIGIN, ProgramNodes, program_permit, remote_hosts, task_remote_hosts,
};
use crate::dispatch_tests::binding;
use crate::runner_program::{Clock, Interpreter, ProgramSettings, SandboxProbe};
use crate::runtime::{Finish, NodeStart, RunBinding, RunFacts, RunSetup};
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
            grant: None,
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

const PLAIN_FLOW: &str = r#"
schema = 1
name = "plain"
start = "check"

[nodes.check]
kind = "program"
script = "check.py"
output = { status = { type = "choice", values = ["ready", "waiting"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// Writes what the script sees to `seen.json` in its scratch, writes `scratch.txt` there too,
/// then tries `changed.txt` in its worktree and in each named lane directory, as `<path>: ok` or
/// `<path>: denied` lines in `writes.txt` in its scratch.
const PROBE_SCRIPT: &str = "import json, os\n\
state = json.load(open(os.environ['BONGOS_STATE']))\n\
scratch = os.environ['TMPDIR']\n\
seen = {'env': dict(os.environ), 'paths': state['paths']}\n\
open(os.path.join(scratch, 'seen.json'), 'w').write(json.dumps(seen))\n\
open(os.path.join(scratch, 'scratch.txt'), 'w').write('mine')\n\
targets = [os.environ['BONGOS_WORKTREE']] + [v for k, v in os.environ.items() if k.startswith('BONGOS_LANE_DIR_')]\n\
lines = []\n\
for target in targets:\n    \
path = os.path.join(target, 'changed.txt')\n    \
try:\n        \
open(path, 'w').write('x')\n        \
lines.append(path + ': ok')\n    \
except PermissionError:\n        \
lines.append(path + ': denied')\n\
open(os.path.join(scratch, 'writes.txt'), 'w').write('\\n'.join(lines))\n\
with open(os.environ['BONGOS_RESULT'], 'w') as result:\n    \
json.dump({'status': 'ready'}, result)\n";

/// Starts [`PROBE_SCRIPT`] as the `check` node of `binding`.
fn start_probe(
    kind: &mut ProgramNodes,
    binding: &RunBinding,
    snapshot: &RunnerSnapshot,
) -> Result<(), Failure> {
    let definition_dir = binding.attempt_dir.join("definition/0");
    fs::create_dir_all(&definition_dir).unwrap();
    fs::write(definition_dir.join("check.py"), PROBE_SCRIPT).unwrap();
    let definition = parse(PLAIN_FLOW).unwrap();
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
            grant: None,
        },
        binding,
        snapshot,
    )
    .map(|_| ())
}

/// Runs [`PROBE_SCRIPT`] as the `check` node of `binding` and returns how it finished, what it
/// saw and what its writes did.
fn probe(binding: &RunBinding, snapshot: &RunnerSnapshot) -> (Finish, Value, String) {
    let mut kind = kind();
    start_probe(&mut kind, binding, snapshot).unwrap();
    let key = NodeKey::new("check", 1);
    let deadline = Instant::now() + Duration::from_secs(20);
    let finish = loop {
        if let Some(finish) = kind.poll().pop() {
            break finish;
        }
        assert!(Instant::now() < deadline, "the program did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    let scratch = binding
        .worktree
        .run_dir
        .join(crate::worktrees::SCRATCH_DIR)
        .join("check")
        .join(key.branch_label())
        .join("1-1")
        .join(crate::worktrees::SCRATCH_TMP_DIR);
    let seen: Value =
        serde_json::from_str(&fs::read_to_string(scratch.join("seen.json")).unwrap()).unwrap();
    assert_eq!(
        fs::read_to_string(scratch.join("scratch.txt")).unwrap(),
        "mine",
        "the scratch directory is writable"
    );
    let writes = fs::read_to_string(scratch.join("writes.txt")).unwrap_or_default();
    (finish, seen, writes)
}

#[test]
fn a_program_of_a_snapshot_run_never_runs_without_the_os_sandbox() {
    let root = TempDir::new().unwrap();
    let Some(snapshot) = snapshot(root.path()) else {
        return;
    };
    let mut binding = binding(root.path(), "plain");
    let tree = root.path().join("snapshot");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("task.toml"), "goal = \"Tidy\"\n").unwrap();
    binding.worktree.path = tree.clone();
    binding.worktree.branch = String::new();
    binding.setup.snapshot = true;

    // The probe says the sandbox is unavailable: the shim alone cannot keep the snapshot
    // read-only, so the node fails before the script runs.
    let mut kind = kind();
    let failure = start_probe(&mut kind, &binding, &snapshot).unwrap_err();
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure
            .detail
            .starts_with(crate::runner_program::SNAPSHOT_NEEDS_SANDBOX),
        "{}",
        failure.detail
    );
    assert_eq!(failure.node, Some(NodeKey::new("check", 1)));
    assert!(kind.poll().is_empty(), "nothing was started");
    assert!(!tree.join("changed.txt").exists());
    assert_eq!(
        fs::read_dir(&tree).unwrap().count(),
        1,
        "the snapshot is as it was"
    );
}

#[test]
fn a_program_of_a_lane_run_gets_its_variables_and_may_write_its_named_directories() {
    let root = TempDir::new().unwrap();
    let Some(snapshot) = snapshot(root.path()) else {
        return;
    };
    let mut binding = binding(root.path(), "plain");
    fs::create_dir_all(&binding.worktree.path).unwrap();
    let target = root.path().join("lane-target");
    fs::create_dir_all(&target).unwrap();
    binding.setup = RunSetup {
        lane: Some(bongos_workflow_model::LaneRef {
            id: "lane-1".to_string(),
            kind: "build".to_string(),
        }),
        lane_dirs: [("build-target".to_string(), target.clone())].into(),
        env: vec![EnvEdit::Set {
            name: "CARGO_TARGET_DIR".to_string(),
            value: target.to_string_lossy().into_owned(),
        }],
        ..RunSetup::default()
    };

    let (finish, seen, writes) = probe(&binding, &snapshot);
    assert_eq!(finish.outcome, Ok(json!({ "status": "ready" })));
    let env = &seen["env"];
    assert_eq!(env["BONGOS_LANE"], json!("lane-1"));
    assert_eq!(env["BONGOS_LANE_KIND"], json!("build"));
    assert_eq!(env["BONGOS_LANE_DIR_BUILD_TARGET"], json!(target));
    assert_eq!(env["CARGO_TARGET_DIR"], json!(target));
    assert_eq!(env["BONGOS_WORKTREE"], json!(binding.worktree.path));
    assert!(target.join("changed.txt").exists());
    assert!(binding.worktree.path.join("changed.txt").exists());
    assert!(!writes.contains("denied"), "{writes}");
}

/// A snapshot whose interpreter does not exist, under `mode`: a start that gets past the sandbox
/// decision fails at the spawn with `program interpreter missing`. Needs no Python.
fn no_python(root: &Path, mode: Mode) -> RunnerSnapshot {
    RunnerSnapshot {
        interpreter: Some(Ok(Interpreter {
            program: root.join("no-such-python"),
            version: (3, 12),
        })),
        base_env: base_env(root),
        programs: ProgramSettings {
            unsandboxed: mode,
            ..ProgramSettings::default()
        },
        ..RunnerSnapshot::default()
    }
}

/// Starts the `check` node of a plain run with `facts` under `mode`, with no sandbox.
fn start_with_facts(facts: RunFacts, mode: Mode) -> Result<(), Failure> {
    let root = TempDir::new().unwrap();
    let mut binding = binding(root.path(), "plain");
    fs::create_dir_all(&binding.worktree.path).unwrap();
    binding.setup.facts = facts;
    start_probe(&mut kind(), &binding, &no_python(root.path(), mode))
}

#[test]
fn unattended_and_heal_runs_refuse_whatever_the_mode() {
    let by = |unattended, started_by| RunFacts {
        unattended,
        started_by: Some(started_by),
    };
    let grant = Grant {
        at: "2026-01-02T03:04:05Z".to_string(),
        by: "person".to_string(),
    };
    let never = [
        by(true, StartedBy::Person),
        by(true, StartedBy::Lane),
        by(false, StartedBy::Healer),
        RunFacts::default(),
    ];
    for facts in never {
        for mode in [Mode::Fallback, Mode::Ask, Mode::Refuse, Mode::Allow] {
            for grant in [None, Some(&grant)] {
                assert_eq!(
                    program_permit(&facts, mode, grant),
                    Permit::Refuse,
                    "{facts:?}"
                );
            }
        }
        for mode in [Mode::Ask, Mode::Allow] {
            let failure = start_with_facts(facts, mode).unwrap_err();
            assert_eq!(
                failure.reason,
                FailureReason::SandboxRefused,
                "{facts:?} {mode:?}"
            );
        }
    }
    assert_eq!(
        program_permit(&by(false, StartedBy::Person), Mode::Ask, None),
        Permit::Ask
    );
    assert_eq!(
        program_permit(&by(false, StartedBy::Lane), Mode::Allow, None),
        Permit::Allowed(AllowedBy::Setting)
    );
    // A person's grant in the store is the permit, under `allow` too.
    for mode in [Mode::Ask, Mode::Allow] {
        assert_eq!(
            program_permit(&by(false, StartedBy::Person), mode, Some(&grant)),
            Permit::Allowed(AllowedBy::RunConsent {
                at: "2026-01-02T03:04:05Z".to_string(),
                by: "person".to_string(),
            })
        );
    }
}

#[test]
fn a_project_sandbox_switch_refuse_wins_over_the_allow_setting() {
    let person = RunFacts {
        unattended: false,
        started_by: Some(StartedBy::Person),
    };
    // Allowed by the setting alone, the start gets past the decision to the spawn.
    let failure = start_with_facts(person, Mode::Allow).unwrap_err();
    assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
    let root = TempDir::new().unwrap();
    let mut binding = binding(root.path(), "plain");
    fs::create_dir_all(&binding.worktree.path).unwrap();
    binding.setup.facts = person;
    let mut snapshot = no_python(root.path(), Mode::Allow);
    snapshot.refusing_runs.insert("run-1".to_string());
    let failure = start_probe(&mut kind(), &binding, &snapshot).unwrap_err();
    assert_eq!(failure.reason, FailureReason::SandboxRefused);
}

#[test]
fn an_attended_run_asks_and_under_fallback_runs_as_before() {
    let person = RunFacts {
        unattended: false,
        started_by: Some(StartedBy::Person),
    };
    let failure = start_with_facts(person, Mode::Ask).unwrap_err();
    assert_eq!(failure.reason, FailureReason::SandboxRequired);
    assert_eq!(failure.node, Some(NodeKey::new("check", 1)));
    // Fallback (the default) gets past the decision to the spawn, as before; so does a run whose
    // facts refuse, since fallback never consults the permit.
    for facts in [person, RunFacts::default()] {
        let failure = start_with_facts(facts, Mode::Fallback).unwrap_err();
        assert_eq!(failure.reason, FailureReason::ProgramInterpreterMissing);
    }
}
