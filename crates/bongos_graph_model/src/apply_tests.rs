use std::collections::{BTreeMap, BTreeSet};

use bongos_project_repo_model::{DefinitionOp, TaskDefinition};
use bongos_work_model::UnblockPoint;
use bongos_workflow_model::definition::Scope;
use serde_json::{Value, json};

use crate::apply::Applied;
use crate::test_support::{
    Facts, fixture, fixture_files, nth_id, parse, refusal, run, run_with,
};

/// One line per op: `move <from> -> <to>`, `delete <path>`, `write <path>`.
fn summary(applied: &Applied) -> Vec<String> {
    applied
        .ops
        .iter()
        .map(|op| match op {
            DefinitionOp::Move { from, to } => format!("move {from} -> {to}"),
            DefinitionOp::Delete { path } => format!("delete {path}"),
            DefinitionOp::Write { path, .. } => format!("write {path}"),
        })
        .collect()
}

fn written(applied: &Applied, path: &str) -> TaskDefinition {
    applied
        .ops
        .iter()
        .find_map(|op| match op {
            DefinitionOp::Write { path: at, bytes } if at.as_str() == path => {
                Some(toml::from_str(std::str::from_utf8(bytes).unwrap()).unwrap())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no write of {path} in {:?}", summary(applied)))
}

fn batch(ops: Value) -> Value {
    json!({ "ops": ops })
}

fn ids(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|id| id.to_string()).collect()
}

#[test]
fn create_writes_a_new_task_file() {
    let applied = run(batch(json!([{
        "op": "create", "ref": "n", "parent": "run", "goal": " Index the files ",
        "work_kind": "step", "areas": ["search"], "size": 2, "repositories": ["app"],
        "todos": [{ "name": "implement", "workflow": "ship" }, { "name": "review" }],
    }])))
    .unwrap();
    assert_eq!(
        summary(&applied),
        ["write tasks/run/tasks/index-the-files/task.toml"]
    );
    let task = written(&applied, "tasks/run/tasks/index-the-files/task.toml");
    assert_eq!(task.id, nth_id(1));
    assert_eq!(task.goal, "Index the files");
    assert_eq!(task.work_kind.as_deref(), Some("step"));
    assert_eq!(task.areas, ["search"]);
    assert_eq!(task.size, Some(2));
    assert_eq!(task.repositories, ["app"]);
    assert_eq!(task.created.to_string(), "2025-10-28T14:32:43Z");
    let todos: Vec<(Option<String>, String, Option<String>)> = task
        .todos
        .iter()
        .map(|todo| (todo.id.clone(), todo.name.clone(), todo.workflow.clone()))
        .collect();
    assert_eq!(
        todos,
        [
            (
                Some(nth_id(2)),
                "implement".to_string(),
                Some("ship".to_string())
            ),
            (Some(nth_id(3)), "review".to_string(), None),
        ]
    );
    assert_eq!(applied.created, [("n".to_string(), nth_id(1))]);
    assert_eq!(applied.touched, ids(&[&nth_id(1)]));
}

#[test]
fn create_without_todos_gets_the_default_todos_with_ids() {
    let applied = run(batch(json!([{ "op": "create", "parent": "run", "goal": "Plain" }]))).unwrap();
    let task = written(&applied, "tasks/run/tasks/plain/task.toml");
    let names: Vec<&str> = task.todos.iter().map(|todo| todo.name.as_str()).collect();
    assert_eq!(names, ["implement", "review", "ship"]);
    assert!(task.todos.iter().all(|todo| todo.id.is_some()));
    assert!(applied.created.is_empty());
}

#[test]
fn create_slugs_avoid_siblings_and_documents() {
    let applied = run(batch(json!([
        { "op": "create", "parent": "run", "goal": "A" },
        { "op": "create", "parent": "run", "goal": "Documents" },
        { "op": "create", "parent": "run", "goal": "Runs" },
    ])))
    .unwrap();
    assert_eq!(
        summary(&applied),
        [
            "write tasks/run/tasks/a-2/task.toml",
            "write tasks/run/tasks/documents-2/task.toml",
            "write tasks/run/tasks/runs-2/task.toml",
        ]
    );
}

#[test]
fn set_rewrites_the_task_file() {
    let applied = run(batch(json!([{
        "op": "set", "task": "a", "goal": "Better", "work_kind": "fix", "size": 3,
        "areas": ["ui", "search"],
    }])))
    .unwrap();
    assert_eq!(summary(&applied), ["write tasks/run/tasks/a/task.toml"]);
    let task = written(&applied, "tasks/run/tasks/a/task.toml");
    assert_eq!(task.goal, "Better");
    assert_eq!(task.work_kind.as_deref(), Some("fix"));
    assert_eq!(task.size, Some(3));
    assert_eq!(task.areas, ["ui", "search"]);
    assert_eq!(task.created.to_string(), "2026-01-01T10:00:00Z");
    assert_eq!(applied.touched, ids(&["a"]));
}

#[test]
fn set_with_unchanged_values_writes_nothing() {
    let applied = run(batch(json!([{ "op": "set", "task": "a", "goal": "Goal a" }]))).unwrap();
    assert!(applied.ops.is_empty());
    assert!(applied.touched.is_empty());
}

#[test]
fn move_moves_the_folder_and_drops_the_edges_to_it() {
    let applied = run(batch(json!([{ "op": "move", "task": "a", "parent": "c" }]))).unwrap();
    assert_eq!(
        summary(&applied),
        [
            "move tasks/run/tasks/a -> tasks/run/tasks/c/tasks/a",
            "write tasks/run/tasks/b/task.toml",
        ]
    );
    assert!(written(&applied, "tasks/run/tasks/b/task.toml").after.is_empty());
    assert_eq!(applied.touched, ids(&["a", "b"]));
}

#[test]
fn move_drops_the_moved_tasks_own_edges_and_renames_on_a_clash() {
    let tree = parse(&{
        let mut files = fixture_files();
        files.insert(
            "tasks/run/tasks/c/tasks/b/task.toml".to_string(),
            crate::test_support::task_toml("cb", "Goal cb", ""),
        );
        files
    });
    let applied = run_with(
        &tree,
        batch(json!([{ "op": "move", "task": "b", "parent": "c" }])),
        Scope::OwnSubtree,
        &Facts::default(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        summary(&applied),
        [
            "move tasks/run/tasks/b -> tasks/run/tasks/c/tasks/b-2",
            "write tasks/run/tasks/c/tasks/b-2/task.toml",
        ]
    );
    assert!(written(&applied, "tasks/run/tasks/c/tasks/b-2/task.toml").after.is_empty());
}

#[test]
fn remove_deletes_the_subtree_and_drops_the_edges_to_it() {
    let applied = run(batch(json!([{ "op": "remove", "task": "a" }]))).unwrap();
    assert_eq!(
        summary(&applied),
        [
            "delete tasks/run/tasks/a/task.toml",
            "write tasks/run/tasks/b/task.toml",
        ]
    );
    assert!(written(&applied, "tasks/run/tasks/b/task.toml").after.is_empty());

    let applied = run_with(
        &fixture(),
        batch(json!([{ "op": "remove", "task": "other" }])),
        Scope::OwnProject,
        &Facts::default(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        summary(&applied),
        [
            "delete tasks/other/task.toml",
            "delete tasks/other/tasks/x/task.toml",
        ]
    );
    assert_eq!(applied.touched, ids(&["other", "x"]));
}

#[test]
fn add_edge_writes_the_holder() {
    let applied = run(batch(json!([
        { "op": "add_edge", "from": "c", "to": "a", "kind": "after" },
        { "op": "add_edge", "from": "b", "to": "c", "kind": "stacked_on" },
    ])))
    .unwrap();
    assert_eq!(
        summary(&applied),
        [
            "write tasks/run/tasks/b/task.toml",
            "write tasks/run/tasks/c/task.toml",
        ]
    );
    let c = written(&applied, "tasks/run/tasks/c/task.toml");
    assert_eq!(c.after.len(), 1);
    assert_eq!(c.after[0].task, "a");
    assert_eq!(c.after[0].unblock, UnblockPoint::Merged);
    let b = written(&applied, "tasks/run/tasks/b/task.toml");
    assert_eq!(b.after[0].task, "a");
    let stacked = b.stacked_on.expect("stacked on c");
    assert_eq!(stacked.task, "c");
    assert_eq!(stacked.unblock, UnblockPoint::Review);
}

#[test]
fn add_edge_takes_the_given_unblock_point() {
    let applied = run(batch(json!([
        { "op": "add_edge", "from": "c", "to": "a", "kind": "after", "unblock": "implemented" },
    ])))
    .unwrap();
    let c = written(&applied, "tasks/run/tasks/c/task.toml");
    assert_eq!(c.after[0].unblock, UnblockPoint::Implemented);
}

#[test]
fn remove_edge_writes_the_holder() {
    let applied = run(batch(json!([
        { "op": "remove_edge", "from": "b", "to": "a", "kind": "after" },
    ])))
    .unwrap();
    assert_eq!(summary(&applied), ["write tasks/run/tasks/b/task.toml"]);
    assert!(written(&applied, "tasks/run/tasks/b/task.toml").after.is_empty());
    assert_eq!(
        refusal(
            batch(json!([{ "op": "remove_edge", "from": "b", "to": "a", "kind": "stacked_on" }])),
            Scope::OwnSubtree,
            &Facts::default(),
        ),
        "b has no stacked_on edge to a"
    );
}

#[test]
fn refs_resolve_across_ops() {
    let applied = run(batch(json!([
        { "op": "create", "ref": "p", "parent": "run", "goal": "Parent" },
        { "op": "create", "ref": "k", "parent": "@p", "goal": "Kid" },
        { "op": "create", "ref": "q", "parent": "run", "goal": "Next" },
        { "op": "add_edge", "from": "@q", "to": "@p", "kind": "after" },
        { "op": "set", "task": "@k", "size": 5 },
        { "op": "move", "task": "a", "parent": "@p" },
    ])))
    .unwrap();
    assert_eq!(
        summary(&applied),
        [
            "move tasks/run/tasks/a -> tasks/run/tasks/parent/tasks/a",
            "write tasks/run/tasks/b/task.toml",
            "write tasks/run/tasks/next/task.toml",
            "write tasks/run/tasks/parent/task.toml",
            "write tasks/run/tasks/parent/tasks/kid/task.toml",
        ]
    );
    let kid = written(&applied, "tasks/run/tasks/parent/tasks/kid/task.toml");
    assert_eq!(kid.size, Some(5));
    let next = written(&applied, "tasks/run/tasks/next/task.toml");
    assert_eq!(next.after[0].task, "parent");
    let created: Vec<&str> = applied.created.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(created, ["p", "k", "q"]);
}

#[test]
fn a_create_with_a_recorded_ref_reuses_its_id() {
    let recorded = "019a0000-0000-7000-8000-00000000abcd".to_string();
    let refs = BTreeMap::from([("n".to_string(), recorded.clone())]);
    let applied = run_with(
        &fixture(),
        batch(json!([
            { "op": "create", "ref": "n", "parent": "run", "goal": "A" },
            { "op": "set", "task": "@n", "size": 1 },
        ])),
        Scope::OwnSubtree,
        &Facts::default(),
        &refs,
    )
    .unwrap();
    // The slug is recomputed on the current tree: `a` is taken.
    let task = written(&applied, "tasks/run/tasks/a-2/task.toml");
    assert_eq!(task.id, recorded);
    assert_eq!(task.size, Some(1));
    assert_eq!(applied.created, [("n".to_string(), recorded)]);
}

#[test]
fn retitling_a_started_task_is_allowed() {
    let applied = run_with(
        &fixture(),
        batch(json!([{ "op": "set", "task": "a", "goal": "Renamed" }])),
        Scope::OwnSubtree,
        &Facts::started(&["a"]),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(written(&applied, "tasks/run/tasks/a/task.toml").goal, "Renamed");
}

#[test]
fn refuses_when_graph_edits_are_not_declared() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "set", "task": "a", "goal": "X" }])),
            Scope::None,
            &Facts::default()
        ),
        "graph edits not declared"
    );
}

