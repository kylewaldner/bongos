use std::collections::{BTreeMap, BTreeSet};

use crate::definition::{AgentKind, parse};
use crate::validate::{
    Ceilings, Diagnostic, Severity, ValidationContext, has_errors, strongly_connected, validate,
};

const VALID: &str = include_str!("../test-fixtures/valid.toml");

/// `implement_item`'s output and routes.
const IMPLEMENT_ROUTES: &str =
    "output = { summary = { type = \"text\" } }\nroutes = [{ to = \"test\" }]";
/// `fix`'s output and routes.
const FIX_ROUTES: &str = "output = { notes = { type = \"text\" } }\nroutes = [{ to = \"test\" }]";

const FILES: &[&str] = &[
    "triage.md",
    "plan.md",
    "implement_item.md",
    "run_tests.py",
    "fix.md",
    "review.md",
];

fn context() -> ValidationContext {
    ValidationContext {
        files: FILES.iter().map(|file| file.to_string()).collect(),
        mcp_servers: BTreeMap::from([
            (AgentKind::Claude, BTreeSet::from(["github".to_string()])),
            (AgentKind::Codex, BTreeSet::from(["linear".to_string()])),
        ]),
        ..ValidationContext::default()
    }
}

fn line_of(text: &str, needle: &str) -> usize {
    let lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(index, _)| index + 1)
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "`{needle}` must be on exactly one line");
    lines[0]
}

/// The valid fixture with `from` replaced by `to` exactly once.
fn mutate(from: &str, to: &str) -> String {
    assert_eq!(VALID.matches(from).count(), 1, "`{from}` must occur once");
    VALID.replacen(from, to, 1)
}

fn diagnostics_with(text: &str, context: &ValidationContext) -> Vec<Diagnostic> {
    validate(&parse(text).unwrap(), context)
}

fn diagnostics(text: &str) -> Vec<Diagnostic> {
    diagnostics_with(text, &context())
}

fn render(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts a diagnostic of `severity` for `node` on the line holding `line_needle` whose message
/// contains `message`.
fn assert_found(
    diagnostics: &[Diagnostic],
    text: &str,
    severity: Severity,
    node: Option<&str>,
    line_needle: &str,
    message: &str,
) {
    let line = line_of(text, line_needle);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == severity
                && diagnostic.node.as_deref() == node
                && diagnostic.line == line
                && diagnostic.message.contains(message)
        }),
        "expected {severity:?} for {node:?} on line {line} containing `{message}`, got:\n{}",
        render(diagnostics)
    );
}

fn assert_error(text: &str, node: Option<&str>, line_needle: &str, message: &str) {
    assert_found(
        &diagnostics(text),
        text,
        Severity::Error,
        node,
        line_needle,
        message,
    );
}

fn assert_error_with(
    text: &str,
    context: &ValidationContext,
    node: Option<&str>,
    line_needle: &str,
    message: &str,
) {
    assert_found(
        &diagnostics_with(text, context),
        text,
        Severity::Error,
        node,
        line_needle,
        message,
    );
}

fn assert_no_errors(text: &str) {
    let diagnostics = diagnostics(text);
    assert!(!has_errors(&diagnostics), "{}", render(&diagnostics));
}

#[test]
fn the_valid_definition_has_no_diagnostics() {
    let diagnostics = diagnostics(VALID);
    assert!(diagnostics.is_empty(), "{}", render(&diagnostics));
}

#[test]
fn unknown_route_target() {
    let text = mutate(
        r#"{ when = { status = "pass" }, to = "review" }"#,
        r#"{ when = { status = "pass" }, to = "reviw" }"#,
    );
    assert_error(
        &text,
        Some("test"),
        r#"to = "reviw""#,
        "route names unknown node or end 'reviw'",
    );
}

