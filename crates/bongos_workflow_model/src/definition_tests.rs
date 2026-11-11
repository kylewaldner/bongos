use crate::definition::{
    AgentKind, Condition, EndKind, FieldType, GateWhen, ItemType, JoinWait, NodeKind, Outcome,
    Target, parse,
};

const FULL: &str = include_str!("../test-fixtures/full.toml");
const UNKNOWN: &str = include_str!("../test-fixtures/unknown_keys.toml");
const MINIMAL: &str = include_str!("../test-fixtures/minimal.toml");

fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|line| line.contains(needle))
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("`{needle}` is not in the text"))
}

#[test]
fn every_top_level_key_parses() {
    let definition = parse(FULL).unwrap();
    assert_eq!(definition.schema, 1);
    assert_eq!(definition.name, "implement PR");
    assert_eq!(
        definition.description.as_deref(),
        Some("Triage, plan, fan out over work items, test and review")
    );
    assert_eq!(definition.start.value, "triage");
    assert_eq!(definition.start.line, line_of(FULL, "start = "));
    assert!(definition.unknown_keys.is_empty());
    assert!(definition.warnings().is_empty());
}

#[test]
fn inputs_and_defaults_parse() {
    let definition = parse(FULL).unwrap();
    assert_eq!(definition.inputs.len(), 2);
    let goal = &definition.inputs[0];
    assert_eq!(goal.name, "goal");
    assert_eq!(goal.from, "task.goal");
    assert!(goal.required);
    assert_eq!(goal.line, line_of(FULL, "goal = { type"));
    let plan = &definition.inputs[1];
    assert_eq!(plan.name, "plan");
    assert!(!plan.required);

    let defaults = &definition.defaults;
    assert_eq!(defaults.agent, Some(AgentKind::Claude));
    assert_eq!(defaults.model.as_deref(), Some("sonnet"));
    assert_eq!(defaults.permission_mode.as_deref(), Some("acceptEdits"));
    assert_eq!(defaults.allowed_tools.as_ref().map(Vec::len), Some(6));
    assert_eq!(defaults.max_turns, Some(40));
    assert_eq!(defaults.max_cost_usd, Some(5.0));
    assert_eq!(defaults.max_minutes, Some(30));
    assert_eq!(defaults.retries, Some(2));
}

#[test]
fn nodes_keep_file_order_and_lines() {
    let definition = parse(FULL).unwrap();
    let names = definition
        .nodes
        .iter()
        .map(|node| node.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "triage",
            "plan",
            "implement_item",
            "test",
            "fix",
            "review",
            "merge"
        ]
    );
    for node in &definition.nodes {
        assert_eq!(
            node.line,
            line_of(FULL, &format!("[nodes.{}]", node.name)),
            "{}",
            node.name
        );
    }
}

#[test]
fn agent_node_keys_parse() {
    let definition = parse(FULL).unwrap();
    let plan = definition.node("plan").unwrap();
    assert_eq!(plan.kind.value, NodeKind::Agent);
    assert_eq!(plan.kind.line, line_of(FULL, "[nodes.plan]") + 1);
    assert_eq!(plan.prompt.as_ref().unwrap().value, "plan.md");
    assert_eq!(plan.settings.agent, Some(AgentKind::Codex));
    assert_eq!(plan.settings.model.as_deref(), Some("gpt"));
    assert_eq!(plan.settings.permission_mode.as_deref(), Some("plan"));
    assert_eq!(plan.settings.allowed_tools, Some(vec!["Read".to_string()]));
    assert_eq!(plan.settings.max_turns, Some(10));
    assert_eq!(plan.settings.max_cost_usd, Some(1.5));
    assert_eq!(plan.settings.max_minutes, Some(5));
    assert_eq!(plan.settings.retries, Some(0));

    let triage = definition.node("triage").unwrap();
    assert_eq!(
        triage.purpose.as_deref(),
        Some("Decide one-shot or planned")
    );
    assert_eq!(triage.settings, Default::default());
}

