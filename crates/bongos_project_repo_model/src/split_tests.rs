use bongos_work_model::fixtures::{
    activity, after, badged, done, group, leaf, pr_ship, stacked_on, with_created,
};
use bongos_work_model::store::Task;
use bongos_work_model::{
    Kind, Status, TaskFile, TodoRole, TodoState, UnblockPoint, parse_task, serialize_task,
};

use crate::{RESERVED_STATE_KEYS, TaskDefinition, join, reserved_keys_in, split};

const FULL: &str = r#"
schema = 1
id = "0196f1c2-aaaa-7bbb-8ccc-ddddeeeeffff"
goal = "Backfill usage events"
kind = "pr-ship"
status = "done"
created = 2026-11-08T10:00:00Z
closed = 2026-11-09T12:00:00Z
repository = "invoicing"
work_kind = "migration"
areas = ["billing", "api"]
size = 3
repositories = ["invoicing", "billing-api"]
merge_is_ship = false
default_visibility = "human-only"
activity = "support"
linear = "SEQ-412"
branch = "backfill-usage-events"
base = "main"
base_sha = "4be1"
rebase_ack_sha = "9ac0"
worktree = "/w/backfill-usage-events"
ship_overridden = { at = 2026-11-09T12:00:00Z, parent_state = "review" }
after = [{ task = "schema-change", unblock = "merged" }]
stacked_on = { task = "backfill", unblock = "implemented" }
planned_result = "a PR"
checks = { required = ["fmt"] }
future_key = "kept"

[[todos]]
id = "0196f1c2-1111-7000-8000-000000000001"
name = "implement"
role = "produces-branch"
workflow = "implement"
state = "done"
started = 2026-11-08T10:05:00Z
note = "todo extra"

[[todos]]
id = "0196f1c2-1111-7000-8000-000000000002"
name = "ship"
role = "ships"
state = "open"

[result]
kind = "pr"
url = "https://github.com/acme/invoicing/pull/412"
state = "approved"

[[proposals]]
number = 1
state = "applied"
at = 2026-11-08T11:00:00Z
by = "agent"
hash = "abc"

[linear_state]
issue_id = "i1"
team_id = "t1"

[[history]]
at = 2026-11-08T10:05:00Z
by = "person"
event = "todo done"
detail = "implement"
"#;

/// The old format: no todo ids and none of the definition fields added later.
const OLD: &str = r#"
schema = 1
id = "0196f1c2-0000-7000-8000-000000000001"
goal = "Old"
kind = "other"
status = "open"
created = 2026-11-08T10:00:00Z
default_visibility = "shared"

[[todos]]
name = "first"
state = "done"
started = 2026-11-08T10:05:00Z

[[todos]]
name = "second"
state = "skipped"
"#;

/// Every task file the work model's fixtures build, plus the text fixtures above.
fn fixture_files() -> Vec<TaskFile> {
    let mut tasks = vec![
        pr_ship("alpha"),
        leaf("done-leaf", Status::Done),
        leaf("cancelled-leaf", Status::Cancelled),
        leaf("open-leaf", Status::Open),
        group("g", vec![leaf("child", Status::Open)]),
        with_created(leaf("dated", Status::Open), "2026-01-02T03:04:05Z"),
        after(leaf("blocked", Status::Open), "alpha", UnblockPoint::Merged),
        stacked_on(pr_ship("stacked"), "alpha", UnblockPoint::Implemented),
        done(pr_ship("progress"), &[0, 1]),
        badged(leaf("badged", Status::Open)),
    ];
    let activity = activity("a", vec![leaf("oneoff", Status::Open)]);
    tasks.extend(activity.oneoffs);
    let mut files: Vec<TaskFile> = Vec::new();
    for task in &tasks {
        collect(task, &mut files);
    }
    files.push(parse_task(FULL).unwrap());
    files.push(parse_task(OLD).unwrap());
    files
}

