use crate::table::{OwnerMode, TreeMode};
use crate::{
    LaneTable, ValidationContext, names_lane_instance, parse, prompt_lane_problems,
    validate_lane_table,
};

const EXAMPLE: &str = r#"schema = 1
owner = "top"
on_interruption = "ask"
dispatcher = "lanes"

[[kind]]
name = "design"
count = 2
ahead = { cap = 2 }
areas = false
rules = ["prepare"]
unattended = true

[[kind]]
name = "build"
count = 3
rules = ["implement-step", "finish"]
[kind.binding]
env = { CARGO_BUILD_JOBS = "{cpus/live_lanes}", BUILD_DIR = "{dir.target}" }
dirs = ["target", "scratch", "logs"]
trees = [{ repo = "app", mode = "persistent", base_tree = { dirs = ["target"] } }]
floors = ["target"]

[rules.prepare]
workflow = "refresh design"

[rules.finish]
use_todo_workflow = true

[rules.implement-step]
work_kind = "step"
state = ["ready", "in-progress"]
workflow = "implement step"
inputs = { step = "task.goal" }
staleness = "lanes/scripts/stale"
refresh = "refresh design"

[[pin]]
task = "0193"
lane = "build-1"

[[exempt]]
tags = ["editor", "billing"]
"#;

fn ctx() -> ValidationContext {
    ValidationContext {
        workflow_names: ["implement step", "refresh design"]
            .into_iter()
            .map(String::from)
            .collect(),
        task_ids: ["0193", "0194"].into_iter().map(String::from).collect(),
    }
}

/// Validates `text` and returns `(line, message)` pairs.
fn problems(text: &str) -> Vec<(usize, String)> {
    let table = parse(text).expect("parses");
    validate_lane_table(&table, &ctx())
        .into_iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.message))
        .collect()
}

#[track_caller]
fn assert_one(text: &str, line: usize, fragment: &str) {
    let found = problems(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, line, "{found:?}");
    assert!(found[0].1.contains(fragment), "{found:?}");
}

#[test]
fn example_parses_and_validates() {
    let table = parse(EXAMPLE).unwrap();
    assert_eq!(table.owner, OwnerMode::Top);
    assert_eq!(table.dispatcher.as_deref(), Some("lanes"));
    assert_eq!(table.kind_names(), ["design", "build"]);
    let design = &table.kinds[0];
    assert_eq!(
        (design.count, design.areas, design.unattended),
        (2, false, Some(true))
    );
    assert_eq!(design.ahead.as_ref().unwrap().cap, 2);
    assert_eq!(design.line, 6);
    let build = &table.kinds[1];
    assert_eq!(
        build
            .rules
            .iter()
            .map(|rule| rule.value.as_str())
            .collect::<Vec<_>>(),
        ["implement-step", "finish"]
    );
    let binding = build.binding.as_ref().unwrap();
    assert_eq!(binding.env.len(), 2);
    assert_eq!(binding.env[0].0, "CARGO_BUILD_JOBS");
    assert_eq!(binding.dirs.len(), 3);
    assert_eq!(binding.trees[0].mode, TreeMode::Persistent);
    assert_eq!(
        binding.trees[0].base_tree.as_ref().unwrap().dirs[0].value,
        "target"
    );
    assert_eq!(binding.floors[0].value, "target");
    assert_eq!(
        table
            .rules
            .iter()
            .map(|rule| rule.name.as_str())
            .collect::<Vec<_>>(),
        ["prepare", "finish", "implement-step"]
    );
    let step = &table.rules[2];
    assert_eq!(step.inputs[0].0, "step");
    assert_eq!(
        step.staleness.as_ref().unwrap().value,
        "lanes/scripts/stale"
    );
    assert_eq!(table.pins[0].lane.as_ref().unwrap().value, "build-1");
    assert_eq!(table.exemptions[0].tags.len(), 2);
    assert_eq!(problems(EXAMPLE), Vec::<(usize, String)>::new());
}