#[test]
fn program_node_keys_parse() {
    let definition = parse(FULL).unwrap();
    let test = definition.node("test").unwrap();
    assert_eq!(test.kind.value, NodeKind::Program);
    assert_eq!(test.join.as_ref().unwrap().wait, JoinWait::Any);
    assert_eq!(test.script.as_ref().unwrap().value, "run_tests.py");
    assert_eq!(
        test.script.as_ref().unwrap().line,
        line_of(FULL, "script = ")
    );
    assert_eq!(test.language.as_deref(), Some("python"));
    let permissions = test.permissions.as_ref().unwrap();
    assert_eq!(permissions.commands, ["cargo"]);
    assert_eq!(permissions.network, ["github.com"]);
    assert_eq!(permissions.mcp, ["github"]);
    let polls = test.polls.as_ref().unwrap();
    assert_eq!(polls.value, "waiting");
    assert_eq!(polls.interval_minutes, Some(5));
    assert_eq!(polls.give_up_days, Some(7));

    let merge = definition.node("merge").unwrap();
    assert_eq!(merge.use_node.as_ref().unwrap().value, "merge");
    assert_eq!(merge.with["style"].as_str(), Some("ff-or-merge"));
    let gate = merge.gate.as_ref().unwrap();
    assert_eq!(gate.when, GateWhen::Before);
    assert_eq!(gate.question, "Merge now?");
    assert_eq!(gate.line, line_of(FULL, "Merge now?"));
}

#[test]
fn join_defaults_to_all() {
    let definition = parse(
        "schema = 1\nname = \"j\"\nstart = \"a\"\n[nodes.a]\nkind = \"program\"\njoin = {}\n",
    )
    .unwrap();
    assert_eq!(
        definition.node("a").unwrap().join.as_ref().unwrap().wait,
        JoinWait::All
    );
}

#[test]
fn output_fields_parse_with_types_and_lines() {
    let definition = parse(FULL).unwrap();
    let triage = definition.node("triage").unwrap();
    assert_eq!(triage.output.len(), 2);
    assert_eq!(triage.output[0].name, "mode");
    assert_eq!(
        triage.output[0].field_type,
        FieldType::Choice(vec!["one-shot".to_string(), "planned".to_string()])
    );
    assert_eq!(triage.output[0].line, line_of(FULL, "output = { mode"));
    assert_eq!(triage.output[1].field_type, FieldType::Text);
    assert!(!triage.output[1].optional);

    let plan = definition.node("plan").unwrap();
    assert_eq!(plan.output[0].field_type, FieldType::List(ItemType::Text));
    assert_eq!(plan.output[1].field_type, FieldType::Path);
    assert_eq!(plan.output[1].outcome, Some(Outcome::Document));
    assert_eq!(plan.output[2].field_type, FieldType::Number);
    assert!(plan.output[2].optional);

    let review = definition.node("review").unwrap();
    assert_eq!(review.output[1].outcome, Some(Outcome::Pr));
    assert_eq!(review.output[2].field_type, FieldType::List(ItemType::Path));
    assert_eq!(review.output[2].outcome, Some(Outcome::Tasks));
    let gate = review.gate.as_ref().unwrap();
    assert_eq!(gate.when, GateWhen::After);
    assert_eq!(gate.question, "Ship this?");
}

#[test]
fn routes_parse_every_shape() {
    let definition = parse(FULL).unwrap();

    let triage = definition.node("triage").unwrap();
    assert_eq!(triage.routes.len(), 2);
    assert_eq!(
        triage.routes[1].when,
        [Condition {
            field: "mode".to_string(),
            value: "planned".to_string()
        }]
    );
    assert_eq!(triage.routes[1].to, Some(Target::One("plan".to_string())));
    assert_eq!(triage.routes[0].line, line_of(FULL, "\"one-shot\" }"));
    assert_eq!(triage.routes[1].line, line_of(FULL, "\"planned\" }"));

    let plan = definition.node("plan").unwrap();
    let fan_out = plan.routes[0].fan_out.as_ref().unwrap();
    assert_eq!(fan_out.over, "items");
    assert_eq!(fan_out.to, "implement_item");
    assert_eq!(fan_out.width, Some(8));
    assert_eq!(plan.routes[0].to, None);

    let test = definition.node("test").unwrap();
    assert_eq!(test.routes[1].max_passes, Some(3));
    assert_eq!(test.routes[1].on_bound.as_deref(), Some("stuck"));
    assert_eq!(test.routes[0].max_passes, None);

    let fix = definition.node("fix").unwrap();
    assert_eq!(
        fix.routes[0].to,
        Some(Target::Many(vec!["test".to_string(), "review".to_string()]))
    );
    assert!(fix.routes[1].default);
    assert!(fix.routes[1].when.is_empty());

    let implement = definition.node("implement_item").unwrap();
    assert_eq!(
        implement.routes[0].to,
        Some(Target::One("test".to_string()))
    );
}