#[test]
fn refuses_tasks_outside_the_scope() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "set", "task": "x", "goal": "X" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "x is outside the declared scope (own subtree of run)"
    );
    // A create's membership is its parent's.
    assert_eq!(
        refusal(
            batch(json!([{ "op": "create", "parent": "other", "goal": "X" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "other is outside the declared scope (own subtree of run)"
    );
    // Both ends of an edge.
    assert_eq!(
        refusal(
            batch(json!([{ "op": "add_edge", "from": "run", "to": "other", "kind": "after" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "other is outside the declared scope (own subtree of run)"
    );
    assert_eq!(
        run_with(
            &fixture(),
            json!({ "project": "q", "ops": [] }),
            Scope::OwnProject,
            &Facts::default(),
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string(),
        "project q is outside the declared scope (own project p)"
    );
}

#[test]
fn own_project_scope_reaches_every_task_of_the_project() {
    let applied = run_with(
        &fixture(),
        json!({ "project": "p", "ops": [{ "op": "set", "task": "x", "size": 4 }] }),
        Scope::OwnProject,
        &Facts::default(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(summary(&applied), ["write tasks/other/tasks/x/task.toml"]);
}

#[test]
fn refuses_unknown_tasks() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "remove", "task": "nope" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "unknown task nope"
    );
}

#[test]
fn refuses_edges_between_non_siblings() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "add_edge", "from": "a", "to": "x", "kind": "after" }])),
            Scope::OwnProject,
            &Facts::default()
        ),
        "edges join siblings only: a and x"
    );
}

#[test]
fn refuses_cycles() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "add_edge", "from": "a", "to": "b", "kind": "after" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "cycle: a -> b -> a"
    );
}

#[test]
fn refuses_reshaping_a_started_task() {
    let started = Facts::started(&["a", "b"]);
    let cases = [
        (
            json!({ "op": "set", "task": "a", "areas": ["ui"] }),
            "started task `Goal a` cannot be retagged",
        ),
        (
            json!({ "op": "set", "task": "a", "size": 9 }),
            "started task `Goal a` cannot be resized",
        ),
        (
            json!({ "op": "set", "task": "a", "work_kind": "spike" }),
            "started task `Goal a` cannot be given a new work kind",
        ),
        (
            json!({ "op": "add_edge", "from": "a", "to": "c", "kind": "after" }),
            "started task `Goal a` cannot be reordered",
        ),
        (
            json!({ "op": "remove_edge", "from": "b", "to": "a", "kind": "after" }),
            "started task `Goal b` cannot be reordered",
        ),
        (
            json!({ "op": "move", "task": "a", "parent": "c" }),
            "started task `Goal a` cannot be moved",
        ),
        (
            json!({ "op": "remove", "task": "a" }),
            "started task `Goal a` cannot be removed",
        ),
    ];
    for (op, expected) in cases {
        assert_eq!(
            refusal(batch(json!([op])), Scope::OwnSubtree, &started),
            expected
        );
    }
}

#[test]
fn removing_a_task_a_started_sibling_follows_is_reordering_it() {
    assert_eq!(
        refusal(
            batch(json!([{ "op": "remove", "task": "a" }])),
            Scope::OwnSubtree,
            &Facts::started(&["b"])
        ),
        "started task `Goal b` cannot be reordered"
    );
}

#[test]
fn refuses_removing_a_task_with_runs() {
    let facts = Facts {
        runs: ids(&["c"]),
        ..Facts::default()
    };
    assert_eq!(
        refusal(
            batch(json!([{ "op": "remove", "task": "c" }])),
            Scope::OwnSubtree,
            &facts
        ),
        "task `Goal c` has runs"
    );
}

#[test]
fn refuses_held_tasks_the_top_group_and_moves_into_the_own_subtree() {
    let facts = Facts {
        held: BTreeMap::from([("c".to_string(), "moving to Web".to_string())]),
        ..Facts::default()
    };
    assert_eq!(
        refusal(
            batch(json!([{ "op": "set", "task": "c", "goal": "X" }])),
            Scope::OwnSubtree,
            &facts
        ),
        "task `Goal c` is held: moving to Web"
    );
    assert_eq!(
        refusal(
            batch(json!([{ "op": "set", "task": "top", "goal": "X" }])),
            Scope::OwnProject,
            &Facts::default()
        ),
        "the top group cannot be edited"
    );
    assert_eq!(
        refusal(
            batch(json!([{ "op": "move", "task": "run", "parent": "a" }])),
            Scope::OwnSubtree,
            &Facts::default()
        ),
        "task `Goal run` cannot be moved into its own subtree"
    );
}

/// A small deterministic generator (a 64-bit LCG with a fixed seed): no clock, no randomness.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, items: &'a [String]) -> &'a String {
        &items[(self.next() as usize) % items.len()]
    }
}