#[test]
fn syntax_errors_and_bad_schema_name_their_line() {
    assert_eq!(parse("schema = 1\n[[kind]\n").unwrap_err().line, 2);
    let error = parse("schema = 2\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert!(error.message.contains("unsupported schema"));
    assert!(parse("").is_err());
}

#[test]
fn unknown_keys_are_errors_at_every_level() {
    let text = "schema = 1\nbogus = 1\n[[kind]]\nname = \"a\"\nahead = { cap = 1, extra = 2 }\n\
                [kind.binding]\nwhat = 1\n[rules.r]\nnope = 1\n[[pin]]\nx = 1\n[[exempt]]\ny = 1\n";
    let found: Vec<_> = problems(text)
        .into_iter()
        .filter(|(_, message)| message.contains("unknown key"))
        .collect();
    let lines: Vec<usize> = found.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [2, 5, 7, 9, 11, 13], "{found:?}");
}

#[test]
fn not_available_fields_are_refused_by_name() {
    let text = "schema = 1\npools = []\n[[kind]]\nname = \"a\"\nhooks = []\npulls = []\n";
    let found = problems(text);
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(found[0], (2, "pools are not available yet".to_string()));
    assert_eq!(found[1], (5, "hooks are not available yet".to_string()));
    assert_eq!(found[2], (6, "pulls are not available yet".to_string()));
}

#[test]
fn registered_fields_are_accepted_and_kept_raw() {
    let text = "schema = 1\n[[kind]]\nname = \"a\"\npulls = [{ queue = \"q\" }]\n[kind.binding]\npools = [\"jobs\"]\n[kind.binding.hooks]\nbefore_job = { run = \"x\" }\n";
    let accepted = ["kind.pulls", "kind.binding.pools", "kind.binding.hooks"];
    let table = crate::parse_accepting(text, &accepted).unwrap();
    assert!(
        validate_lane_table(&table, &ValidationContext::default()).is_empty(),
        "{:?}",
        table.diagnostics
    );
    let kind = &table.kinds[0];
    let pulls = kind.raw.get("pulls").unwrap();
    assert_eq!(pulls.line, 4);
    assert_eq!(
        pulls.value.as_array().unwrap()[0]["queue"].as_str(),
        Some("q")
    );
    let binding = kind.binding.as_ref().unwrap();
    assert_eq!(binding.raw.get("pools").unwrap().line, 6);
    assert_eq!(
        binding.raw.get("hooks").unwrap().value["before_job"]["run"].as_str(),
        Some("x")
    );

    // Unregistered keys are refused as before, and their values are kept all the same.
    let only_pulls = crate::parse_accepting(text, &["kind.pulls"]).unwrap();
    let messages: Vec<&str> = only_pulls
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "unknown key \"pools\" in [kind.binding]",
            "unknown key \"hooks\" in [kind.binding]",
        ]
    );
    assert!(only_pulls.kinds[0].raw.get("pulls").is_some());
    let unregistered = parse(text).unwrap();
    assert_eq!(unregistered.diagnostics.len(), 3);
    assert_eq!(unregistered.kinds[0].raw, kind.raw);
}

#[test]
fn duplicate_kind_names_the_second() {
    assert_one(
        "schema = 1\n[[kind]]\nname = \"a\"\n[[kind]]\nname = \"a\"\n",
        4,
        "declared twice (first on line 2)",
    );
}

#[test]
fn kind_names_must_match_the_pattern() {
    assert_one(
        "schema = 1\n[[kind]]\nname = \"Build-1\"\n",
        2,
        "[a-z][a-z0-9]*",
    );
    assert_one(
        "schema = 1\n[[kind]]\nname = \"b-c\"\n",
        2,
        "[a-z][a-z0-9]*",
    );
}

#[test]
fn undeclared_rule_is_named_at_its_line() {
    assert_one(
        "schema = 1\n[[kind]]\nname = \"a\"\nrules = [\n  \"ghost\",\n]\n",
        5,
        "\"ghost\", which is not declared",
    );
}

#[test]
fn workflow_and_refresh_must_resolve() {
    let text =
        "schema = 1\n[rules.r]\nworkflow = \"nope\"\nrefresh = \"also nope\"\nstaleness = \"s\"\n";
    let found = problems(text);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].0, 3);
    assert!(found[0].1.contains("workflow \"nope\""));
    assert_eq!(found[1].0, 4);
    assert!(found[1].1.contains("refresh \"also nope\""));
}

#[test]
fn staleness_needs_refresh() {
    assert_one(
        "schema = 1\n[rules.r]\nworkflow = \"implement step\"\nstaleness = \"s\"\n",
        4,
        "staleness without refresh",
    );
}