#[test]
fn ends_parse() {
    let definition = parse(FULL).unwrap();
    assert_eq!(definition.ends.len(), 2);
    let done = definition.end("done").unwrap();
    assert_eq!(done.kind, EndKind::Success);
    assert_eq!(
        done.outcomes,
        [Outcome::Pr, Outcome::Document, Outcome::Tasks]
    );
    assert_eq!(done.line, line_of(FULL, "[ends.done]"));
    let stuck = definition.end("stuck").unwrap();
    assert_eq!(stuck.kind, EndKind::Failure);
    assert_eq!(stuck.reason.as_deref(), Some("merge conflict needs a hand"));
    assert_eq!(definition.ends[1].name, "stuck");
}

#[test]
fn minimal_definition_parses() {
    let definition = parse(MINIMAL).unwrap();
    assert_eq!(definition.name, "tiny");
    assert!(definition.inputs.is_empty());
    assert_eq!(definition.defaults, Default::default());
    assert_eq!(definition.nodes.len(), 1);
    assert!(definition.nodes[0].output.is_empty());
    assert_eq!(definition.ends[0].outcomes, []);
}

#[test]
fn unknown_keys_are_warnings_with_lines() {
    let definition = parse(UNKNOWN).unwrap();
    let warnings = definition.warnings();
    let found = warnings
        .iter()
        .map(|warning| {
            (
                warning.node.as_deref(),
                warning.line,
                warning.message.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            (None, line_of(UNKNOWN, "heal = true"), "unknown key 'heal'"),
            (None, line_of(UNKNOWN, "[extras]"), "unknown key 'extras'"),
            (
                Some("work"),
                line_of(UNKNOWN, "sandbox = "),
                "unknown key 'sandbox' in 'work'"
            ),
            (
                Some("finish"),
                line_of(UNKNOWN, "flavor = "),
                "unknown key 'flavor' in 'finish'"
            ),
        ]
    );
}

#[test]
fn unknown_keys_keep_their_values() {
    let definition = parse(UNKNOWN).unwrap();
    assert_eq!(definition.unknown_keys[0].value, toml::Value::Boolean(true));
    assert_eq!(
        definition.unknown_keys[2].value,
        toml::Value::String("strict".to_string())
    );
    assert_eq!(definition.nodes.len(), 1);
}

#[test]
fn syntax_errors_carry_the_line() {
    let text = "schema = 1\nname = \"x\"\nstart = \n";
    let error = parse(text).unwrap_err();
    assert_eq!(error.line, 3);
}

#[test]
fn wrong_types_carry_the_line() {
    let text = "schema = 1\nname = \"x\"\nstart = \"a\"\n\n[nodes.a]\nkind = \"robot\"\n";
    let error = parse(text).unwrap_err();
    assert_eq!(error.line, 6);
    assert!(error.message.contains("robot"), "{}", error.message);
}

#[test]
fn unsupported_schema_is_an_error() {
    let text = "schema = 2\nname = \"x\"\nstart = \"a\"\n";
    let error = parse(text).unwrap_err();
    assert_eq!(error.line, 1);
    assert!(error.message.contains("unsupported schema 2"));
}

#[test]
fn choice_without_values_and_list_without_of_are_errors() {
    let choice = "schema = 1\nname = \"x\"\nstart = \"a\"\n\n[nodes.a]\nkind = \"agent\"\noutput = { m = { type = \"choice\" } }\n";
    let error = parse(choice).unwrap_err();
    assert_eq!(error.line, 7);
    assert!(error.message.contains("values"));

    let list = "schema = 1\nname = \"x\"\nstart = \"a\"\n\n[nodes.a]\nkind = \"agent\"\noutput = { m = { type = \"list\" } }\n";
    let error = parse(list).unwrap_err();
    assert_eq!(error.line, 7);
    assert!(error.message.contains("of"));
}
