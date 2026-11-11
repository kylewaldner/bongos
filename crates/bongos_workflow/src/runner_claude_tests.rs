//! Tests of the Claude runner: argv, the capability refusal, the clean environment, stream parsing
//! into a typed result or a named failure, the engine's time cap, and a routed run through the
//! runtime. The CLI is `test-fixtures/fake-claude-headless`, which replays recorded streams.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bongos_chat::model::{Agent, Capabilities};
use bongos_chat::{AgentCommand, Health, HealthState};
use bongos_workflow_model::record::EndState;
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{
    Definition, Event, Failure, FailureReason, Limits, NodeKey, json_schema, parse,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::process::ProcessRecord;
use crate::runner_claude::{
    AgentDefaults, ClaudeCli, ClaudeNode, ClaudeRunner, ENV_NAMES, Exit, NodeCaps, PASSED_ENV,
    RESULT_FILE, classify, launch_env,
};
use crate::runtime::{Finish, RunSpec, Runner, Runtime};

const FAKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-fixtures/fake-claude-headless"
);

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-fixtures")
            .join(name),
    )
    .unwrap()
}

const TRIAGE: &str = r#"
schema = 1
name = "triage"
start = "triage"

[defaults]
model = "haiku"
permission_mode = "acceptEdits"
allowed_tools = ["Read", "Bash(cargo test:*)"]
max_turns = 3
max_cost_usd = 0.5
max_minutes = 2

[nodes.triage]
kind = "agent"
prompt = "triage.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] }, why = { type = "text" } }
routes = [
  { when = { mode = "one-shot" }, to = "implement" },
  { when = { mode = "planned" }, to = "plan" },
]

[nodes.implement]
kind = "agent"
prompt = "implement.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] }, why = { type = "text" } }
routes = [{ to = "done" }]

[nodes.plan]
kind = "agent"
prompt = "plan.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] }, why = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const SESSION: &str = "00000000-0000-4000-8000-0000000000aa";

fn definition() -> Definition {
    parse(TRIAGE).unwrap()
}

fn help_of_fake() -> (String, String) {
    let run = |arg: &str| {
        String::from_utf8(Command::new(FAKE).arg(arg).output().unwrap().stdout).unwrap()
    };
    (run("--version"), run("--help"))
}

fn fake_caps() -> Capabilities {
    let (version, help) = help_of_fake();
    Capabilities::from_help(&version, &help, &[])
}

fn ready(caps: Capabilities) -> Health {
    Health {
        agent: Agent::Claude,
        state: HealthState::Ready {
            version: caps.version().to_string(),
            login_checked: true,
        },
        fix: String::new(),
        caps: Some(caps),
    }
}

fn base_env(home: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), home.to_string_lossy().into_owned());
    env.insert("LANG".into(), "C.UTF-8".into());
    env.insert("TMPDIR".into(), home.to_string_lossy().into_owned());
    env.insert("ANTHROPIC_API_KEY".into(), "not-a-real-key".into());
    env.insert("SECRET_TOKEN".into(), "do-not-pass".into());
    env
}

fn command(extra: &[&str]) -> AgentCommand {
    AgentCommand {
        command: FAKE.to_string(),
        extra_args: extra.iter().map(|arg| arg.to_string()).collect(),
    }
}

fn cli(extra: &[&str], home: &Path) -> ClaudeCli {
    ClaudeCli::check(&command(extra), &ready(fake_caps()), &base_env(home)).unwrap()
}

fn node(definition: &Definition, name: &str, root: &Path, try_number: u32) -> ClaudeNode {
    let declared = definition.node(name).unwrap();
    let worktree = root.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let try_dir = root.join(format!("record/nodes/{name}/1-{try_number}"));
    ClaudeNode {
        caps: NodeCaps::resolve(definition, declared, &AgentDefaults::default()),
        output: declared.output.clone(),
        mcp_exclude: Vec::new(),
        prompt: format!("Triage the task ({name}).\n"),
        system: "Task context: the goal.\n".to_string(),
        worktree,
        try_dir: try_dir.clone(),
        task_id: "task-1".to_string(),
        task_dir: root.join("task"),
        node_result: try_dir.join("node-result.json"),
        session_id: SESSION.to_string(),
    }
}

