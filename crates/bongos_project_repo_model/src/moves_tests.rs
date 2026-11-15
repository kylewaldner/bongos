use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bongos_work_model::TodoState;

use crate::checks::{HoldReason, LiveFacts, LiveSource, MoveHold};
use crate::moves::{MoveRequest, child_folder, is_plain_name, move_plan};
use crate::summary::EdgeKind;
use crate::test_trees::{after, tree_with_project};
use crate::{DefinitionTree, TaskStateRecord, TodoStateRecord};

const SOURCE: &str = "schema = 1\nid = \"src\"\ntitle = \"Invoicing\"\n";
const TARGET: &str =
    "schema = 1\nid = \"dst\"\ntitle = \"Billing\"\nrepositories = [\"billing-api\"]\n";

/// The source: `a` (with the child `c` and grandchild `d`) after `b`, and `e` after `a`.
fn source() -> DefinitionTree {
    tree_with_project(
        SOURCE,
        &[
            (
                "tasks/a",
                &format!("repository = \"ledger\"\n{}", after(&["b"])),
            ),
            ("tasks/a/tasks/c", "repositories = [\"billing-api\"]\n"),
            ("tasks/a/tasks/c/tasks/d", ""),
            ("tasks/b", ""),
            ("tasks/e", &after(&["a"])),
        ],
    )
}

/// The target: the top group with a child folder `a` (id `x`) already.
fn target() -> DefinitionTree {
    tree_with_project(TARGET, &[("tasks/a", ""), ("tasks/a/tasks/q", "")])
}

fn rename_first_id(tree: DefinitionTree, id: &str) -> DefinitionTree {
    let mut tree = tree;
    let top = tree.top.as_mut().unwrap();
    top.children[0].definition.id = id.to_string();
    tree
}

fn request(task: &str, parent: &str) -> MoveRequest {
    MoveRequest {
        task_id: task.to_string(),
        parent_id: parent.to_string(),
    }
}

fn started(id: &str) -> LiveFacts {
    LiveFacts::from_states(BTreeMap::from([(
        id.to_string(),
        TaskStateRecord {
            todos: vec![TodoStateRecord {
                id: Some("t1".to_string()),
                state: TodoState::Done,
                started: None,
            }],
            ..TaskStateRecord::default()
        },
    )]))
}

#[test]
fn a_plan_moves_the_subtree_renames_a_taken_slug_and_drops_crossing_edges() {
    // The target's `a` folder holds another id: the moved `a` becomes `a-2`.
    let target = rename_first_id(target(), "x");
    let plan = move_plan(
        &source(),
        &target,
        &request("a", "top"),
        &LiveFacts::default(),
    )
    .unwrap();
    assert_eq!(plan.slug, "a-2");
    assert_eq!(plan.renamed_from.as_deref(), Some("a"));
    let folders: Vec<(&str, &str)> = plan
        .tasks
        .iter()
        .map(|task| (task.from.as_str(), task.to.as_str()))
        .collect();
    assert_eq!(
        folders,
        [
            ("tasks/a", "tasks/a-2"),
            ("tasks/a/tasks/c", "tasks/a-2/tasks/c"),
            ("tasks/a/tasks/c/tasks/d", "tasks/a-2/tasks/c/tasks/d"),
        ]
    );
    assert_eq!(plan.task_ids(), ["a", "c", "d"]);
    let edges: Vec<(String, EdgeKind, String, bool)> = plan
        .dropped_edges
        .iter()
        .map(|edge| {
            (
                edge.slug.clone(),
                edge.kind,
                edge.target.clone(),
                edge.holder_moves,
            )
        })
        .collect();
    assert_eq!(
        edges,
        [
            ("a".to_string(), EdgeKind::After, "b".to_string(), true),
            ("e".to_string(), EdgeKind::After, "a".to_string(), false),
        ]
    );
    assert_eq!(
        plan.dropped_edges_line().as_deref(),
        Some("Dropped edges: a after b, e after a")
    );
    // `billing-api` is in the target already; `ledger` is not.
    assert_eq!(plan.missing_repositories, ["ledger"]);
    assert_eq!(plan.source.title, "Invoicing");
    assert_eq!(plan.target.title, "Billing");
    plan.validate().unwrap();
}

