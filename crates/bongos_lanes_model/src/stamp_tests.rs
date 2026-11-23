use crate::graph::*;
use crate::stamp::*;
use crate::state::TaskId;
use crate::table::{LaneTable, parse};

fn id(text: &str) -> TaskId {
    TaskId::new(text)
}

fn named(task: &str, name: &str) -> PickTask {
    let mut leaf = PickTask::leaf(task);
    leaf.name = name.to_string();
    leaf
}

fn after(mut task: PickTask, target: &str) -> PickTask {
    task.edges.push(PickEdge {
        kind: EdgeKind::After,
        target: id(target),
        unblock: "merged".to_string(),
        reached: false,
        progress: "step 1 of 2 not done".to_string(),
    });
    task
}

fn tasks() -> Vec<PickTask> {
    let mut group = PickTask::group("f", &["f1", "f2"]);
    group.name = "Billing".to_string();
    group.areas = vec!["ui".to_string(), "db".to_string()];
    let mut f1 = named("f1", "Billing design");
    f1.work_kind = Some("design".to_string());
    f1.size = Some(2);
    vec![
        named("a", "Invoice export"),
        after(named("b", "Usage events"), "a"),
        group,
        f1,
        after(named("f2", "Billing UI"), "f1"),
    ]
}

fn graph_of(tasks: Vec<PickTask>) -> PickGraph {
    PickGraph::new(vec![id("a"), id("b"), id("f")], tasks)
}

fn table() -> LaneTable {
    parse(
        "schema = 1\n[[kind]]\nname = \"build\"\n\
         [[pin]]\ntask = \"a\"\nlane = \"build-1\"\n\
         [[pin]]\ntask = \"b\"\nkind = \"build\"\n\
         [[exempt]]\ntags = [\"ui\", \"db\"]\n\
         [[exempt]]\ntasks = [\"a\", \"b\"]\n",
    )
    .unwrap()
}

#[test]
fn the_stamp_does_not_depend_on_input_order() {
    let expected = compute(&graph_of(tasks()), &table());
    let mut reversed = tasks();
    reversed.reverse();
    let mut table = table();
    table.pins.reverse();
    table.exemptions.reverse();
    for exempt in &mut table.exemptions {
        exempt.tags.reverse();
        exempt.tasks.reverse();
    }
    let mut shuffled = tasks();
    for task in &mut shuffled {
        task.areas.reverse();
        task.edges.reverse();
    }
    assert_eq!(compute(&graph_of(reversed), &table), expected);
    assert_eq!(compute(&graph_of(shuffled), &table), expected);
    assert_eq!(compute(&graph_of(tasks()), &table), expected);
    assert_eq!(expected.digest_hex().len(), 64);
    assert_eq!(expected.per_task.len(), 5);
}

// Nothing that changes with status changes the stamp.
#[test]
fn a_status_change_does_not_change_the_stamp() {
    let table = table();
    let before = compute(&graph_of(tasks()), &table);
    let mut changed = tasks();
    for task in &mut changed {
        task.state = TaskState::Done;
        task.started = true;
        task.name.push_str(" (renamed)");
        task.active_run = Some("run-1".to_string());
        task.held = Some("moving".to_string());
        task.open_todo = Some(OpenTodo {
            index: 2,
            name: "review".to_string(),
            workflow: Some("w".to_string()),
        });
        for edge in &mut task.edges {
            edge.reached = true;
            edge.progress.clear();
        }
    }
    let after = compute(&graph_of(changed), &table);
    assert_eq!(after.digest, before.digest);
    assert_eq!(after.per_task, before.per_task);
    assert!(diff(&StampFile::from_stamp(&before), &after).is_empty());

    // An unset size counts as one job, like the rank does.
    let mut sized = tasks();
    sized[0].size = Some(1);
    assert_eq!(compute(&graph_of(sized), &table).digest, before.digest);
}