fn generated_batch(rng: &mut Lcg) -> Value {
    let mut names: Vec<String> = ["top", "run", "a", "b", "c", "other", "x"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let goals = ["A", "B", "Documents", "Runs", "New work", "Tasks", "Project"];
    let mut ops = Vec::new();
    let mut made = 0;
    for _ in 0..(1 + rng.next() % 6) {
        let op = match rng.next() % 6 {
            0 => {
                made += 1;
                let name = format!("r{made}");
                let op = json!({
                    "op": "create", "ref": name, "parent": rng.pick(&names),
                    "goal": goals[(rng.next() as usize) % goals.len()],
                });
                names.push(format!("@{name}"));
                op
            }
            1 => json!({ "op": "set", "task": rng.pick(&names), "size": rng.next() % 4 }),
            2 => json!({ "op": "move", "task": rng.pick(&names), "parent": rng.pick(&names) }),
            3 => json!({ "op": "remove", "task": rng.pick(&names) }),
            4 => json!({
                "op": "add_edge", "from": rng.pick(&names), "to": rng.pick(&names),
                "kind": if rng.next() % 2 == 0 { "after" } else { "stacked_on" },
            }),
            _ => json!({
                "op": "remove_edge", "from": rng.pick(&names), "to": rng.pick(&names),
                "kind": "after",
            }),
        };
        ops.push(op);
    }
    json!({ "ops": ops })
}

/// Lands `ops` on `files` as a landing does: the moves first, then deletes and writes.
fn land(files: &mut BTreeMap<String, String>, ops: &[DefinitionOp]) {
    for op in ops {
        if let DefinitionOp::Move { from, to } = op {
            let prefix = format!("{from}/");
            let moved: Vec<String> = files
                .keys()
                .filter(|path| path.starts_with(&prefix))
                .cloned()
                .collect();
            assert!(!moved.is_empty(), "the move of {from} finds files");
            for path in moved {
                let text = files.remove(&path).unwrap();
                files.insert(format!("{to}/{}", &path[prefix.len()..]), text);
            }
        }
    }
    for op in ops {
        match op {
            DefinitionOp::Delete { path } => {
                assert!(files.remove(path.as_str()).is_some(), "{path} exists");
            }
            DefinitionOp::Write { path, bytes } => {
                files.insert(path.to_string(), String::from_utf8(bytes.clone()).unwrap());
            }
            DefinitionOp::Move { .. } => {}
        }
    }
}

fn under_tasks(path: &str) -> bool {
    path.starts_with("tasks/")
        && !path.split('/').any(|part| {
            ["project.toml", "workflows", "lanes", "checks", "..", "."].contains(&part)
        })
}

#[test]
fn generated_batches_only_change_task_files_under_tasks() {
    let mut rng = Lcg(0x5eed_0052);
    let mut applied_count = 0;
    for _ in 0..2000 {
        let value = generated_batch(&mut rng);
        let Ok(applied) = run_with(
            &fixture(),
            value.clone(),
            Scope::OwnProject,
            &Facts::default(),
            &BTreeMap::new(),
        ) else {
            continue;
        };
        applied_count += 1;
        for op in &applied.ops {
            match op {
                DefinitionOp::Write { path, .. } | DefinitionOp::Delete { path } => {
                    assert!(
                        under_tasks(path.as_str()) && path.as_str().ends_with("/task.toml"),
                        "{path} in {value}"
                    );
                }
                DefinitionOp::Move { from, to } => {
                    assert!(under_tasks(from) && under_tasks(to), "{from} -> {to} in {value}");
                    assert!(!from.ends_with(".toml") && !to.ends_with(".toml"));
                }
            }
        }
        // The landed files parse, and hold exactly the tasks the batch left.
        let mut files = fixture_files();
        land(&mut files, &applied.ops);
        assert_eq!(files.get("project.toml"), fixture_files().get("project.toml"));
        assert_eq!(files.get("task.toml"), fixture_files().get("task.toml"));
        let landed = parse(&files);
        let landed_ids: BTreeSet<String> = landed
            .nodes()
            .iter()
            .map(|node| node.definition.id.clone())
            .collect();
        let original: BTreeSet<String> = fixture()
            .nodes()
            .iter()
            .map(|node| node.definition.id.clone())
            .collect();
        for id in &original {
            if !applied.touched.contains(id) {
                assert!(landed_ids.contains(id), "{id} kept in {value}");
            }
        }
        for id in &landed_ids {
            assert!(
                original.contains(id) || applied.touched.contains(id),
                "{id} is new but not touched in {value}"
            );
        }
    }
    assert!(applied_count >= 250,"only {applied_count} batches applied");
}