#[test]
fn pins_and_exemptions_name_known_tasks_and_kinds() {
    let text = "schema = 1\n[[kind]]\nname = \"build\"\n\
                [[pin]]\ntask = \"9999\"\nkind = \"build\"\n\
                [[pin]]\ntask = \"0193\"\nkind = \"deploy\"\n\
                [[pin]]\ntask = \"0193\"\nlane = \"deploy-2\"\n\
                [[pin]]\ntask = \"0193\"\nlane = \"build-0\"\n\
                [[pin]]\ntask = \"0193\"\n\
                [[exempt]]\ntasks = [\"0194\", \"8888\"]\n\
                [[exempt]]\n";
    let found = problems(text);
    let lines: Vec<usize> = found.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [5, 9, 12, 15, 16, 19, 20], "{found:?}");
    assert!(found[0].1.contains("not in master"));
    assert!(found[1].1.contains("kind \"deploy\""));
    assert!(found[2].1.contains("kind \"deploy\" does not exist"));
    assert!(found[3].1.contains("<kind>-<n>"));
    assert!(found[4].1.contains("lane or a kind"));
    assert!(found[5].1.contains("8888"));
    assert!(found[6].1.contains("tags or tasks"));
}

#[test]
fn pin_may_name_a_lane_instance_of_a_declared_kind() {
    let text =
        "schema = 1\n[[kind]]\nname = \"build\"\n[[pin]]\ntask = \"0193\"\nlane = \"build-2\"\n";
    assert!(problems(text).is_empty());
}

#[test]
fn bad_count_and_cap_name_their_line() {
    let text = "schema = 1\n[[kind]]\nname = \"a\"\ncount = -1\nahead = { cap = 1.5 }\n\
                [[kind]]\nname = \"b\"\ncount = \"2\"\n[[kind]]\nname = \"c\"\ncount = 0\nahead = {}\n";
    let found = problems(text);
    let lines: Vec<usize> = found.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [4, 5, 8], "{found:?}");
    assert!(found[0].1.contains("count must be a non-negative integer"));
    assert!(found[1].1.contains("ahead.cap must be"));
    let table = parse(text).unwrap();
    assert_eq!(table.kinds[2].count, 0);
    assert_eq!(table.kinds[2].ahead.as_ref().unwrap().cap, 1);
}

#[test]
fn unknown_tokens_in_env_are_errors() {
    let text = "schema = 1\n[[kind]]\nname = \"a\"\n[kind.binding]\ndirs = [\"t\"]\n\
                env = { A = \"{nope}\", B = \"{dir.missing}\", C = \"{dir.t}\", D = \"{cpus/0}\" }\n";
    let found = problems(text);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found.iter().all(|(line, _)| *line == 6));
    assert!(
        found[0].1.contains("{nope}")
            || found[1].1.contains("{nope}")
            || found[2].1.contains("{nope}")
    );
}

#[test]
fn base_tree_dirs_must_be_bound() {
    let text = "schema = 1\n[[kind]]\nname = \"a\"\n[kind.binding]\ndirs = [\"target\"]\n\
                trees = [{ repo = \"app\", base_tree = { dirs = [\"target\", \"cache\"] } }]\n";
    assert_one(text, 6, "\"cache\", which kind \"a\" does not bind");
}

#[test]
fn bad_tree_mode_and_owner_are_errors() {
    let text = "schema = 1\nowner = \"all\"\n[[kind]]\nname = \"a\"\n[kind.binding]\n\
                trees = [{ repo = \"app\", mode = \"weird\" }]\n";
    let found = problems(text);
    assert_eq!(
        found.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        [2, 6],
        "{found:?}"
    );
}

#[test]
fn lane_instances_in_rules_and_env_are_refused() {
    let text = "schema = 1\n[[kind]]\nname = \"build\"\n[kind.binding]\nenv = { A = \"x-build-2\", B = \"build-2\" }\n\
                [rules.r]\nworkflow = \"implement step\"\ninputs = { step = \"build-2\", other = \"rebuild-2\", more = \"build-x\" }\n";
    let found = problems(text);
    let lines: Vec<usize> = found.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [5, 8], "{found:?}");
    assert!(found[0].1.contains("names lane build-2"));
    assert!(found[1].1.contains("names lane build-2"));
}