fn key(name: &str) -> NodeKey {
    NodeKey::new(name, 1)
}

/// Runs one try of `triage` with the fake in `scenario` and returns how it finished.
fn run_one(scenario: &str, tweak: impl FnOnce(&mut ClaudeNode)) -> (Finish, TempDir) {
    let root = TempDir::new().unwrap();
    let log = root.path().join("log");
    let log_arg = log.to_string_lossy().into_owned();
    let mut runner = ClaudeRunner::new(
        cli(
            &["--fake-scenario", scenario, "--fake-log", &log_arg],
            root.path(),
        ),
        base_env(root.path()),
    );
    let mut claude_node = node(&definition(), "triage", root.path(), 1);
    tweak(&mut claude_node);
    runner
        .start("run-1", &key("triage"), 1, claude_node)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(finish) = runner.poll().pop() {
            return (finish, root);
        }
        assert!(Instant::now() < deadline, "the fake did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn failure(finish: &Finish) -> &Failure {
    finish.outcome.as_ref().unwrap_err()
}

fn says(failure: &Failure) -> String {
    format!("{}: {}", failure.reason, failure.detail)
}

#[test]
fn argv_for_a_node() {
    let root = TempDir::new().unwrap();
    let definition = definition();
    let cli = cli(&["--fake-scenario", "success"], root.path());
    let mut claude_node = node(&definition, "triage", root.path(), 1);
    claude_node.mcp_exclude = vec!["github".to_string()];
    let system = Path::new("/record/system.md");
    let schema = json_schema(&definition.node("triage").unwrap().output).to_string();
    let expected: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "haiku",
        "--permission-mode",
        "acceptEdits",
        "--allowedTools",
        "Read",
        "Bash(cargo test:*)",
        "--disallowedTools",
        "mcp__github__*",
        "--max-turns",
        "3",
        "--max-budget-usd",
        "0.5",
        "--json-schema",
        &schema,
        "--append-system-prompt-file",
        "/record/system.md",
        "--session-id",
        SESSION,
        "--permission-prompts",
        "none",
        "--fake-scenario",
        "success",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    assert_eq!(cli.args(&claude_node, system), expected);

    // A CLI without the optional flags gets neither; the cost cap is then checked at the end.
    let caps = Capabilities::from_names(
        "2.1.322",
        &[
            "--print",
            "--output-format",
            "--verbose",
            "--model",
            "--permission-mode",
            "--allowedTools",
            "--json-schema",
            "--session-id",
        ],
        &[],
    );
    let bare = ClaudeCli::check(&command(&[]), &ready(caps), &base_env(root.path())).unwrap();
    claude_node.mcp_exclude.clear();
    let args = bare.args(&claude_node, system);
    assert!(!args.contains(&"--max-budget-usd".to_string()));
    assert!(!args.contains(&"--permission-prompts".to_string()));
    assert!(!args.contains(&"--disallowedTools".to_string()));
}

#[test]
fn node_caps_take_the_node_over_defaults_over_settings() {
    let definition = parse(
        r#"
schema = 1
name = "caps"
start = "a"

[defaults]
max_turns = 10

[nodes.a]
kind = "agent"
model = "claude-opus-5-5"
allowed_tools = ["Read", "mcp__github__*", "mcp__linear__create_issue"]
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#,
    )
    .unwrap();
    let caps = NodeCaps::resolve(
        &definition,
        definition.node("a").unwrap(),
        &AgentDefaults::default(),
    );
    assert_eq!(caps.model, "claude-opus-5-5");
    assert_eq!(caps.max_turns, 10);
    assert_eq!(caps.max_cost_usd, 5.0);
    assert_eq!(caps.permission_mode, "acceptEdits");
    assert_eq!(caps.max_time, Duration::from_secs(30 * 60));
    assert_eq!(caps.mcp_servers(), vec!["github", "linear"]);
}

#[test]
fn a_missing_flag_refuses_the_start_with_fix_text() {
    let root = TempDir::new().unwrap();
    let env = base_env(root.path());
    let (version, help) = help_of_fake();
    let old_help: String = help
        .lines()
        .filter(|line| !line.contains("--json-schema"))
        .collect::<Vec<_>>()
        .join("\n");
    let caps = Capabilities::from_help(&version, &old_help, &[]);
    let refusal = ClaudeCli::check(&command(&[]), &ready(caps), &env).unwrap_err();
    assert_eq!(refusal.reason, FailureReason::AgentNotInstalledOrLoggedOut);
    assert_eq!(
        refusal.detail,
        "Claude Code 2.1.322 lacks --json-schema; update it"
    );

    // `--max-turns` is hidden from `--help`; it counts only from the version it was checked on.
    let older = Capabilities::from_help("2.1.319 (Claude Code)", &help, &[]);
    let refusal = ClaudeCli::check(&command(&[]), &ready(older), &env).unwrap_err();
    assert_eq!(
        refusal.detail,
        "Claude Code 2.1.319 lacks --max-turns; update it"
    );

    // Not installed, logged out and not checked yet refuse with 04's fix text.
    let mut health = ready(fake_caps());
    health.state = HealthState::NotFound;
    let refusal = ClaudeCli::check(&command(&[]), &health, &env).unwrap_err();
    assert_eq!(refusal.reason, FailureReason::AgentNotInstalledOrLoggedOut);
    assert!(
        refusal.detail.contains("was not found"),
        "{}",
        refusal.detail
    );
    health.state = HealthState::LoggedOut;
    let refusal = ClaudeCli::check(&command(&[]), &health, &env).unwrap_err();
    assert!(refusal.detail.contains("log in"), "{}", refusal.detail);
    let unchecked = Health {
        state: HealthState::Unchecked,
        caps: None,
        ..ready(fake_caps())
    };
    let refusal = ClaudeCli::check(&command(&[]), &unchecked, &env).unwrap_err();
    assert!(refusal.detail.contains("not checked"), "{}", refusal.detail);

    // A missing program is refused even when health had caps.
    let gone = AgentCommand {
        command: root.path().join("no-claude").to_string_lossy().into_owned(),
        extra_args: Vec::new(),
    };
    let refusal = ClaudeCli::check(&gone, &ready(fake_caps()), &env).unwrap_err();
    assert!(
        refusal.detail.contains("was not found"),
        "{}",
        refusal.detail
    );
}

#[test]
fn a_valid_result_is_typed_and_recorded() {
    let (finish, root) = run_one("success", |_| {});
    assert_eq!(
        finish.outcome,
        Ok(json!({ "mode": "one-shot", "why": "trivial" }))
    );
    assert!(finish.cost_usd > 0.0);
    assert_eq!(finish.turns, Some(3));
    assert_eq!(
        finish.session_id.as_deref(),
        Some("00000000-0000-4000-8000-000100000001")
    );
    let recorded: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.path()
                .join("record/nodes/triage/1-1")
                .join(RESULT_FILE),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(recorded, json!({ "mode": "one-shot", "why": "trivial" }));

    // The prompt went in on stdin and the system text through its file.
    let log = root.path().join("log");
    assert_eq!(
        std::fs::read_to_string(log.join("stdin")).unwrap(),
        "Triage the task (triage).\n"
    );
    let argv: Vec<String> = std::fs::read_to_string(log.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let at = argv
        .iter()
        .position(|arg| arg == "--append-system-prompt-file")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&argv[at + 1]).unwrap(),
        "Task context: the goal.\n"
    );
}