fn collect(task: &Task, files: &mut Vec<TaskFile>) {
    files.extend(task.file.clone());
    for child in &task.children {
        collect(child, files);
    }
}

#[test]
fn join_of_split_is_the_file_for_every_fixture() {
    let files = fixture_files();
    assert!(files.len() >= 12);
    for file in files {
        let (definition, state) = split(&file);
        assert_eq!(join(definition, state), file, "{}", file.goal);
    }
}

#[test]
fn join_of_split_survives_a_file_round_trip_of_each_half() {
    for file in fixture_files() {
        let (definition, state) = split(&file);
        let definition: TaskDefinition =
            toml::from_str(&toml::to_string(&definition).unwrap()).unwrap();
        let state = toml::from_str(&toml::to_string(&state).unwrap()).unwrap();
        assert_eq!(join(definition, state), file, "{}", file.goal);
    }
}

#[test]
fn state_fields_stay_out_of_the_definition_and_the_reverse() {
    let file = parse_task(FULL).unwrap();
    let (definition, state) = split(&file);
    let text = toml::to_string(&definition).unwrap();
    for key in [
        "status",
        "closed",
        "branch",
        "base_sha",
        "worktree",
        "result",
        "history",
        "proposals",
        "linear_state",
        "ship_overridden",
        "default_visibility",
        "rebase_ack_sha",
        "started",
    ] {
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with(&format!("{key} ="))
                    || line.starts_with(&format!("[{key}]"))
                    || line.starts_with(&format!("[[{key}]]"))),
            "{key} in definition: {text}"
        );
    }
    assert!(reserved_keys_in(&definition).is_empty(), "{text}");
    for key in [
        "goal",
        "work_kind",
        "areas",
        "size",
        "repositories",
        "planned_result",
        "stacked_on",
        "after",
    ] {
        assert!(text.contains(key), "{key} missing from definition: {text}");
    }
    assert_eq!(state.status, Status::Done);
    assert_eq!(state.branch.as_deref(), Some("backfill-usage-events"));
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.todos.len(), 2);
    assert_eq!(state.todos[0].state, TodoState::Done);
}

#[test]
fn new_definition_fields_are_carried() {
    let file = parse_task(FULL).unwrap();
    let (definition, _) = split(&file);
    assert_eq!(definition.work_kind.as_deref(), Some("migration"));
    assert_eq!(definition.areas, ["billing", "api"]);
    assert_eq!(definition.size, Some(3));
    assert_eq!(definition.repositories, ["invoicing", "billing-api"]);
    assert_eq!(definition.repository.as_deref(), Some("invoicing"));
    assert_eq!(
        definition.todos[0].id.as_deref(),
        Some("0196f1c2-1111-7000-8000-000000000001")
    );
    assert_eq!(definition.todos[0].role, Some(TodoRole::ProducesBranch));
}

#[test]
fn unknown_keys_survive_both_directions() {
    let file = parse_task(FULL).unwrap();
    assert!(file.extra.contains_key("future_key") && file.extra.contains_key("checks"));
    let (definition, state) = split(&file);
    assert_eq!(definition.extra, file.extra);
    assert_eq!(
        definition.todos[0]
            .extra
            .get("note")
            .and_then(|v| v.as_str()),
        Some("todo extra")
    );

    // Definition text to task file text.
    let text = toml::to_string(&definition).unwrap();
    let parsed: TaskDefinition = toml::from_str(&text).unwrap();
    assert_eq!(parsed, definition);
    let joined = join(parsed, state);
    assert_eq!(joined, file);
    let written = serialize_task(&joined).unwrap();
    assert!(written.contains("future_key = \"kept\""), "{written}");
    assert!(written.contains("note = \"todo extra\""), "{written}");
    assert!(written.contains("required"), "{written}");
}

