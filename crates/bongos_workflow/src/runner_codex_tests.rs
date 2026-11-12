//! Tests of the Codex runner: argv per permission mode, the output schema file, the capability
//! refusal, stream parsing into a typed result or a named failure, the turn and time caps, the
//! MCP checks at start and the clean environment. The CLI is `test-fixtures/fake-codex-exec`,
//! which replays event streams written from the assumed shape of `codex exec --json` (Codex is not
//! installed where this was written).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use bongos_chat::model::{Agent, Capabilities};
use bongos_chat::{AgentCommand, Health, HealthState};
use bongos_workflow_model::{
    Definition, Failure, FailureReason, NodeKey, json_schema_strict, parse,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::runner_claude::{AgentDefaults, ENV_NAMES, NodeCaps, PASSED_ENV};
use crate::runner_codex::{
    CodexCli, CodexNode, CodexRunner, Exit, RESULT_FILE, SCHEMA_FILE, classify, launch_env,
    own_model, sandbox_for, stdin_text,
};
use crate::runtime::Finish;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fixtures/fake-codex-exec");

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
agent = "codex"
permission_mode = "acceptEdits"
allowed_tools = ["mcp__linear__*"]
max_turns = 3
max_minutes = 2

[nodes.triage]
kind = "agent"
prompt = "triage.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] }, why = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn definition() -> Definition {
    parse(TRIAGE).unwrap()
}

// A test reads the fake's help texts once; blocking the thread is fine here.
#[allow(clippy::disallowed_methods)]
fn output_of_fake(args: &[&str]) -> String {
    String::from_utf8(Command::new(FAKE).args(args).output().unwrap().stdout).unwrap()
}

fn fake_caps() -> Capabilities {
    Capabilities::from_help(
        &output_of_fake(&["--version"]),
        &output_of_fake(&["--help"]),
        &[],
    )
}

fn exec_help() -> String {
    output_of_fake(&["exec", "--help"])
}