#[test]
fn an_invalid_or_missing_result_fails_without_reading_the_text() {
    let (finish, _root) = run_one("invalid", |_| {});
    let invalid = failure(&finish);
    assert_eq!(invalid.reason, FailureReason::NodeOutputInvalid);
    assert!(invalid.detail.contains("mode"), "{}", invalid.detail);

    // The answer's text holds a valid result, but only `structured_output` counts.
    let (finish, root) = run_one("missing", |_| {});
    let missing = failure(&finish);
    assert_eq!(missing.reason, FailureReason::NodeOutputInvalid);
    assert_eq!(missing.detail, "the agent returned no structured result");
    assert!(
        !root
            .path()
            .join("record/nodes/triage/1-1")
            .join(RESULT_FILE)
            .exists()
    );

    // No result event at all, with a clean exit.
    let node = node(&definition(), "triage", root.path(), 2);
    let none = classify(
        &key("triage"),
        &node,
        "claude",
        true,
        "not json\n",
        "",
        Exit::Code(0),
    );
    assert_eq!(
        none.outcome.unwrap_err().reason,
        FailureReason::NodeOutputInvalid
    );
}

#[test]
fn each_cap_names_itself() {
    let (finish, _root) = run_one("max-turns", |_| {});
    assert_eq!(failure(&finish).reason, FailureReason::TurnCapHit);
    assert_eq!(failure(&finish).reason.label(), "turn cap hit");
    assert_eq!(finish.turns, Some(2));

    let (finish, _root) = run_one("max-budget", |_| {});
    assert_eq!(failure(&finish).reason, FailureReason::CostCapHit);
    assert_eq!(failure(&finish).reason.label(), "cost cap hit");

    let started = Instant::now();
    let (finish, _root) = run_one("sleep", |node| {
        node.caps.max_time = Duration::from_millis(300);
    });
    assert_eq!(failure(&finish).reason, FailureReason::TimeCapHit);
    assert_eq!(failure(&finish).reason.label(), "time cap hit");
    assert!(started.elapsed() < Duration::from_secs(10));

    // Without --max-budget-usd the cost cap is checked when the node ends.
    let root = TempDir::new().unwrap();
    let mut node = node(&definition(), "triage", root.path(), 1);
    node.caps.max_cost_usd = 0.001;
    let stream = fixture("claude-success.jsonl");
    let after = classify(
        &key("triage"),
        &node,
        "claude",
        false,
        &stream,
        "",
        Exit::Code(0),
    );
    assert_eq!(after.outcome.unwrap_err().reason, FailureReason::CostCapHit);
    let enforced = classify(
        &key("triage"),
        &node,
        "claude",
        true,
        &stream,
        "",
        Exit::Code(0),
    );
    assert!(enforced.outcome.is_ok());
}

