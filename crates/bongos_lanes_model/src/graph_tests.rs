use std::collections::BTreeSet;

use bongos_work_model::fixtures::{
    after, done, file_mut, group, id_of, leaf, pr_ship, todo, tree, with_created,
};
use bongos_work_model::{Status, TodoRole, UnblockPoint};

use crate::graph::*;
use crate::picker::parallel;
use crate::rank::{Chains, critical_path, plain_rank_key};
use crate::state::TaskId;
use crate::table::{LaneTable, parse};

fn id(text: &str) -> TaskId {
    TaskId::new(text)
}

fn tags(list: &[&str]) -> Vec<String> {
    list.iter().map(|tag| tag.to_string()).collect()
}

fn edge_to(target: &str, reached: bool) -> PickEdge {
    PickEdge {
        kind: EdgeKind::After,
        target: id(target),
        unblock: "merged".to_string(),
        reached,
        progress: if reached {
            String::new()
        } else {
            "step 1 of 1 not done".to_string()
        },
    }
}

/// Top level: A (group of a1, a2), B after A, C after B, D (size 5 when `big_d`).
fn fixture(big_d: bool) -> Vec<PickTask> {
    let mut b = PickTask::leaf("B");
    b.edges.push(edge_to("A", false));
    let mut c = PickTask::leaf("C");
    c.edges.push(edge_to("B", false));
    let mut d = PickTask::leaf("D");
    if big_d {
        d.size = Some(5);
    }
    vec![
        PickTask::group("A", &["a1", "a2"]),
        PickTask::leaf("a1"),
        PickTask::leaf("a2"),
        b,
        c,
        d,
    ]
}

fn graph_of(top: &[&str], tasks: Vec<PickTask>) -> PickGraph {
    PickGraph::new(top.iter().map(|t| id(t)).collect(), tasks)
}

#[test]
fn walk_order_parents_and_leaves() {
    let graph = graph_of(&["A", "B", "C", "D"], fixture(false));
    let walk: Vec<&str> = graph.walk().map(|task| task.id.0.as_str()).collect();
    assert_eq!(walk, ["A", "a1", "a2", "B", "C", "D"]);
    assert_eq!(graph.order(&id("a2")), 3);
    assert_eq!(graph.parent(&id("a2")), Some(&id("A")));
    assert_eq!(graph.path(&id("a2")), vec![id("A"), id("a2")]);
    assert_eq!(graph.top_level_of(&id("a1")), id("A"));
    assert_eq!(graph.dependents(&id("A")), vec![id("B")]);
    let leaves: Vec<&str> = graph.leaves().map(|task| task.id.0.as_str()).collect();
    assert_eq!(leaves, ["a1", "a2", "B", "C", "D"]);
}

#[test]
fn chain_counts_remaining_jobs_down_the_dependents() {
    let graph = graph_of(&["A", "B", "C", "D"], fixture(false));
    let chains = Chains::compute(&graph);
    assert_eq!(chains.remaining(&id("A")), 2);
    assert_eq!(chains.chain(&id("C")), 1);
    assert_eq!(chains.chain(&id("B")), 2);
    assert_eq!(chains.chain(&id("A")), 4);
    assert_eq!(critical_path(&graph), 4);

    // A finished leaf no longer counts; a size counts as that many jobs.
    let mut tasks = fixture(true);
    tasks[1].state = TaskState::Done;
    let graph = graph_of(&["A", "B", "C", "D"], tasks);
    let chains = Chains::compute(&graph);
    assert_eq!(chains.chain(&id("A")), 3);
    assert_eq!(critical_path(&graph), 5);
}

#[test]
fn critical_path_skips_finished_top_level_tasks() {
    let mut tasks = fixture(true);
    tasks[5].state = TaskState::Cancelled;
    let graph = graph_of(&["A", "B", "C", "D"], tasks);
    assert_eq!(critical_path(&graph), 4);
}

#[test]
fn a_started_leaf_counts_as_started_in_the_state_free_rank() {
    let mut tasks = fixture(false);
    tasks[5].started = true;
    let graph = graph_of(&["A", "B", "C", "D"], tasks);
    let chains = Chains::compute(&graph);
    let d = plain_rank_key(&graph, &chains, &id("D"));
    let a1 = plain_rank_key(&graph, &chains, &id("a1"));
    assert!(d.levels[0].started);
    assert!(!a1.levels[0].started);
    assert!(d < a1, "started first, before the longer chain");
}

fn area_graph(started: Option<&str>) -> PickGraph {
    let mut tasks: Vec<PickTask> = [
        ("X", &["editor", "billing"][..]),
        ("Y", &["editor"][..]),
        ("Z", &["billing"][..]),
        ("W", &[][..]),
    ]
    .into_iter()
    .map(|(name, areas)| {
        let mut task = PickTask::leaf(name);
        task.areas = tags(areas);
        task
    })
    .collect();
    if let Some(started) = started {
        for task in &mut tasks {
            if task.id.0 == started {
                task.started = true;
            }
        }
    }
    graph_of(&["X", "Y", "Z", "W"], tasks)
}

#[test]
fn parallel_counts_area_free_tasks_in_rank_order() {
    let table = LaneTable::implicit();
    // X goes first by order and shuts out Y and Z.
    assert_eq!(parallel(&area_graph(None), &table, None), 2);
    // A started leaf counts as started: Y goes first, then Z fits beside it.
    assert_eq!(parallel(&area_graph(Some("Y")), &table, None), 3);
}

#[test]
fn cap_bounds_parallel() {
    let table = LaneTable::implicit();
    let graph = area_graph(Some("Y"));
    assert_eq!(parallel(&graph, &table, Some(2)), 2);
    assert_eq!(parallel(&graph, &table, Some(0)), 0);
    assert_eq!(parallel(&graph, &table, Some(10)), 3);
}