#[test]
fn instance_check_uses_word_boundaries() {
    let kinds = ["build", "design"];
    assert_eq!(
        names_lane_instance("run on build-2 now", &kinds).as_deref(),
        Some("build-2")
    );
    assert_eq!(
        names_lane_instance("(design-10).", &kinds).as_deref(),
        Some("design-10")
    );
    assert_eq!(names_lane_instance("rebuild-2", &kinds), None);
    assert_eq!(names_lane_instance("build-x", &kinds), None);
    assert_eq!(names_lane_instance("build-", &kinds), None);
    assert_eq!(names_lane_instance("build-2x", &kinds), None);
    assert_eq!(names_lane_instance("build 2", &kinds), None);
    assert_eq!(names_lane_instance("deploy-2", &kinds), None);
    assert_eq!(names_lane_instance("build-2", &[]), None);
}

#[test]
fn prompt_check_names_file_line_and_lane() {
    let text = "Do the work.\nIf stuck ask build-2 for help.\n\nrebuild-2 is fine\n";
    let found = prompt_lane_problems("prompts/work.md", text, &["build"]);
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].message,
        "prompt prompts/work.md line 2 names lane build-2; name the kind"
    );
    assert!(prompt_lane_problems("p.md", "all clear", &["build"]).is_empty());
}

#[test]
fn implicit_table_has_one_default_kind() {
    let table = LaneTable::implicit();
    assert_eq!(table.kind_names(), ["default"]);
    let kind = &table.kinds[0];
    assert_eq!((kind.count, kind.areas), (1, true));
    assert!(kind.rules.is_empty() && kind.binding.is_none() && kind.ahead.is_none());
    assert!(table.rules.is_empty());
    assert!(validate_lane_table(&table, &ValidationContext::default()).is_empty());
}

#[test]
fn a_file_never_gets_the_implicit_kind() {
    let table = parse("schema = 1\n").unwrap();
    assert!(table.kinds.is_empty());
    let table = parse("schema = 1\n[[kind]]\nname = \"build\"\n").unwrap();
    assert_eq!(table.kind_names(), ["build"]);
    assert_eq!(table.kinds[0].count, 1);
}

#[test]
fn locations_parse_shared_and_per_repository_roots() {
    let shared = parse(
        "schema = 1\n[locations]\nworktree_root = \"/mnt/fast/trees\"\ndirs_root = \"/mnt/fast/dirs\"\n",
    )
    .unwrap();
    assert!(shared.diagnostics.is_empty(), "{:?}", shared.diagnostics);
    assert_eq!(
        shared.locations.worktree_root_of("app").unwrap().value,
        "/mnt/fast/trees"
    );
    assert_eq!(shared.locations.worktree_root_of("app").unwrap().line, 3);
    assert_eq!(
        shared.locations.dirs_root.as_ref().unwrap().value,
        "/mnt/fast/dirs"
    );
    assert_eq!(shared.locations.dirs_root.as_ref().unwrap().line, 4);

    let per_repo = parse(
        "schema = 1\n[locations]\nworktree_root.app = \"/mnt/app\"\nworktree_root.lib = \"/mnt/lib\"\n",
    )
    .unwrap();
    assert!(
        per_repo.diagnostics.is_empty(),
        "{:?}",
        per_repo.diagnostics
    );
    assert_eq!(
        per_repo.locations.worktree_root_of("lib").unwrap().value,
        "/mnt/lib"
    );
    assert_eq!(
        per_repo.locations.worktree_root_of("app").unwrap().value,
        "/mnt/app"
    );
    assert_eq!(per_repo.locations.worktree_root_of("web"), None);
    assert_eq!(per_repo.locations.worktree_root_of("app").unwrap().line, 3);
    assert_eq!(per_repo.locations.worktree_root_of("lib").unwrap().line, 4);

    let none = parse("schema = 1\n").unwrap();
    assert_eq!(none.locations, Default::default());
    assert_eq!(LaneTable::implicit().locations, Default::default());
}

#[test]
fn an_unknown_or_wrongly_typed_location_is_an_error() {
    let table = parse("schema = 1\n[locations]\nbuild_root = \"/x\"\n").unwrap();
    let messages: Vec<&str> = table
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    assert_eq!(messages, ["unknown key \"build_root\" in [locations]"]);
    assert_eq!(table.diagnostics[0].line, 3);

    let error = parse("schema = 1\n[locations]\nworktree_root = 3\n").unwrap_err();
    assert_eq!(error.line, 3, "{error}");
}