#[test]
fn a_denial_fails_naming_the_tool() {
    let (finish, _root) = run_one("permission-denied", |_| {});
    let denied = failure(&finish);
    assert_eq!(denied.reason, FailureReason::PermissionDenied);
    assert_eq!(says(denied), "permission denied: Bash");
}

#[test]
fn an_allowed_mcp_server_that_failed_at_init_fails_the_node() {
    // The recorded init event reports the server `broken` as failed.
    let (finish, _root) = run_one("success", |node| {
        node.caps.allowed_tools.push("mcp__broken__*".to_string());
    });
    assert_eq!(says(failure(&finish)), "mcp server unavailable: broken");

    // A failed server the node does not allow does not matter.
    let (finish, _root) = run_one("success", |node| {
        node.caps.allowed_tools.push("mcp__github__*".to_string());
    });
    assert!(finish.outcome.is_ok());
}

#[test]
fn a_crash_and_a_logout_name_themselves() {
    let (finish, _root) = run_one("crash", |_| {});
    assert_eq!(
        says(failure(&finish)),
        "agent exited nonzero: exit code 3: boom: the fake crashed"
    );
    let (finish, _root) = run_one("logged-out", |_| {});
    assert_eq!(
        failure(&finish).reason,
        FailureReason::AgentNotInstalledOrLoggedOut
    );
}