#[test]
fn unknown_start() {
    let text = mutate(r#"start = "triage""#, r#"start = "triag""#);
    assert_error(&text, None, "start = ", "start names unknown node 'triag'");
}

#[test]
fn start_at_an_end() {
    let text = mutate(r#"start = "triage""#, r#"start = "done""#);
    assert_error(
        &text,
        None,
        "start = ",
        "start must be a node, not end 'done'",
    );
}

#[test]
fn unknown_on_bound() {
    let text = mutate(r#"on_bound = "stuck""#, r#"on_bound = "stuk""#);
    assert_error(
        &text,
        Some("test"),
        "on_bound = ",
        "on_bound names unknown node or end 'stuk'",
    );
}

#[test]
fn on_bound_without_max_passes() {
    let text = mutate(
        r#"to = "fix", max_passes = 3, on_bound = "stuck""#,
        r#"to = "fix", on_bound = "stuck""#,
    );
    assert_error(
        &text,
        Some("test"),
        "on_bound = ",
        "`on_bound` needs `max_passes`",
    );
}

#[test]
fn unknown_fan_out_target() {
    let text = mutate(
        r#"to = "implement_item", width"#,
        r#"to = "implement", width"#,
    );
    assert_error(
        &text,
        Some("plan"),
        "fan_out = ",
        "fan_out.to names unknown node 'implement'",
    );
}

#[test]
fn fan_out_to_an_end() {
    let text = mutate(r#"to = "implement_item", width"#, r#"to = "done", width"#);
    assert_error(
        &text,
        Some("plan"),
        "fan_out = ",
        "fan_out.to 'done' is an end",
    );
}

#[test]
fn route_without_target() {
    let text = mutate(
        FIX_ROUTES,
        "output = { notes = { type = \"text\" } }\nroutes = [{ max_passes = 2 }]",
    );
    assert_error(
        &text,
        Some("fix"),
        "routes = [{ max_passes",
        "a route needs `to` or `fan_out`",
    );
}

#[test]
fn missing_prompt_and_script_files() {
    let mut context = context();
    context.files.remove("plan.md");
    context.files.remove("run_tests.py");
    assert_error_with(
        VALID,
        &context,
        Some("plan"),
        r#"prompt = "plan.md""#,
        "prompt file 'plan.md' is missing",
    );
    assert_error_with(
        VALID,
        &context,
        Some("test"),
        "script = ",
        "script file 'run_tests.py' is missing",
    );
}

#[test]
fn prompt_files_are_matched_after_normalizing() {
    let text = mutate(r#"prompt = "plan.md""#, r#"prompt = "./plan.md""#);
    assert_no_errors(&text);
}

#[test]
fn prompt_outside_the_definition_dir() {
    let text = mutate(r#"prompt = "plan.md""#, r#"prompt = "../plan.md""#);
    assert_error(
        &text,
        Some("plan"),
        "prompt = \"../plan.md\"",
        "must be a relative path inside the definition directory",
    );
    let text = mutate(r#"prompt = "plan.md""#, r#"prompt = "/etc/plan.md""#);
    assert_error(
        &text,
        Some("plan"),
        "prompt = \"/etc/plan.md\"",
        "must be a relative path inside the definition directory",
    );
}

#[test]
fn missing_definition_dir() {
    let context = ValidationContext {
        dir_exists: false,
        ..context()
    };
    let text = VALID.to_string();
    assert_error_with(
        &text,
        &context,
        None,
        "schema = 1",
        "the definition directory is missing",
    );
}

#[test]
fn use_naming_no_shipped_program() {
    let text = mutate(r#"use = "merge""#, r#"use = "mrege""#);
    assert_error(
        &text,
        Some("merge"),
        "use = ",
        "'mrege' is not a shipped program node (shipped: merge, wait)",
    );
}

#[test]
fn node_kinds_need_their_body() {
    let text = mutate(r#"prompt = "fix.md""#, "");
    assert_error(
        &text,
        Some("fix"),
        "[nodes.fix]",
        "an agent node needs a `prompt`",
    );

    let text = mutate(r#"use = "merge""#, "");
    assert_error(
        &text,
        Some("merge"),
        "[nodes.merge]",
        "a program node needs `script` or `use`",
    );

    let text = mutate(
        r#"use = "merge""#,
        "use = \"merge\"\nscript = \"run_tests.py\"",
    );
    assert_error(
        &text,
        Some("merge"),
        "use = ",
        "a program node takes `script` or `use`, not both",
    );
}

#[test]
fn undeclared_routed_field() {
    let text = mutate(r#"{ mode = "one-shot" }"#, r#"{ mood = "one-shot" }"#);
    assert_error(
        &text,
        Some("triage"),
        "mood = ",
        "route on undeclared field 'mood'",
    );
}

#[test]
fn when_value_outside_the_choice() {
    let text = mutate(r#"{ mode = "one-shot" }"#, r#"{ mode = "oneshot" }"#);
    assert_error(
        &text,
        Some("triage"),
        r#"mode = "oneshot""#,
        "'oneshot' is not a value of choice field 'mode' (one-shot, planned)",
    );
    // The real value now has no route either.
    assert_error(
        &text,
        Some("triage"),
        "output = { mode",
        "no route for 'one-shot'",
    );
}

#[test]
fn choice_value_without_route() {
    let text = mutate(
        "  { when = { mode = \"planned\" }, to = \"plan\" },\n",
        "  { when = { mode = \"one-shot\" }, to = \"plan\" },\n",
    );
    assert_error(
        &text,
        Some("triage"),
        "output = { mode",
        "no route for 'planned' (field 'mode') and no default route",
    );
}

#[test]
fn a_default_route_covers_every_value() {
    let text = mutate(
        "  { when = { mode = \"planned\" }, to = \"plan\" },\n",
        "  { default = true, to = \"plan\" },\n",
    );
    assert_no_errors(&text);
}

#[test]
fn coverage_over_two_routed_choices() {
    let text = mutate(
        r#"output = { status = { type = "choice", values = ["pass", "fail"] } }"#,
        r#"output = { status = { type = "choice", values = ["pass", "fail"] }, flaky = { type = "choice", values = ["yes", "no"] } }"#,
    );
    let text = text.replacen(
        r#"{ when = { status = "pass" }, to = "review" }"#,
        r#"{ when = { status = "pass", flaky = "no" }, to = "review" }"#,
        1,
    );
    assert_error(
        &text,
        Some("test"),
        "flaky = { type",
        "no route for status = 'pass', flaky = 'yes' and no default route",
    );
}

#[test]
fn fan_out_over_must_be_a_list() {
    let text = mutate(r#"over = "items""#, r#"over = "design""#);
    assert_error(
        &text,
        Some("plan"),
        "fan_out = ",
        "fan_out.over field 'design' is not a list",
    );
    let text = mutate(r#"over = "items""#, r#"over = "things""#);
    assert_error(
        &text,
        Some("plan"),
        "fan_out = ",
        "fan_out.over names undeclared field 'things'",
    );
}

#[test]
fn fan_out_width_within_the_setting() {
    let text = mutate("width = 8", "width = 9");
    assert_error(
        &text,
        Some("plan"),
        "fan_out = ",
        "fan_out.width 9 is above the limit 8",
    );
}

#[test]
fn unreachable_node() {
    let text = format!(
        "{VALID}\n[nodes.orphan]\nkind = \"program\"\nuse = \"wait\"\nroutes = [{{ to = \"done\" }}]\n"
    );
    assert_error(
        &text,
        Some("orphan"),
        "[nodes.orphan]",
        "node 'orphan' is unreachable from start 'triage'",
    );
}

#[test]
fn node_that_cannot_reach_an_end() {
    let text = mutate(IMPLEMENT_ROUTES, "routes = []");
    assert_error(
        &text,
        Some("implement_item"),
        "[nodes.implement_item]",
        "node 'implement_item' cannot reach any end",
    );
}

#[test]
fn no_success_end() {
    let text = mutate("kind = \"success\"", "kind = \"failure\"");
    assert_error(&text, None, "start = ", "no success end");
}

#[test]
fn success_end_not_reachable() {
    let text = mutate(
        r#"{ when = { result = "merged" }, to = "done" }"#,
        r#"{ when = { result = "merged" }, to = "stuck" }"#,
    );
    assert_error(
        &text,
        None,
        "start = ",
        "no success end is reachable from start",
    );
}

#[test]
fn promised_outcome_no_path_declares() {
    let text = mutate(r#"outcomes = ["pr"]"#, r#"outcomes = ["pr", "tasks"]"#);
    assert_error(
        &text,
        Some("done"),
        "[ends.done]",
        "end 'done' promises outcome 'tasks' but no node on a path to it declares",
    );
    // `document` is declared by `plan`, which is on one path to `done`.
    let text = mutate(r#"outcomes = ["pr"]"#, r#"outcomes = ["pr", "document"]"#);
    assert_no_errors(&text);
}

#[test]
fn cycle_without_a_bounded_edge() {
    let text = mutate(
        r#"to = "fix", max_passes = 3, on_bound = "stuck""#,
        r#"to = "fix""#,
    );
    assert_error(
        &text,
        Some("test"),
        r#"{ when = { status = "fail" }, to = "fix" }"#,
        "cycle with no bounded edge through test, fix",
    );
}

#[test]
fn cycle_with_a_bounded_edge_is_valid() {
    // The fixture loops test -> fix -> test and review -> fix -> test -> review, each through a
    // bounded edge.
    assert_no_errors(VALID);
}

#[test]
fn self_loop_without_a_bound() {
    let text = mutate(IMPLEMENT_ROUTES, r#"routes = [{ to = "implement_item" }]"#);
    assert_error(
        &text,
        Some("implement_item"),
        r#"routes = [{ to = "implement_item" }]"#,
        "cycle with no bounded edge through implement_item",
    );
}

#[test]
fn max_passes_ceiling() {
    let text = mutate(
        r#"to = "fix", max_passes = 3, on_bound"#,
        r#"to = "fix", max_passes = 21, on_bound"#,
    );
    assert_error(
        &text,
        Some("test"),
        "max_passes = 21",
        "max_passes 21 is above the ceiling 20",
    );
    let text = mutate(
        r#"to = "fix", max_passes = 3, on_bound"#,
        r#"to = "fix", max_passes = 0, on_bound"#,
    );
    assert_error(
        &text,
        Some("test"),
        "max_passes = 0",
        "max_passes must be at least 1",
    );
}

#[test]
fn branch_that_neither_joins_nor_ends() {
    let text = mutate(IMPLEMENT_ROUTES, "routes = []");
    assert_error(
        &text,
        Some("implement_item"),
        "[nodes.implement_item]",
        "in a branch of fan-out 'plan', node 'implement_item' reaches no join or end",
    );
}

#[test]
fn branch_that_ends_without_a_join_is_valid() {
    let text = r#"schema = 1
name = "branches end"
start = "a"

[nodes.a]
kind = "agent"
prompt = "a.md"
output = { items = { type = "list", of = "path" } }
routes = [{ fan_out = { over = "items", to = "b" } }]

[nodes.b]
kind = "agent"
prompt = "b.md"
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;
    let context = ValidationContext {
        files: BTreeSet::from(["a.md".to_string(), "b.md".to_string()]),
        ..ValidationContext::default()
    };
    let diagnostics = diagnostics_with(text, &context);
    assert!(diagnostics.is_empty(), "{}", render(&diagnostics));
}

#[test]
fn join_no_fan_out_feeds() {
    let text = mutate(r#"use = "merge""#, "use = \"merge\"\njoin = {}");
    assert_error(
        &text,
        Some("merge"),
        "join = {}",
        "the join on 'merge' is fed by no fan-out",
    );
}

#[test]
fn nested_fan_out_is_refused() {
    let text = mutate(
        IMPLEMENT_ROUTES,
        "output = { parts = { type = \"list\", of = \"text\" } }\nroutes = [{ fan_out = { over = \"parts\", to = \"fix\" } }]",
    );
    assert_error(
        &text,
        Some("implement_item"),
        r#"over = "parts""#,
        "a fan-out inside a branch of fan-out 'plan' is not supported in v1",
    );
}

#[test]
fn bounded_edge_leaving_a_branch_is_refused() {
    let text = mutate(
        IMPLEMENT_ROUTES,
        "output = { again = { type = \"choice\", values = [\"yes\", \"no\"] } }\nroutes = [\n  { when = { again = \"yes\" }, to = \"plan\", max_passes = 2 },\n  { when = { again = \"no\" }, to = \"test\" },\n]",
    );
    assert_error(
        &text,
        Some("implement_item"),
        r#"to = "plan", max_passes = 2"#,
        "a bounded edge from a branch of fan-out 'plan' to 'plan' outside it is not supported in v1",
    );
}

#[test]
fn fixed_fan_out_feeds_a_join() {
    let text = mutate(
        r#"routes = [{ fan_out = { over = "items", to = "implement_item", width = 8 } }]"#,
        r#"routes = [{ to = ["implement_item", "fix"] }]"#,
    );
    assert_no_errors(&text);
    let text = mutate(
        r#"routes = [{ fan_out = { over = "items", to = "implement_item", width = 8 } }]"#,
        r#"routes = [{ to = ["implement_item", "done"] }]"#,
    );
    assert_error(
        &text,
        Some("plan"),
        "routes = [{ to = [",
        "fan-out branch 'done' is an end",
    );
}

#[test]
fn gate_with_an_empty_question() {
    let text = mutate(r#"question = "Ship this?""#, r#"question = "  ""#);
    assert_error(
        &text,
        Some("review"),
        "gate = ",
        "the gate has an empty question",
    );
}

#[test]
fn retries_ceiling() {
    let text = mutate("retries = 2", "retries = 6");
    assert_error(
        &text,
        Some("triage"),
        "retries = 6",
        "retries 6 (from [defaults]) is above the ceiling 5",
    );
    // Program nodes inherit retries too.
    assert_error(
        &text,
        Some("merge"),
        "retries = 6",
        "retries 6 (from [defaults]) is above the ceiling 5",
    );
    let text = mutate(r#"prompt = "fix.md""#, "prompt = \"fix.md\"\nretries = 9");
    assert_error(
        &text,
        Some("fix"),
        "retries = 9",
        "retries 9 is above the ceiling 5",
    );
}

#[test]
fn caps_ceilings() {
    let text = mutate("max_turns = 40", "max_turns = 201");
    assert_error(
        &text,
        Some("plan"),
        "max_turns = 201",
        "max_turns 201 (from [defaults]) is above the ceiling 200",
    );
    let text = mutate(
        r#"prompt = "fix.md""#,
        "prompt = \"fix.md\"\nmax_cost_usd = 25.5",
    );
    assert_error(
        &text,
        Some("fix"),
        "max_cost_usd = 25.5",
        "max_cost_usd 25.5 is above the ceiling 25",
    );
    let text = mutate(
        r#"prompt = "fix.md""#,
        "prompt = \"fix.md\"\nmax_minutes = 0",
    );
    assert_error(
        &text,
        Some("fix"),
        "max_minutes = 0",
        "max_minutes must be above zero",
    );
    // Program nodes have their own wall-time ceiling.
    let text = mutate(
        r#"script = "run_tests.py""#,
        "script = \"run_tests.py\"\nmax_minutes = 121",
    );
    assert_error(
        &text,
        Some("test"),
        "max_minutes = 121",
        "max_minutes 121 is above the ceiling 120 (workflows.ceilings.program_max_minutes)",
    );
    let raised = ValidationContext {
        ceilings: Ceilings {
            max_turns: 300,
            ..Ceilings::default()
        },
        ..context()
    };
    let text = mutate("max_turns = 40", "max_turns = 201");
    assert!(!has_errors(&diagnostics_with(&text, &raised)));
}

#[test]
fn permission_mode_ceiling() {
    let text = mutate(
        r#"permission_mode = "acceptEdits""#,
        r#"permission_mode = "bypassPermissions""#,
    );
    assert_error(
        &text,
        Some("triage"),
        "permission_mode = ",
        "permission mode 'bypassPermissions' (from [defaults]) is not allowed",
    );
    let allowed = ValidationContext {
        ceilings: Ceilings {
            allow_bypass_permissions: true,
            ..Ceilings::default()
        },
        ..context()
    };
    assert!(!has_errors(&diagnostics_with(&text, &allowed)));

    let text = mutate(
        r#"permission_mode = "acceptEdits""#,
        r#"permission_mode = "yolo""#,
    );
    assert_error(
        &text,
        Some("triage"),
        "permission_mode = ",
        "unknown permission mode 'yolo'",
    );
}

#[test]
fn mcp_servers_must_be_installed_for_the_agent() {
    let text = mutate(
        r#""mcp__github__*"]"#,
        r#""mcp__github__*", "mcp__sentry__issues"]"#,
    );
    assert_error(
        &text,
        Some("triage"),
        "allowed_tools = ",
        "MCP server 'sentry' (from [defaults]) is not installed for claude; install it with \
         `claude mcp add`, or remove it from allowed_tools",
    );

    let text = mutate(
        r#"prompt = "fix.md""#,
        "prompt = \"fix.md\"\nallowed_tools = [\n  \"Read\",\n  \"mcp__github__create_issue\",\n]",
    );
    assert_no_errors(&text);
}

#[test]
fn bare_mcp_server_is_refused() {
    let text = mutate(
        r#"prompt = "fix.md""#,
        "prompt = \"fix.md\"\nallowed_tools = [\n  \"Read\",\n  \"mcp__github\",\n]",
    );
    assert_error(
        &text,
        Some("fix"),
        "\"mcp__github\",",
        "'mcp__github' names a server, not a tool; use mcp__github__<tool> or mcp__github__*",
    );
}

#[test]
fn codex_mcp_and_bash_patterns() {
    let text = mutate(
        r#"prompt = "fix.md""#,
        "prompt = \"fix.md\"\nagent = \"codex\"\nallowed_tools = [\n  \"Bash(git status:*)\",\n  \"mcp__linear__*\",\n  \"mcp__linear__search\",\n  \"mcp__github__*\",\n]",
    );
    let diagnostics = diagnostics(&text);
    assert_found(
        &diagnostics,
        &text,
        Severity::Warning,
        Some("fix"),
        "\"Bash(git status:*)\",",
        "codex does not limit commands by pattern",
    );
    assert_found(
        &diagnostics,
        &text,
        Severity::Error,
        Some("fix"),
        "\"mcp__linear__search\",",
        "codex limits MCP by server; use mcp__linear__*",
    );
    // github is installed for claude only.
    assert_found(
        &diagnostics,
        &text,
        Severity::Error,
        Some("fix"),
        "\"mcp__github__*\",",
        "MCP server 'github' is not installed for codex; install it with `codex mcp add`",
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.line == line_of(&text, "\"mcp__linear__*\",")),
        "{}",
        render(&diagnostics)
    );
}

#[test]
fn program_permissions_ceiling() {
    let text = mutate("network = []", r#"network = ["crates.io"]"#);
    assert_no_errors(&text);
    let closed = ValidationContext {
        allow_network: false,
        ..context()
    };
    assert_error_with(
        &text,
        &closed,
        Some("test"),
        "permissions = ",
        "network permission (crates.io) is not allowed: workflows.programs.allow_network is false",
    );

    let text = mutate("mcp = []", r#"mcp = ["github"]"#);
    assert_error(
        &text,
        Some("test"),
        "permissions = ",
        "MCP from program nodes needs the bongos MCP server (feature 21)",
    );
}

#[test]
fn node_names_are_branch_and_path_safe() {
    for bad in ["Fix", "fix me", "fix/me", "-fix", "fix.me", "fïx"] {
        let header = format!("[nodes.\"{bad}\"]");
        let text = format!("{VALID}\n{header}\nkind = \"agent\"\nprompt = \"fix.md\"\n");
        assert_error(
            &text,
            Some(bad),
            &header,
            &format!("node name '{bad}' may use only lowercase letters, digits, '-' and '_'"),
        );
    }
    let diagnostics = diagnostics(VALID);
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("may use only")),
        "{}",
        render(&diagnostics)
    );
}

#[test]
fn node_and_end_share_a_name() {
    let text = format!("{VALID}\n[ends.fix]\nkind = \"failure\"\n");
    assert_error(
        &text,
        Some("fix"),
        "[ends.fix]",
        "'fix' is both a node and an end",
    );
}

#[test]
fn unknown_keys_stay_warnings() {
    let text = mutate(r#"prompt = "fix.md""#, "prompt = \"fix.md\"\nheal = true");
    let diagnostics = diagnostics(&text);
    assert!(!has_errors(&diagnostics), "{}", render(&diagnostics));
    assert_found(
        &diagnostics,
        &text,
        Severity::Warning,
        Some("fix"),
        "heal = true",
        "unknown key 'heal' in 'fix'",
    );
}

#[test]
fn every_diagnostic_names_a_line_and_sorts_by_it() {
    let text = mutate(r#"start = "triage""#, r#"start = "nowhere""#)
        .replacen(r#"to = "review" }"#, r#"to = "reviw" }"#, 1)
        .replacen(r#"question = "Ship this?""#, r#"question = """#, 1);
    let diagnostics = diagnostics(&text);
    assert!(diagnostics.len() >= 3, "{}", render(&diagnostics));
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.line > 0));
    assert!(
        diagnostics
            .windows(2)
            .all(|pair| pair[0].line <= pair[1].line)
    );
}

#[test]
fn tarjan_finds_components() {
    // 0 -> 1 -> 2 -> 0, 2 -> 3, 3 -> 3, 4 alone.
    let adjacency = vec![vec![1], vec![2], vec![0, 3], vec![3], vec![]];
    let mut components = strongly_connected(&adjacency)
        .into_iter()
        .map(|mut component| {
            component.sort_unstable();
            component
        })
        .collect::<Vec<_>>();
    components.sort();
    assert_eq!(components, vec![vec![0, 1, 2], vec![3], vec![4]]);
}

#[test]
fn permission_modes_are_the_ones_claude_accepts() {
    assert_eq!(
        crate::validate::PERMISSION_MODES,
        [
            "acceptEdits",
            "auto",
            "bypassPermissions",
            "manual",
            "dontAsk",
            "plan"
        ]
    );
    // `default` was replaced by `manual`.
    let text = mutate(
        r#"permission_mode = "acceptEdits""#,
        r#"permission_mode = "default""#,
    );
    assert_error(
        &text,
        Some("triage"),
        "permission_mode = ",
        "unknown permission mode 'default'",
    );
    for mode in ["auto", "manual", "dontAsk", "plan"] {
        let text = mutate(
            r#"permission_mode = "acceptEdits""#,
            &format!("permission_mode = \"{mode}\""),
        );
        assert!(!has_errors(&diagnostics(&text)), "{mode}");
    }
}