#[test]
fn exemptions_let_tasks_share_tags() {
    let table =
        parse("schema = 1\n[[exempt]]\ntags = [\"editor\"]\n[[kind]]\nname = \"build\"\n").unwrap();
    // Only billing conflicts now: X, Y and W.
    assert_eq!(parallel(&area_graph(None), &table, None), 3);
    let table = parse(
        "schema = 1\n[[exempt]]\ntasks = [\"X\", \"Y\"]\n[[exempt]]\ntasks = [\"X\", \"Z\"]\n[[kind]]\nname = \"build\"\n",
    )
    .unwrap();
    assert_eq!(parallel(&area_graph(None), &table, None), 4);
}

#[test]
fn parallel_skips_tasks_that_are_not_ready() {
    let table = LaneTable::implicit();
    let mut tasks = vec![
        PickTask::leaf("P"),
        PickTask::leaf("Q"),
        PickTask::leaf("R"),
    ];
    tasks[0].active_run = Some("run-1".to_string());
    tasks[1].edges.push(edge_to("R", false));
    let graph = graph_of(&["P", "Q", "R"], tasks);
    assert_eq!(parallel(&graph, &table, None), 1);
}

#[test]
fn areas_inherit_from_every_ancestor() {
    let mut tasks = vec![
        PickTask::group("G", &["H"]),
        PickTask::group("H", &["l"]),
        PickTask::leaf("l"),
    ];
    tasks[0].areas = tags(&["editor"]);
    tasks[1].areas = tags(&["lsp"]);
    tasks[2].areas = tags(&["ui"]);
    let graph = graph_of(&["G"], tasks);
    let expected: BTreeSet<String> = tags(&["editor", "lsp", "ui"]).into_iter().collect();
    assert_eq!(graph.areas(&id("l")), expected);
    assert_eq!(graph.subtree_areas(&id("G")), expected);
}

#[test]
fn a_cycle_in_hand_made_children_never_loops() {
    let tasks = vec![PickTask::group("G", &["H"]), PickTask::group("H", &["G"])];
    let graph = graph_of(&["G"], tasks);
    assert_eq!(graph.walk().count(), 2);
    assert!(graph.leaves_under(&id("G")).is_empty());
    assert_eq!(critical_path(&graph), 0);
}

#[test]
fn builds_from_a_work_tree() {
    let mut first = with_created(pr_ship("first"), "2026-01-01T00:00:00Z");
    file_mut(&mut first).todos[0].workflow = Some("implement".to_string());
    file_mut(&mut first).areas = tags(&["ui"]);
    let second = after(
        with_created(pr_ship("second"), "2026-01-02T00:00:00Z"),
        "first",
        UnblockPoint::Merged,
    );
    let mut feature = with_created(
        group("feature", vec![first, second]),
        "2026-01-01T00:00:00Z",
    );
    file_mut(&mut feature).areas = tags(&["editor"]);
    let mut later = with_created(leaf("later", Status::Open), "2026-01-03T00:00:00Z");
    file_mut(&mut later).todos = vec![todo("write", Some(TodoRole::ProducesBranch))];
    let later = after(later, "feature", UnblockPoint::Merged);
    let finished = done(
        with_created(pr_ship("finished"), "2026-01-04T00:00:00Z"),
        &[0, 1, 2],
    );
    let tree = tree(vec![later, feature, finished]);

    let feature_id = id(&id_of(&tree, "feature"));
    let first_id = id(&id_of(&tree, "first"));
    let second_id = id(&id_of(&tree, "second"));
    let later_id = id(&id_of(&tree, "later"));
    let finished_id = id(&id_of(&tree, "finished"));

    let mut live = LiveFacts::default();
    live.active_runs
        .insert(first_id.clone(), "run-1".to_string());
    live.held.insert(later_id.clone(), "moving".to_string());

    let graph = from_work_tree(&tree, "p", &live).unwrap();
    assert!(from_work_tree(&tree, "nope", &live).is_none());
    assert_eq!(
        graph.top(),
        &[feature_id.clone(), later_id.clone(), finished_id.clone()]
    );

    let feature = graph.task(&feature_id).unwrap();
    assert_eq!(feature.shape, Shape::Group);
    assert_eq!(feature.children, vec![first_id.clone(), second_id.clone()]);

    let first = graph.task(&first_id).unwrap();
    assert_eq!(first.shape, Shape::Leaf);
    assert_eq!(first.state, TaskState::Ready);
    assert_eq!(first.active_run.as_deref(), Some("run-1"));
    let open = first.open_todo.as_ref().unwrap();
    assert_eq!(
        (open.index, open.workflow.as_deref()),
        (0, Some("implement"))
    );
    let expected: BTreeSet<String> = tags(&["editor", "ui"]).into_iter().collect();
    assert_eq!(graph.areas(&first_id), expected);

    let second = graph.task(&second_id).unwrap();
    assert_eq!(second.edges.len(), 1);
    assert_eq!(second.edges[0].target, first_id);
    assert!(!second.edges[0].reached);
    assert_eq!(second.edges[0].progress, "ready; needs merged");

    let later = graph.task(&later_id).unwrap();
    assert_eq!(later.held.as_deref(), Some("moving"));
    assert_eq!(later.edges[0].target, feature_id);
    assert_eq!(later.edges[0].progress, "step 1 of 2 not done");

    assert!(graph.task(&finished_id).unwrap().is_finished());
    assert_eq!(graph.dependents(&feature_id), vec![later_id.clone()]);
    assert_eq!(critical_path(&graph), 3);
}