#[test]
fn the_environment_holds_only_the_named_variables() {
    let root = TempDir::new().unwrap();
    let node = node(&definition(), "triage", root.path(), 1);
    let env = launch_env(&base_env(root.path()), "run-1", &key("triage"), &node);
    let names: BTreeSet<&str> = env.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = PASSED_ENV.iter().chain(ENV_NAMES).copied().collect();
    assert_eq!(names, expected);
    assert_eq!(env["BONGOS_RUN_ID"], "run-1");
    assert_eq!(env["BONGOS_RUN_NODE"], "triage#1");
    assert_eq!(env["BONGOS_TASK_ID"], "task-1");

    // What the process sees, minus what `sh` sets for itself.
    let (_finish, root) = run_one("success", |_| {});
    let seen: BTreeSet<String> = std::fs::read_to_string(root.path().join("log/env"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_string()))
        .filter(|name| !matches!(name.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "_"))
        .collect();
    let expected: BTreeSet<String> = expected.iter().map(|name| name.to_string()).collect();
    assert_eq!(seen, expected);
}

#[test]
fn a_stopped_try_is_never_reported() {
    let root = TempDir::new().unwrap();
    let mut runner = ClaudeRunner::new(
        cli(&["--fake-scenario", "sleep"], root.path()),
        base_env(root.path()),
    );
    let record = runner
        .start(
            "run-1",
            &key("triage"),
            1,
            node(&definition(), "triage", root.path(), 1),
        )
        .unwrap();
    assert!(record.pid > 0);
    runner.stop(&[("run-1".to_string(), key("triage"))]);
    assert!(runner.in_flight().is_empty());
    assert!(runner.poll().is_empty());
}

/// The engine's runner for these tests: every node is a Claude node.
struct ClaudeOnly {
    claude: ClaudeRunner,
    root: PathBuf,
}

impl Runner for ClaudeOnly {
    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        let node = node(definition, &key.node, &self.root, try_number);
        self.claude.start(run, key, try_number, node)
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.claude.poll()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        self.claude.stop(tries);
    }

    fn merge_branch(&mut self, _: &str, _: &str, _: &Branch) -> Result<String, Failure> {
        Ok("0000000".to_string())
    }
}

#[test]
fn a_valid_result_routes_through_the_runtime() {
    let root = TempDir::new().unwrap();
    let runner = ClaudeOnly {
        claude: ClaudeRunner::new(
            cli(&["--fake-scenario", "success"], root.path()),
            base_env(root.path()),
        ),
        root: root.path().to_path_buf(),
    };
    let mut runtime = Runtime::new(Limits::default(), runner);
    let id = "01890a5d-ac96-774b-bcce-b30200000001".to_string();
    runtime
        .add(RunSpec {
            id: id.clone(),
            definition: definition(),
            attempt_dir: root.path().join("runs/1/attempts/1"),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while runtime.state(&id).unwrap().ended.is_none() {
        runtime.tick();
        assert!(Instant::now() < deadline, "the run did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        runtime.state(&id).unwrap().ended.map(|(state, _)| state),
        Some(EndState::Succeeded)
    );
    // `mode = "one-shot"` routed triage to implement; plan never ran.
    let events = runtime.events(&id).unwrap();
    let started: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            Event::NodeStarted { key, .. } => Some(key.node.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec!["triage", "implement"]);
    let finished: Vec<(String, f64, Option<String>)> = events
        .iter()
        .filter_map(|event| match event {
            Event::NodeFinished {
                key,
                cost_usd,
                session_id,
                ..
            } => Some((key.node.clone(), *cost_usd, session_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2);
    assert!(
        finished
            .iter()
            .all(|(_, cost, session)| *cost > 0.0 && session.is_some())
    );
}