fn ready(caps: Capabilities) -> Health {
    Health {
        agent: Agent::Codex,
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
    env.insert("OPENAI_API_KEY".into(), "not-a-real-key".into());
    env.insert("SECRET_TOKEN".into(), "do-not-pass".into());
    env
}

fn command(extra: &[&str]) -> AgentCommand {
    AgentCommand {
        command: FAKE.to_string(),
        extra_args: extra.iter().map(|arg| arg.to_string()).collect(),
    }
}

fn cli(extra: &[&str], home: &Path) -> CodexCli {
    CodexCli::check(
        &command(extra),
        &ready(fake_caps()),
        &exec_help(),
        &base_env(home),
    )
    .unwrap()
}

fn node(definition: &Definition, root: &Path) -> CodexNode {
    let declared = definition.node("triage").unwrap();
    let worktree = root.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let try_dir = root.join("record/nodes/triage/1-1");
    CodexNode {
        caps: NodeCaps::resolve(definition, declared, &AgentDefaults::default()),
        model: own_model(definition, declared),
        output: declared.output.clone(),
        mcp_exclude: Vec::new(),
        mcp_installed: ["broken", "github", "linear"]
            .iter()
            .map(|server| server.to_string())
            .collect(),
        prompt: "Triage the task.\n".to_string(),
        system: "Task context: the goal.\n".to_string(),
        worktree,
        try_dir: try_dir.clone(),
        task_id: "task-1".to_string(),
        task_dir: root.join("task"),
        node_result: try_dir.join("node-result.json"),
    }
}

fn key() -> NodeKey {
    NodeKey::new("triage", 1)
}

/// Runs one try of `triage` with the fake in `scenario` and returns how it finished.
fn run_one(scenario: &str, tweak: impl FnOnce(&mut CodexNode)) -> (Finish, TempDir) {
    let root = TempDir::new().unwrap();
    let log = root.path().join("log");
    let log_arg = log.to_string_lossy().into_owned();
    let mut runner = CodexRunner::new(
        cli(
            &["--fake-scenario", scenario, "--fake-log", &log_arg],
            root.path(),
        ),
        base_env(root.path()),
    );
    let mut codex_node = node(&definition(), root.path());
    tweak(&mut codex_node);
    runner.start("run-1", &key(), 1, codex_node).unwrap();
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

fn argv_of(root: &TempDir) -> Vec<String> {
    std::fs::read_to_string(root.path().join("log/argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn argv_per_permission_mode() {
    let root = TempDir::new().unwrap();
    let definition = definition();
    let cli = cli(&["--fake-scenario", "success"], root.path());
    let mut codex_node = node(&definition, root.path());
    let schema = Path::new("/record/output-schema.json");
    let worktree = codex_node.worktree.to_string_lossy().into_owned();

    let expected = |sandbox: &str, model: &[&str], disabled: &[&str]| -> Vec<String> {
        let mut args: Vec<String> = [
            "exec",
            "--json",
            "--output-schema",
            "/record/output-schema.json",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.extend(model.iter().map(|arg| arg.to_string()));
        args.extend(["--sandbox", sandbox, "-C", &worktree].map(String::from));
        for server in disabled {
            args.push("-c".to_string());
            args.push(format!("mcp_servers.{server}.enabled=false"));
        }
        args.extend(["--fake-scenario", "success", "-"].map(String::from));
        args
    };

    // `broken` and `github` are installed but not named, so they are off; `linear` is allowed.
    let off = ["broken", "github"];
    for (mode, sandbox) in [
        ("plan", "read-only"),
        ("acceptEdits", "workspace-write"),
        ("auto", "workspace-write"),
        ("default", "workspace-write"),
        ("manual", "workspace-write"),
        ("dontAsk", "workspace-write"),
        ("bypassPermissions", "danger-full-access"),
    ] {
        codex_node.caps.permission_mode = mode.to_string();
        assert_eq!(
            cli.args(&codex_node, schema).unwrap(),
            expected(sandbox, &[], &off),
            "mode {mode}"
        );
        assert_eq!(sandbox_for(mode), Ok(sandbox));
    }

    // A model the node names is passed; unknown modes have no sandbox.
    codex_node.caps.permission_mode = "plan".to_string();
    codex_node.model = Some("example-model".to_string());
    assert_eq!(
        cli.args(&codex_node, schema).unwrap(),
        expected("read-only", &["-m", "example-model"], &off)
    );
    codex_node.caps.permission_mode = "yolo".to_string();
    assert_eq!(
        cli.args(&codex_node, schema).unwrap_err(),
        "permission mode 'yolo' has no Codex sandbox"
    );
}

#[test]
fn a_model_comes_only_from_the_node_or_its_defaults() {
    let definition = definition();
    assert_eq!(
        own_model(&definition, definition.node("triage").unwrap()),
        None
    );
    let named = parse(&TRIAGE.replace("max_turns = 3", "max_turns = 3\nmodel = \"example-model\""))
        .unwrap();
    assert_eq!(
        own_model(&named, named.node("triage").unwrap()).as_deref(),
        Some("example-model")
    );
}

#[test]
fn an_excluded_server_is_disabled_even_when_the_node_names_it() {
    let root = TempDir::new().unwrap();
    let cli = cli(&[], root.path());
    let mut codex_node = node(&definition(), root.path());
    codex_node.caps.allowed_tools.clear();
    codex_node.mcp_exclude = vec!["linear".to_string()];
    let args = cli.args(&codex_node, Path::new("/schema.json")).unwrap();
    let overrides: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "-c")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        overrides,
        vec![
            "mcp_servers.broken.enabled=false",
            "mcp_servers.github.enabled=false",
            "mcp_servers.linear.enabled=false",
        ]
    );
}

#[test]
fn the_schema_file_requires_every_property_for_strict_mode() {
    let (finish, root) = run_one("success", |node| {
        node.output = parse(&TRIAGE.replace(
            r#"why = { type = "text" }"#,
            r#"why = { type = "text", optional = true }"#,
        ))
        .unwrap()
        .node("triage")
        .unwrap()
        .output
        .clone();
    });
    assert!(finish.outcome.is_ok());
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.path()
                .join("record/nodes/triage/1-1")
                .join(SCHEMA_FILE),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(written["required"], json!(["mode", "why"]));
    assert_eq!(
        written["properties"]["why"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn the_schema_file_holds_the_declared_output() {
    let (finish, root) = run_one("success", |_| {});
    assert!(finish.outcome.is_ok());
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.path()
                .join("record/nodes/triage/1-1")
                .join(SCHEMA_FILE),
        )
        .unwrap(),
    )
    .unwrap();
    let declared = definition();
    assert_eq!(
        written,
        json_schema_strict(&declared.node("triage").unwrap().output)
    );
    assert_eq!(written["additionalProperties"], json!(false));
    assert_eq!(
        written["properties"]["mode"]["enum"],
        json!(["one-shot", "planned"])
    );
    // The path the process was given is that file.
    let argv = argv_of(&root);
    let at = argv
        .iter()
        .position(|arg| arg == "--output-schema")
        .unwrap();
    assert_eq!(
        Path::new(&argv[at + 1]),
        root.path()
            .join("record/nodes/triage/1-1")
            .join(SCHEMA_FILE)
    );
}

#[test]
fn a_valid_result_is_typed_and_recorded() {
    let (finish, root) = run_one("success", |_| {});
    assert_eq!(
        finish.outcome,
        Ok(json!({ "mode": "one-shot", "why": "trivial" }))
    );
    // Codex reports no cost; one turn completed.
    assert_eq!(finish.cost_usd, 0.0);
    assert_eq!(finish.turns, Some(1));
    assert_eq!(
        finish.session_id.as_deref(),
        Some("00000000-0000-4000-8000-000200000001")
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

    // The task context and the prompt went in on stdin; the process ran in the worktree.
    assert_eq!(
        std::fs::read_to_string(root.path().join("log/stdin")).unwrap(),
        "Task context: the goal.\n\n---\n\nTriage the task.\n"
    );
    let argv = argv_of(&root);
    assert_eq!(argv.first().map(String::as_str), Some("exec"));
    assert_eq!(argv.last().map(String::as_str), Some("-"));
    let mut node = node(&definition(), root.path());
    assert_eq!(
        stdin_text(&node),
        "Task context: the goal.\n\n---\n\nTriage the task.\n"
    );
    node.system.clear();
    assert_eq!(stdin_text(&node), "Triage the task.\n");
}

#[test]
fn an_invalid_result_or_free_text_fails_node_output_invalid() {
    let (finish, _root) = run_one("invalid", |_| {});
    let invalid = failure(&finish);
    assert_eq!(invalid.reason, FailureReason::NodeOutputInvalid);
    assert!(invalid.detail.contains("mode"), "{}", invalid.detail);

    // The final message is prose: no fallback to reading it.
    let (finish, root) = run_one("free-text", |_| {});
    assert_eq!(
        says(failure(&finish)),
        "node output invalid: the final message is not JSON"
    );
    assert!(
        !root
            .path()
            .join("record/nodes/triage/1-1")
            .join(RESULT_FILE)
            .exists()
    );

    // No final message at all, with a clean exit.
    let root = TempDir::new().unwrap();
    let node = node(&definition(), root.path());
    let none = classify(
        &key(),
        &node,
        "codex",
        "{\"type\":\"turn.started\"}\n",
        "",
        Exit::Code(0),
    );
    assert_eq!(
        says(&none.outcome.unwrap_err()),
        "node output invalid: the agent ended without a final message"
    );
}

#[test]
fn the_turn_cap_stops_the_process_and_names_itself() {
    // The fake completes three turns, starts a fourth and sleeps; the cap is three.
    let started = Instant::now();
    let (finish, _root) = run_one("turns", |_| {});
    assert_eq!(says(failure(&finish)), "turn cap hit: max_turns 3");
    assert_eq!(finish.turns, Some(3));
    assert!(started.elapsed() < Duration::from_secs(15));

    // After the fact: more turns completed than the cap allows.
    let root = TempDir::new().unwrap();
    let mut node = node(&definition(), root.path());
    node.caps.max_turns = 0;
    let after = classify(
        &key(),
        &node,
        "codex",
        &fixture("codex-success.jsonl"),
        "",
        Exit::Code(0),
    );
    assert_eq!(after.outcome.unwrap_err().reason, FailureReason::TurnCapHit);
}

#[test]
fn the_time_cap_stops_the_process() {
    let started = Instant::now();
    let (finish, _root) = run_one("sleep", |node| {
        node.caps.max_time = Duration::from_millis(300);
    });
    assert_eq!(failure(&finish).reason, FailureReason::TimeCapHit);
    assert_eq!(failure(&finish).reason.label(), "time cap hit");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_transient_error_event_before_a_completed_turn_does_not_fail_the_try() {
    let root = TempDir::new().unwrap();
    let node = node(&definition(), root.path());
    let transient = classify(
        &key(),
        &node,
        "codex",
        &fixture("codex-transient-error.jsonl"),
        "",
        Exit::Code(0),
    );
    assert!(transient.outcome.is_ok(), "{:?}", transient.outcome);

    // The same error with no completed turn after it is terminal.
    let terminal = fixture("codex-transient-error.jsonl")
        .lines()
        .filter(|line| !line.contains("turn.completed"))
        .collect::<Vec<_>>()
        .join("\n");
    let failed = classify(&key(), &node, "codex", &terminal, "", Exit::Code(0));
    assert!(says(&failed.outcome.unwrap_err()).contains("Reconnecting"));

    // A turn.failed is fatal even when an earlier turn completed.
    let with_failed = format!(
        "{}{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"boom\"}}}}\n",
        fixture("codex-transient-error.jsonl")
    );
    let failed = classify(&key(), &node, "codex", &with_failed, "", Exit::Code(0));
    assert!(says(&failed.outcome.unwrap_err()).contains("boom"));
}

#[test]
fn failures_name_themselves() {
    // An allowed server that failed to start; a failed server the node does not name is ignored.
    let (finish, _root) = run_one("mcp-failed", |node| {
        node.caps.allowed_tools.push("mcp__broken__*".to_string());
    });
    assert_eq!(says(failure(&finish)), "mcp server unavailable: broken");
    let (finish, _root) = run_one("mcp-failed", |_| {});
    assert!(finish.outcome.is_ok());

    let (finish, _root) = run_one("logged-out", |_| {});
    assert_eq!(
        failure(&finish).reason,
        FailureReason::AgentNotInstalledOrLoggedOut
    );
    let (finish, _root) = run_one("crash", |_| {});
    assert_eq!(
        says(failure(&finish)),
        "agent exited nonzero: exit code 3: boom: the fake crashed"
    );
}

#[test]
fn an_uninstalled_or_single_tool_mcp_entry_refuses_the_start() {
    let root = TempDir::new().unwrap();
    let mut runner = CodexRunner::new(cli(&[], root.path()), base_env(root.path()));
    let mut codex_node = node(&definition(), root.path());
    codex_node.caps.allowed_tools = vec!["mcp__sentry__*".to_string()];
    let refusal = runner
        .start("run-1", &key(), 1, codex_node.clone())
        .unwrap_err();
    assert_eq!(refusal.reason, FailureReason::McpServerUnavailable);
    assert!(
        refusal
            .detail
            .starts_with("sentry: MCP server not installed for codex")
    );

    codex_node.caps.allowed_tools = vec!["mcp__linear__search".to_string()];
    let refusal = runner
        .start("run-1", &key(), 1, codex_node.clone())
        .unwrap_err();
    assert!(
        refusal.detail.contains("codex limits MCP by server"),
        "{}",
        refusal.detail
    );

    codex_node.caps.allowed_tools = vec!["mcp__linear__*".to_string()];
    codex_node.mcp_exclude = vec!["linear".to_string()];
    let refusal = runner
        .start("run-1", &key(), 1, codex_node.clone())
        .unwrap_err();
    assert!(
        refusal.detail.starts_with("linear: excluded"),
        "{}",
        refusal.detail
    );

    // A mode with no sandbox is refused before anything runs.
    codex_node.mcp_exclude.clear();
    codex_node.caps.permission_mode = "yolo".to_string();
    let refusal = runner.start("run-1", &key(), 1, codex_node).unwrap_err();
    assert_eq!(refusal.reason, FailureReason::PermissionDenied);
    assert!(runner.in_flight().is_empty());
}

#[test]
fn a_missing_flag_refuses_the_start_with_fix_text() {
    let root = TempDir::new().unwrap();
    let env = base_env(root.path());
    let help = exec_help();
    let old_help: String = help
        .lines()
        .filter(|line| !line.contains("--output-schema"))
        .collect::<Vec<_>>()
        .join("\n");
    let refusal = CodexCli::check(&command(&[]), &ready(fake_caps()), &old_help, &env).unwrap_err();
    assert_eq!(refusal.reason, FailureReason::AgentNotInstalledOrLoggedOut);
    assert_eq!(
        refusal.detail,
        "Codex CLI 0.46.0 lacks --output-schema; update it"
    );

    // No `exec` command in the top-level help.
    let caps = Capabilities::from_names("0.1.0", &[], &["login"]);
    let refusal = CodexCli::check(&command(&[]), &ready(caps), &help, &env).unwrap_err();
    assert_eq!(
        refusal.detail,
        "Codex CLI 0.1.0 has no `exec` command; update it"
    );

    // Not installed, logged out, not checked yet and a missing program.
    let mut health = ready(fake_caps());
    health.state = HealthState::NotFound;
    let refusal = CodexCli::check(&command(&[]), &health, &help, &env).unwrap_err();
    assert!(
        refusal.detail.contains("was not found"),
        "{}",
        refusal.detail
    );
    health.state = HealthState::LoggedOut;
    let refusal = CodexCli::check(&command(&[]), &health, &help, &env).unwrap_err();
    assert!(refusal.detail.contains("log in"), "{}", refusal.detail);
    let unchecked = Health {
        state: HealthState::Unchecked,
        caps: None,
        ..ready(fake_caps())
    };
    let refusal = CodexCli::check(&command(&[]), &unchecked, &help, &env).unwrap_err();
    assert!(refusal.detail.contains("not checked"), "{}", refusal.detail);
    let gone = AgentCommand {
        command: root.path().join("no-codex").to_string_lossy().into_owned(),
        extra_args: Vec::new(),
    };
    let refusal = CodexCli::check(&gone, &ready(fake_caps()), &help, &env).unwrap_err();
    assert!(
        refusal.detail.contains("was not found"),
        "{}",
        refusal.detail
    );

    // The fake has the `mcp` command, so discovery may use `mcp list`.
    assert!(cli(&[], root.path()).has_mcp());
}

#[test]
fn the_environment_holds_only_the_named_variables() {
    let root = TempDir::new().unwrap();
    let node = node(&definition(), root.path());
    let env = launch_env(&base_env(root.path()), "run-1", &key(), &node);
    let names: BTreeSet<&str> = env.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = PASSED_ENV.iter().chain(ENV_NAMES).copied().collect();
    assert_eq!(names, expected);
    assert_eq!(env["BONGOS_RUN_NODE"], "triage#1");

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
    let mut runner = CodexRunner::new(
        cli(&["--fake-scenario", "sleep"], root.path()),
        base_env(root.path()),
    );
    let record = runner
        .start("run-1", &key(), 1, node(&definition(), root.path()))
        .unwrap();
    assert!(record.pid > 0);
    assert_eq!(runner.in_flight(), vec![("run-1".to_string(), key())]);
    runner.stop(&[("run-1".to_string(), key())]);
    assert!(runner.in_flight().is_empty());
    assert!(runner.poll().is_empty());
}