// A structure change names the task.
#[test]
fn a_structure_change_names_the_task() {
    let table = table();
    let recorded = StampFile::from_stamp(&compute(&graph_of(tasks()), &table));

    let mut edge_added = tasks();
    edge_added[0] = after(named("a", "Invoice export"), "b");
    let current = compute(&graph_of(edge_added), &table);
    let found = diff(&recorded, &current);
    assert_eq!(
        found.changed,
        vec![StampTask {
            id: id("a"),
            name: "Invoice export".to_string()
        }]
    );
    assert_eq!(found.first_task(), Some(&id("a")));
    assert_eq!(
        found.to_string(),
        "graph changed since the stamp: changed Invoice export"
    );

    for change in [
        |task: &mut PickTask| task.work_kind = Some("step".to_string()),
        |task: &mut PickTask| task.areas.push("lsp".to_string()),
        |task: &mut PickTask| task.size = Some(5),
        |task: &mut PickTask| task.edges[0].unblock = "review".to_string(),
        |task: &mut PickTask| task.edges[0].kind = EdgeKind::StackedOn,
    ] {
        let mut changed = tasks();
        change(&mut changed[4]);
        let found = diff(&recorded, &compute(&graph_of(changed), &table));
        assert_eq!(found.changed.len(), 1);
        assert_eq!(found.changed[0].id, id("f2"));
        assert!(found.added.is_empty() && found.removed.is_empty());
    }

    // Added and removed tasks; a removed task is named by its id.
    let mut replaced: Vec<PickTask> = tasks()
        .into_iter()
        .filter(|task| task.id != id("a"))
        .collect();
    replaced.push(named("c", "Cleanup"));
    replaced[0].edges.clear();
    let graph = PickGraph::new(vec![id("c"), id("b"), id("f")], replaced);
    let found = diff(&recorded, &compute(&graph, &table));
    assert_eq!(
        found.to_string(),
        "graph changed since the stamp: added Cleanup; changed Usage events; removed a"
    );
    assert!(!found.table_changed);

    // A parent change: f2 moves to the top level.
    let mut moved = tasks();
    moved[2].children.retain(|child| child != &id("f2"));
    let graph = PickGraph::new(vec![id("a"), id("b"), id("f"), id("f2")], moved);
    let found = diff(&recorded, &compute(&graph, &table));
    assert_eq!(found.changed[0].id, id("f2"));
}

#[test]
fn a_pin_or_exemption_change_is_a_table_change() {
    let recorded = StampFile::from_stamp(&compute(&graph_of(tasks()), &table()));
    for edit in [
        |table: &mut LaneTable| table.pins.truncate(1),
        |table: &mut LaneTable| table.exemptions.truncate(1),
        |table: &mut LaneTable| table.pins[0].lane.as_mut().unwrap().value = "build-2".to_string(),
    ] {
        let mut table = table();
        edit(&mut table);
        let found = diff(&recorded, &compute(&graph_of(tasks()), &table));
        assert!(found.table_changed);
        assert_eq!(
            found.to_string(),
            "graph changed since the stamp: pins or exemptions changed"
        );
    }
    // A table field that is not a pin or an exemption is not part of the stamp.
    let mut table = table();
    table.kinds[0].count = 7;
    let current = compute(&graph_of(tasks()), &table);
    assert!(current.matches(&recorded));
}

#[test]
fn stamp_toml_round_trips_with_a_stable_key_order() {
    let file = StampFile::from_stamp(&compute(&graph_of(tasks()), &table()));
    let text = file.render().unwrap();
    assert!(text.starts_with("schema = 1\ndigest = \""), "{text}");
    let tasks_at = text.find("[tasks]").unwrap();
    let keys: Vec<&str> = text[tasks_at..]
        .lines()
        .skip(1)
        .filter_map(|line| line.split(" = ").next())
        .collect();
    assert_eq!(keys, vec!["a", "b", "f", "f1", "f2"]);
    assert_eq!(StampFile::parse(&text).unwrap(), file);
    assert_eq!(StampFile::parse(&text).unwrap().render().unwrap(), text);
}

#[test]
fn a_malformed_stamp_is_a_typed_error() {
    let good_digest = "0".repeat(64);
    let cases: Vec<(String, &str)> = vec![
        ("schema = ".to_string(), "parse"),
        ("digest = \"x\"".to_string(), "parse"),
        (
            format!("schema = 2\ndigest = \"{good_digest}\"\n"),
            "schema",
        ),
        ("schema = 1\ndigest = \"abc\"\n".to_string(), "digest"),
        (
            format!("schema = 1\ndigest = \"{good_digest}\"\n[tasks]\na = \"zz\"\n"),
            "task",
        ),
        (
            format!("schema = 1\ndigest = \"{good_digest}\"\nextra = 1\n"),
            "parse",
        ),
        ("schema = \"one\"\n".to_string(), "parse"),
    ];
    for (text, kind) in cases {
        let error = StampFile::parse(&text).unwrap_err();
        let matched = match (&error, kind) {
            (StampError::Toml(_), "parse") => true,
            (StampError::Schema(2), "schema") => true,
            (StampError::Digest(_), "digest") => true,
            (StampError::TaskHash { task, .. }, "task") => task == &id("a"),
            _ => false,
        };
        assert!(matched, "{text:?} gave {error:?}");
    }
    let empty = StampFile::parse(&format!("schema = 1\ndigest = \"{good_digest}\"\n")).unwrap();
    assert!(empty.tasks.is_empty());
}

#[test]
fn an_empty_graph_has_a_stamp() {
    let empty = PickGraph::new(Vec::new(), Vec::new());
    let table = LaneTable::implicit();
    let stamp = compute(&empty, &table);
    assert!(stamp.per_task.is_empty());
    assert_eq!(compute(&empty, &table), stamp);
    assert!(stamp.matches(&StampFile::from_stamp(&stamp)));
}
