//! Tests of the agent node kind: what a node start puts on the Claude CLI's command line and
//! stdin, run against `test-fixtures/fake-claude-headless`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use bongos_chat::model::{Agent, Capabilities};
use bongos_chat::{AgentCommand, Health, HealthState};
use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::record::ClosedGate;
use bongos_workflow_model::{Failure, FailureReason, NodeKey, RunState, parse};
use serde_json::json;
use tempfile::TempDir;

use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::dispatch_agent::AgentNodes;
use crate::dispatch_tests::binding;
use crate::runner_claude::{AgentDefaults, ClaudeCli};
use crate::runtime::{NodeStart, RunBinding};

const FAKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-fixtures/fake-claude-headless"
);

const FLOW: &str = r#"
schema = 1
name = "flow"
start = "triage"

[defaults]
model = "haiku"
allowed_tools = ["Read", "mcp__github__*"]
max_turns = 3
max_cost_usd = 0.5

[nodes.triage]
kind = "agent"
prompt = "triage.md"
output = { why = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

// A test reads the fake's `--version` and `--help` once; blocking the thread is fine here.
#[allow(clippy::disallowed_methods)]
fn fake_cli(log: &Path, home: &Path) -> Result<ClaudeCli, Failure> {
    let run = |arg: &str| {
        String::from_utf8(Command::new(FAKE).arg(arg).output().unwrap().stdout).unwrap()
    };
    let caps = Capabilities::from_help(&run("--version"), &run("--help"), &[]);
    let health = Health {
        agent: Agent::Claude,
        state: HealthState::Ready {
            version: caps.version().to_string(),
            login_checked: true,
        },
        fix: String::new(),
        caps: Some(caps),
        chat_caps: None,
    };
    ClaudeCli::check(
        &AgentCommand {
            command: FAKE.to_string(),
            extra_args: vec!["--fake-log".to_string(), log.to_string_lossy().into_owned()],
        },
        &health,
        &env(home),
    )
}

fn env(home: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
    ])
}

struct World {
    root: TempDir,
    binding: RunBinding,
    snapshot: RunnerSnapshot,
}

fn world() -> World {
    let root = TempDir::new().unwrap();
    let binding = binding(root.path(), "flow");
    fs::create_dir_all(binding.task_dir.as_path()).unwrap();
    fs::write(binding.task_dir.join("task.toml"), "schema = 1\nid = \"task\"\ngoal = \"Add login\"\nkind = \"other\"\nstatus = \"open\"\ncreated = 2026-01-01T00:00:00Z\n").unwrap();
    fs::create_dir_all(&binding.worktree.path).unwrap();
    let prompts = binding.attempt_dir.join("definition/0");
    fs::create_dir_all(&prompts).unwrap();
    fs::write(prompts.join("triage.md"), "Triage {{inputs.topic}}.\n").unwrap();
    let mut binding = binding;
    binding.inputs.push(bongos_workflow_model::inputs::Input {
        name: "topic".to_string(),
        value: Some(bongos_workflow_model::inputs::InputValue::Text(
            "the login bug".to_string(),
        )),
        source: "literal:the login bug".to_string(),
        missing: false,
    });
    let snapshot = RunnerSnapshot {
        agents: AgentDefaults::default(),
        claude: Some(fake_cli(&root.path().join("log"), root.path())),
        mcp_installed: BTreeMap::from([(
            AgentKind::Claude,
            BTreeSet::from(["github".to_string()]),
        )]),
        mcp_exclude: Vec::new(),
        base_env: env(root.path()),
        ..RunnerSnapshot::default()
    };
    World {
        root,
        binding,
        snapshot,
    }
}