#[test]
fn a_started_child_or_a_held_task_refuses_the_move() {
    let target = rename_first_id(target(), "x");
    let refusal = move_plan(&source(), &target, &request("a", "top"), &started("d")).unwrap_err();
    assert_eq!(
        refusal.reasons,
        ["child task d is started; only tasks that have not started can move"]
    );
    let refusal = move_plan(&source(), &target, &request("a", "top"), &started("a")).unwrap_err();
    assert_eq!(
        refusal.reasons,
        ["task a is started; only tasks that have not started can move"]
    );

    struct Held;
    impl LiveSource for Held {
        fn name(&self) -> &str {
            "moves"
        }
        fn started(&self, _: &str) -> bool {
            false
        }
        fn held(&self, task_id: &str) -> Option<HoldReason> {
            (task_id == "c").then(|| {
                HoldReason::Moving(MoveHold {
                    move_id: "m1".to_string(),
                    label: "moving to Other".to_string(),
                    coordinator_changes: BTreeSet::new(),
                })
            })
        }
    }
    let mut live = LiveFacts::default();
    live.register(Arc::new(Held));
    let refusal = move_plan(&source(), &target, &request("a", "top"), &live).unwrap_err();
    assert_eq!(
        refusal.reasons,
        ["child task c is held: moving to Other (move m1)"]
    );
}

#[test]
fn a_started_sibling_holding_an_edge_to_the_moved_task_refuses() {
    let target = rename_first_id(target(), "x");
    let refusal = move_plan(&source(), &target, &request("a", "top"), &started("e")).unwrap_err();
    assert_eq!(
        refusal.reasons,
        ["task e is started and has an edge to a; a started task's edges cannot be dropped"]
    );
}

#[test]
fn an_id_already_in_the_target_refuses() {
    // The target's `tasks/a` has the id `a`.
    let refusal = move_plan(
        &source(),
        &target(),
        &request("a", "top"),
        &LiveFacts::default(),
    )
    .unwrap_err();
    assert_eq!(refusal.reasons, ["task a is already in Billing"]);
}

#[test]
fn unknown_tasks_the_top_group_and_the_same_project_refuse() {
    let live = LiveFacts::default();
    let target = rename_first_id(target(), "x");
    let reasons = |source: &DefinitionTree, target: &DefinitionTree, request: MoveRequest| {
        move_plan(source, target, &request, &live)
            .unwrap_err()
            .reasons
    };
    assert_eq!(
        reasons(&source(), &target, request("nope", "top")),
        ["there is no task nope in Invoicing"]
    );
    assert_eq!(
        reasons(&source(), &target, request("a", "nope")),
        ["there is no task nope in Billing"]
    );
    assert_eq!(
        reasons(&source(), &target, request("top", "top")),
        ["the top group of a project cannot be moved"]
    );
    assert_eq!(
        reasons(&source(), &source(), request("a", "top")),
        ["the task is already in Invoicing; pick another project"]
    );
}

#[test]
fn hostile_ids_from_a_request_or_a_definition_are_refused() {
    let live = LiveFacts::default();
    let target = rename_first_id(target(), "x");
    // A peer's request, through serde, naming paths instead of ids.
    for hostile in ["../etc", "a/b", "", ".", "..", ".hidden", "a\\b", "a\0b"] {
        let json = serde_json_like(hostile);
        let request: MoveRequest = toml::from_str(&json).unwrap();
        let refusal = move_plan(&source(), &target, &request, &live).unwrap_err();
        assert!(
            refusal.reasons[0].contains("is not a task id"),
            "{hostile:?}: {refusal}"
        );
    }
    // A definition whose id climbs out of the files folder.
    let source = rename_first_id(source(), "../../outside");
    let refusal = move_plan(&source, &target, &request("../../outside", "top"), &live).unwrap_err();
    assert!(refusal.reasons[0].contains("is not a task id"));
    assert!(!is_plain_name("../../outside"));
    assert!(is_plain_name("a-2"));
    assert_eq!(child_folder("", "a"), "tasks/a");
    assert_eq!(child_folder("tasks/a", "b"), "tasks/a/tasks/b");
}

/// A request as TOML text with the task id `task`.
fn serde_json_like(task: &str) -> String {
    let mut table = toml::Table::new();
    table.insert("task_id".into(), task.into());
    table.insert("parent_id".into(), "top".into());
    toml::to_string(&table).unwrap()
}

#[test]
fn a_stored_plan_with_a_path_in_it_does_not_validate() {
    let target = rename_first_id(target(), "x");
    let plan = move_plan(
        &source(),
        &target,
        &request("a", "top"),
        &LiveFacts::default(),
    )
    .unwrap();
    let text = toml::to_string(&plan).unwrap();
    let read: crate::moves::MovePlan = toml::from_str(&text).unwrap();
    assert_eq!(read, plan);

    let mut hostile = plan.clone();
    hostile.tasks[1].to = "tasks/a-2/../../../escape".to_string();
    assert!(hostile.validate().is_err());
    let mut hostile = plan.clone();
    hostile.tasks[2].id = "../d".to_string();
    assert!(hostile.validate().is_err());
    let mut hostile = plan.clone();
    hostile.tasks[1].from = "tasks/b".to_string();
    assert!(hostile.validate().is_err());
    let mut hostile = plan;
    hostile.target.id = hostile.source.id.clone();
    assert!(hostile.validate().is_err());
}