#[test]
fn each_reserved_state_key_in_a_definition_is_reported_by_name() {
    let base = "id = \"0196f1c2-0000-7000-8000-000000000001\"\ngoal = \"g\"\nkind = \"other\"\ncreated = 2026-11-08T10:00:00Z\n";
    let top_level = |key: &str| match key {
        "status" => "status = \"open\"\n",
        "closed" => "closed = 2026-11-08T10:00:00Z\n",
        "result" => "result = { kind = \"pr\" }\n",
        "branch" => "branch = \"b\"\n",
        "base" => "base = \"main\"\n",
        "base_sha" => "base_sha = \"abc\"\n",
        "worktree" => "worktree = \"/w\"\n",
        "claims" => "claims = []\n",
        "history" => "history = []\n",
        "linear" => "linear = \"SEQ-1\"\n",
        "linear_state" => "linear_state = { issue_id = \"i\" }\n",
        "proposals" => "proposals = []\n",
        other => panic!("{other}"),
    };
    for key in RESERVED_STATE_KEYS {
        let text = match key {
            "todo.state" => format!("{base}\n[[todos]]\nname = \"t\"\nstate = \"done\"\n"),
            "todo.started" => {
                format!("{base}\n[[todos]]\nname = \"t\"\nstarted = 2026-11-08T10:00:00Z\n")
            }
            key => format!("{base}{}", top_level(key)),
        };
        let definition: TaskDefinition = toml::from_str(&text).unwrap();
        assert_eq!(reserved_keys_in(&definition), [key], "{text}");
    }

    let all = format!(
        "{base}status = \"open\"\nbranch = \"b\"\n[[todos]]\nname = \"t\"\nstate = \"done\"\nstarted = 2026-11-08T10:00:00Z\n"
    );
    let definition: TaskDefinition = toml::from_str(&all).unwrap();
    assert_eq!(
        reserved_keys_in(&definition),
        ["status", "todo.state", "todo.started", "branch"]
    );
    // Reported, never dropped.
    assert!(definition.extra.contains_key("status"));
    assert!(definition.todos[0].extra.contains_key("state"));
}

#[test]
fn a_todo_keeps_its_state_when_renamed_or_moved() {
    let file = parse_task(FULL).unwrap();
    let (mut definition, state) = split(&file);
    definition.todos.reverse();
    definition.todos[0].name = "ship it".to_string();
    let joined = join(definition, state);
    assert_eq!(joined.todos[0].name, "ship it");
    assert_eq!(joined.todos[0].state, TodoState::Open);
    assert_eq!(joined.todos[1].name, "implement");
    assert_eq!(joined.todos[1].state, TodoState::Done);
    assert!(joined.todos[1].started.is_some());
}

#[test]
fn an_added_todo_with_no_state_is_open() {
    let file = parse_task(FULL).unwrap();
    let (mut definition, state) = split(&file);
    let mut added = definition.todos[1].clone();
    added.id = Some("0196f1c2-1111-7000-8000-000000000003".to_string());
    definition.todos.push(added);
    let joined = join(definition, state);
    assert_eq!(joined.todos.len(), 3);
    assert_eq!(joined.todos[2].state, TodoState::Open);
    assert_eq!(joined.todos[2].started, None);
}

#[test]
fn old_todos_without_ids_match_by_position() {
    let file = parse_task(OLD).unwrap();
    let (definition, state) = split(&file);
    assert!(definition.todos.iter().all(|todo| todo.id.is_none()));
    assert_eq!(state.todos[1].state, TodoState::Skipped);
    assert_eq!(join(definition, state), file);
}

#[test]
fn a_new_task_file_splits_and_joins() {
    let mut file = TaskFile::new("new", Kind::Document);
    file.areas = vec!["x".to_string()];
    file.size = Some(2);
    file.work_kind = Some("design".to_string());
    let (definition, state) = split(&file);
    assert_eq!(state.status, Status::Open);
    assert_eq!(join(definition, state), file);
}