/// Starts `triage` and waits for it to end; the failure of the start, or the argv and stdin the
/// fake logged.
fn run(
    world: &World,
    state: &RunState,
    budget_left_usd: Option<f64>,
) -> Result<(Vec<String>, String), Failure> {
    let definition = parse(FLOW).unwrap();
    let key = NodeKey::new("triage", 1);
    let mut kind = AgentNodes::default();
    kind.start(
        &NodeStart {
            run: "run-1",
            definition: &definition,
            key: &key,
            try_number: 1,
            state,
            budget_left_usd,
            binding: Some(&world.binding),
        },
        &world.binding,
        &world.snapshot,
    )?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while kind.poll().is_empty() {
        assert!(Instant::now() < deadline, "the fake did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = world.root.path().join("log");
    let argv = fs::read_to_string(log.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    Ok((argv, fs::read_to_string(log.join("stdin")).unwrap()))
}

fn after<'a>(argv: &'a [String], flag: &str) -> &'a str {
    let at = argv.iter().position(|arg| arg == flag).unwrap();
    &argv[at + 1]
}

#[test]
fn a_node_gets_its_rendered_prompt_and_caps() {
    let world = world();
    let (argv, stdin) = run(&world, &RunState::default(), None).unwrap();
    assert_eq!(stdin, "Triage the login bug.\n");
    assert_eq!(after(&argv, "--model"), "haiku");
    assert_eq!(after(&argv, "--max-turns"), "3");
    assert_eq!(after(&argv, "--max-budget-usd"), "0.5");
    let system = fs::read_to_string(after(&argv, "--append-system-prompt-file")).unwrap();
    assert!(system.contains("Goal: Add login"), "{system}");
    // The try's record directory follows the record layout.
    assert!(
        world
            .binding
            .attempt_dir
            .join("nodes/triage/1-1/prompt.md")
            .is_file()
    );
}

#[test]
fn a_revision_comment_reaches_the_prompt() {
    let world = world();
    let key = NodeKey::new("triage", 1);
    let mut state = RunState::default();
    state.closed_gates.push(ClosedGate {
        key,
        by: "person".to_string(),
        edit: None,
        comment: Some("name the file".to_string()),
    });
    let (_, stdin) = run(&world, &state, None).unwrap();
    assert!(stdin.starts_with("Triage the login bug."), "{stdin}");
    assert!(
        stdin.contains("Revision requested:\nname the file"),
        "{stdin}"
    );
}

#[test]
fn an_earlier_result_fills_a_node_name() {
    let mut world = world();
    let prompts = world.binding.attempt_dir.join("definition/0");
    fs::write(prompts.join("triage.md"), "{{nodes.plan.why}}\n").unwrap();
    world.snapshot.mcp_exclude = Vec::new();
    let mut state = RunState::default();
    state
        .finished
        .push(bongos_workflow_model::record::FinishedNode {
            key: NodeKey::new("plan", 1),
            try_number: 1,
            result: Some(json!({ "why": "small" })),
            failure: None,
            cost_usd: 0.0,
            turns: None,
            elapsed_ms: 0,
            session_id: None,
            had_start: true,
        });
    let (_, stdin) = run(&world, &state, None).unwrap();
    assert_eq!(stdin, "small\n");
}

#[test]
fn a_missing_allowed_server_fails_naming_it() {
    let mut world = world();
    world.snapshot.mcp_installed.clear();
    let failure = run(&world, &RunState::default(), None).unwrap_err();
    assert_eq!(failure.reason, FailureReason::McpServerUnavailable);
    assert!(failure.detail.contains("github"), "{}", failure.detail);
}

#[test]
fn the_run_budget_lowers_the_cost_cap() {
    let world = world();
    let (argv, _) = run(&world, &RunState::default(), Some(0.25)).unwrap();
    assert_eq!(after(&argv, "--max-budget-usd"), "0.25");
}

#[test]
fn a_failed_cli_check_fails_the_node_with_its_failure() {
    let mut world = world();
    let check = Failure::new(
        FailureReason::AgentNotInstalledOrLoggedOut,
        None,
        "install Claude Code",
    );
    world.snapshot.claude = Some(Err(check));
    let failure = run(&world, &RunState::default(), None).unwrap_err();
    assert_eq!(failure.reason, FailureReason::AgentNotInstalledOrLoggedOut);
    assert_eq!(failure.detail, "install Claude Code");
    assert_eq!(failure.node, Some(NodeKey::new("triage", 1)));
}
