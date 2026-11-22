use std::collections::{BTreeMap, BTreeSet};

use crate::graph::*;
use crate::picker::*;
use crate::rank::critical_path;
use crate::stamp::{self, StampFile};
use crate::state::*;
use crate::table::{LaneTable, OwnerMode, parse};

fn id(text: &str) -> TaskId {
    TaskId::new(text)
}

fn lane(kind: &str, index: u32) -> LaneId {
    LaneId::new(kind, index)
}

/// A ready leaf whose first todo names `workflow`.
fn todo_leaf(name: &str, workflow: Option<&str>) -> PickTask {
    let mut task = PickTask::leaf(name);
    task.open_todo = Some(OpenTodo {
        index: 0,
        name: "implement".to_string(),
        workflow: workflow.map(str::to_string),
    });
    task
}

fn w(name: &str) -> PickTask {
    todo_leaf(name, Some("w"))
}

fn after(mut task: PickTask, target: &str, reached: bool, progress: &str) -> PickTask {
    task.edges.push(PickEdge {
        kind: EdgeKind::After,
        target: id(target),
        unblock: "merged".to_string(),
        reached,
        progress: progress.to_string(),
    });
    task
}

fn graph(top: &[&str], tasks: Vec<PickTask>) -> PickGraph {
    PickGraph::new(top.iter().map(|t| id(t)).collect(), tasks)
}

fn table(body: &str) -> LaneTable {
    parse(&format!("schema = 1\n{body}")).unwrap()
}

fn builds(count: u32) -> LaneTable {
    table(&format!("[[kind]]\nname = \"build\"\ncount = {count}\n"))
}

fn owner(state: OwnerState, on: Option<LaneId>) -> OwnerRecord {
    OwnerRecord {
        lane: on,
        state,
        since: "t0".to_string(),
        by: Author::System,
        reason: None,
        prepared_commit: None,
        refreshed: false,
    }
}

fn owners(list: &[(&str, OwnerState, Option<LaneId>)]) -> LanesState {
    let mut state = LanesState::default();
    for (task, owner_state, on) in list {
        state
            .owners
            .insert(id(task), owner(*owner_state, on.clone()));
    }
    state
}

fn pick_on(graph: &PickGraph, state: &LanesState, table: &LaneTable, on: &LaneId) -> PickOutcome {
    pick(graph, state, table, on, &PickOptions::default())
}

#[track_caller]
fn assignment(outcome: &PickOutcome) -> &Assignment {
    match outcome {
        PickOutcome::Assign(assignment) => assignment,
        other => panic!("expected an assignment, got {other:?}"),
    }
}

#[track_caller]
fn picked(outcome: &PickOutcome) -> (String, String) {
    let assignment = assignment(outcome);
    match &assignment.work {
        AssignedWork::Leaf { task, .. } => (task.0.clone(), assignment.workflow.clone()),
        other => panic!("expected a leaf, got {other:?}"),
    }
}

#[track_caller]
fn reasons(outcome: &PickOutcome) -> Vec<(String, WaitReason)> {
    match outcome {
        PickOutcome::Nothing { reasons, .. } => reasons
            .iter()
            .map(|waiting| (waiting.task.0.clone(), waiting.reason.clone()))
            .collect(),
        other => panic!("expected nothing, got {other:?}"),
    }
}

fn pair(task: &str, workflow: &str) -> (String, String) {
    (task.to_string(), workflow.to_string())
}

// A leaf, its workflow and the reason, reading only the graph and run state.
#[test]
fn picks_the_best_ranked_leaf_with_its_workflow_and_reason() {
    let graph = graph(&["a", "b"], vec![w("a"), w("b")]);
    let outcome = pick_on(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
    );
    assert_eq!(picked(&outcome), pair("a", "w"));
    let assignment = assignment(&outcome);
    assert_eq!(assignment.reason, "not started; chain 1 job; order 1");
    assert_eq!(
        assignment.work,
        AssignedWork::Leaf {
            task: id("a"),
            todo_index: Some(0)
        }
    );
    assert!(!assignment.refresh_first);
    assert_eq!(
        assignment.owner_change,
        vec![OwnerChange {
            task: id("a"),
            from: None,
            from_lane: None,
            to: OwnerState::Active,
            lane: Some(lane("build", 1)),
        }]
    );
}

// Changing one edge changes the next pick; rank is never stored.
#[test]
fn changing_one_edge_changes_the_next_pick() {
    let table = builds(1);
    let state = LanesState::default();
    let without = graph(&["a", "b", "c"], vec![w("a"), w("b"), w("c")]);
    assert_eq!(
        picked(&pick_on(&without, &state, &table, &lane("build", 1))),
        pair("a", "w")
    );

    let with = graph(
        &["a", "b", "c"],
        vec![
            w("a"),
            w("b"),
            after(w("c"), "b", false, "ready; needs merged"),
        ],
    );
    let before = with.clone();
    let first = pick_on(&with, &state, &table, &lane("build", 1));
    assert_eq!(picked(&first), pair("b", "w"));
    assert_eq!(
        assignment(&first).reason,
        "not started; chain 2 jobs; order 2"
    );
    // Picking again gives the same answer and leaves the graph as it was.
    assert_eq!(pick_on(&with, &state, &table, &lane("build", 1)), first);
    assert_eq!(with, before);
}

#[test]
fn started_tasks_come_first() {
    let mut b = w("b");
    b.started = true;
    let graph = graph(
        &["a", "b", "c"],
        vec![w("a"), b, after(w("c"), "a", false, "")],
    );
    let outcome = pick_on(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
    );
    assert_eq!(picked(&outcome), pair("b", "w"));
    assert_eq!(
        assignment(&outcome).reason,
        "started first; chain 1 job; order 2"
    );
}

#[test]
fn ranks_level_by_level_inside_a_group() {
    // Inside G, g2 has the longer chain (g3 depends on it), so it goes before g1.
    let graph = graph(
        &["G"],
        vec![
            PickTask::group("G", &["g1", "g2", "g3"]),
            w("g1"),
            w("g2"),
            after(w("g3"), "g2", false, ""),
        ],
    );
    let outcome = pick_on(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
    );
    assert_eq!(picked(&outcome), pair("g2", "w"));
    assert_eq!(
        assignment(&outcome).reason,
        "not started; chain 3 jobs; order 3"
    );
}

fn rule_table() -> LaneTable {
    table(
        r#"
[[kind]]
name = "build"
rules = ["step", "any"]

[[kind]]
name = "design"
rules = ["prepare"]

[rules.step]
work_kind = "step"
workflow = "implement step"
inputs = { step = "task.goal" }

[rules.any]
state = ["ready", "in-progress"]
use_todo_workflow = true

[rules.prepare]
work_kind = "design"
workflow = "prepare"
"#,
    )
}

fn single(task: PickTask) -> PickGraph {
    let top = task.id.0.clone();
    graph(&[&top], vec![task])
}

fn with_kind(mut task: PickTask, kind: &str) -> PickTask {
    task.work_kind = Some(kind.to_string());
    task
}

// First rule wins, the todo's workflow as fallback, "no workflow".
#[test]
fn the_first_matching_rule_wins() {
    let table = rule_table();
    let state = LanesState::default();
    let build = lane("build", 1);

    let step = single(with_kind(todo_leaf("s", Some("todo wf")), "step"));
    let outcome = pick_on(&step, &state, &table, &build);
    assert_eq!(picked(&outcome), pair("s", "implement step"));
    assert_eq!(
        assignment(&outcome).inputs,
        vec![("step".to_string(), "task.goal".to_string())]
    );

    let other = single(todo_leaf("o", Some("todo wf")));
    assert_eq!(
        picked(&pick_on(&other, &state, &table, &build)),
        pair("o", "todo wf")
    );

    let none = single(todo_leaf("n", None));
    assert_eq!(
        reasons(&pick_on(&none, &state, &table, &build)),
        vec![("n".to_string(), WaitReason::NoWorkflow)]
    );

    // A blocked leaf matches no rule of build (the `any` rule wants ready or in progress).
    let mut blocked = todo_leaf("x", Some("todo wf"));
    blocked.state = TaskState::Blocked;
    assert_eq!(
        reasons(&pick_on(&single(blocked), &state, &table, &build)),
        vec![("x".to_string(), WaitReason::NoWorkflow)]
    );
}

#[test]
fn a_leaf_only_another_kind_takes_names_that_kind() {
    let table = rule_table();
    let state = LanesState::default();
    let design = single(with_kind(todo_leaf("d", None), "design"));
    assert_eq!(
        reasons(&pick_on(&design, &state, &table, &lane("build", 1))),
        vec![(
            "d".to_string(),
            WaitReason::LaneKind {
                kind: "design".to_string()
            }
        )]
    );
    assert_eq!(
        picked(&pick_on(&design, &state, &table, &lane("design", 1))),
        pair("d", "prepare")
    );
    let step = single(with_kind(todo_leaf("s", None), "step"));
    assert_eq!(
        reasons(&pick_on(&step, &state, &table, &lane("design", 1))),
        vec![(
            "s".to_string(),
            WaitReason::LaneKind {
                kind: "build".to_string()
            }
        )]
    );
}

#[test]
fn with_no_rules_the_todo_names_the_workflow() {
    let table = LaneTable::implicit();
    let state = LanesState::default();
    let on = lane("default", 1);
    assert_eq!(
        picked(&pick_on(
            &single(todo_leaf("a", Some("ship"))),
            &state,
            &table,
            &on
        )),
        pair("a", "ship")
    );
    assert_eq!(
        reasons(&pick_on(&single(todo_leaf("a", None)), &state, &table, &on)),
        vec![("a".to_string(), WaitReason::NoWorkflow)]
    );
    assert_eq!(
        reasons(&pick_on(&single(PickTask::leaf("a")), &state, &table, &on)),
        vec![("a".to_string(), WaitReason::NoWorkflow)]
    );
}

#[test]
fn no_group_is_ever_assigned() {
    let table = LaneTable::implicit();
    let state = LanesState::default();
    let on = lane("default", 1);
    let mut group = PickTask::group("G", &["g1", "g2"]);
    group.open_todo = todo_leaf("G", Some("w")).open_todo;
    let mut g1 = w("g1");
    g1.state = TaskState::Done;
    let mut g2 = w("g2");
    g2.state = TaskState::Cancelled;
    let finished = graph(&["G"], vec![group.clone(), g1.clone(), g2]);
    assert_eq!(reasons(&pick_on(&finished, &state, &table, &on)), vec![]);

    let open = graph(&["G"], vec![group, g1, w("g2")]);
    assert_eq!(
        picked(&pick_on(&open, &state, &table, &on)),
        pair("g2", "w")
    );
}

/// A (group: a1 done, a2), B with C after it.
fn owned_graph(a2_run: Option<&str>, a2_state: TaskState) -> PickGraph {
    let mut a1 = w("a1");
    a1.state = TaskState::Done;
    let mut a2 = w("a2");
    a2.active_run = a2_run.map(str::to_string);
    a2.state = a2_state;
    graph(
        &["A", "B", "C"],
        vec![
            PickTask::group("A", &["a1", "a2"]),
            a1,
            a2,
            w("B"),
            after(w("C"), "B", false, ""),
        ],
    )
}

// An owning lane picks only under its task.
#[test]
fn an_owning_lane_picks_only_under_its_task() {
    let table = builds(2);
    let state = owners(&[("A", OwnerState::Active, Some(lane("build", 1)))]);

    let outcome = pick_on(
        &owned_graph(None, TaskState::Ready),
        &state,
        &table,
        &lane("build", 1),
    );
    assert_eq!(picked(&outcome), pair("a2", "w"));
    assert!(assignment(&outcome).owner_change.is_empty());

    // With a2 busy the owner waits; it never takes B.
    let busy = owned_graph(Some("run-7"), TaskState::Ready);
    assert_eq!(
        reasons(&pick_on(&busy, &state, &table, &lane("build", 1))),
        vec![(
            "a2".to_string(),
            WaitReason::ActiveRun {
                run: "run-7".to_string()
            }
        )]
    );

    // Another lane sees A's leaves as owned.
    let other = pick_on(&busy, &state, &table, &lane("build", 2));
    assert_eq!(picked(&other), pair("B", "w"));

    // Once every leaf of A is finished, A is done and the lane is free.
    let finished = owned_graph(None, TaskState::Done);
    let outcome = pick_on(&finished, &state, &table, &lane("build", 1));
    assert_eq!(picked(&outcome), pair("B", "w"));
    assert_eq!(
        assignment(&outcome).owner_change,
        vec![
            OwnerChange {
                task: id("A"),
                from: Some(OwnerState::Active),
                from_lane: Some(lane("build", 1)),
                to: OwnerState::Done,
                lane: Some(lane("build", 1)),
            },
            OwnerChange {
                task: id("B"),
                from: None,
                from_lane: None,
                to: OwnerState::Active,
                lane: Some(lane("build", 1)),
            },
        ]
    );
}

#[test]
fn a_finished_own_task_with_nothing_else_reports_the_release() {
    let table = builds(1);
    let state = owners(&[("A", OwnerState::Active, Some(lane("build", 1)))]);
    let mut a1 = w("a1");
    a1.state = TaskState::Done;
    let graph = graph(&["A"], vec![PickTask::group("A", &["a1"]), a1]);
    match pick_on(&graph, &state, &table, &lane("build", 1)) {
        PickOutcome::Nothing {
            reasons,
            owner_change,
        } => {
            assert!(reasons.is_empty());
            assert_eq!(owner_change.len(), 1);
            assert_eq!(owner_change[0].to, OwnerState::Done);
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

/// Twenty-four siblings, each with an edge to every other: every pair is a cycle.
fn clique() -> Vec<PickTask> {
    let names: Vec<String> = (0..24).map(|index| format!("n{index:02}")).collect();
    names
        .iter()
        .map(|name| {
            let mut task = w(name);
            for other in names.iter().filter(|other| *other != name) {
                task = after(task, other, false, "waits");
            }
            task
        })
        .collect()
}

#[test]
fn edge_cycles_terminate_and_do_not_depend_on_input_order() {
    let table = builds(1);
    let top: Vec<String> = (0..24).map(|index| format!("n{index:02}")).collect();
    let top_refs: Vec<&str> = top.iter().map(String::as_str).collect();
    let tasks = clique();
    let expected_graph = graph(&top_refs, tasks.clone());
    let expected_path = critical_path(&expected_graph);
    let expected_pick = pick_on(
        &expected_graph,
        &LanesState::default(),
        &table,
        &lane("build", 1),
    );
    let mut rng = Rng(0xc1c1e);
    for _ in 0..5 {
        let mut shuffled = tasks.clone();
        rng.shuffle(&mut shuffled);
        let graph = graph(&top_refs, shuffled);
        assert_eq!(critical_path(&graph), expected_path);
        assert_eq!(
            pick_on(&graph, &LanesState::default(), &table, &lane("build", 1)),
            expected_pick
        );
    }
    assert!(expected_path > 0);
}

#[test]
fn a_duplicated_task_id_is_dropped_whatever_the_input_order() {
    let mut finished = w("b");
    finished.state = TaskState::Done;
    let table = builds(1);
    for tasks in [
        vec![w("a"), w("b"), finished.clone()],
        vec![finished.clone(), w("b"), w("a")],
        vec![w("b"), w("a"), finished],
    ] {
        let graph = graph(&["a", "b"], tasks);
        assert!(graph.task(&id("b")).is_none());
        let outcome = pick_on(&graph, &LanesState::default(), &table, &lane("build", 1));
        assert_eq!(picked(&outcome), pair("a", "w"));
    }
}

#[test]
fn an_own_task_missing_from_the_graph_is_not_released() {
    let table = builds(1);
    let state = owners(&[("gone", OwnerState::Active, Some(lane("build", 1)))]);
    let graph = graph(&["b"], vec![w("b")]);
    match pick_on(&graph, &state, &table, &lane("build", 1)) {
        PickOutcome::Nothing {
            reasons,
            owner_change,
        } => {
            assert!(owner_change.is_empty());
            assert_eq!(reasons.len(), 1);
            assert_eq!(reasons[0].task, id("gone"));
            assert_eq!(
                reasons[0].reason.to_string(),
                "task is missing from the graph"
            );
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

#[test]
fn an_own_group_without_leaves_is_not_released() {
    let table = builds(1);
    let state = owners(&[("g", OwnerState::Active, Some(lane("build", 1)))]);
    // Both copies of the duplicated id are dropped, leaving the group without a leaf.
    let graph = graph(&["g"], vec![PickTask::group("g", &["d"]), w("d"), w("d")]);
    match pick_on(&graph, &state, &table, &lane("build", 1)) {
        PickOutcome::Nothing {
            reasons,
            owner_change,
        } => {
            assert!(owner_change.is_empty());
            assert_eq!(reasons.len(), 1);
            assert_eq!(reasons[0].task, id("g"));
            assert_eq!(
                reasons[0].reason.to_string(),
                "task has no leaves in the graph"
            );
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

// A stopped task only for its lane, or once that lane no longer exists; halted never.
#[test]
fn a_stopped_task_is_resumed_only_by_its_lane() {
    let graph = graph(&["a", "b"], vec![w("a"), w("b")]);
    let table = builds(2);
    let state = owners(&[("a", OwnerState::Stopped, Some(lane("build", 2)))]);

    let first = pick_on(&graph, &state, &table, &lane("build", 1));
    assert_eq!(picked(&first), pair("b", "w"));

    let own = pick_on(&graph, &state, &table, &lane("build", 2));
    assert_eq!(picked(&own), pair("a", "w"));
    assert_eq!(
        assignment(&own).owner_change,
        vec![OwnerChange {
            task: id("a"),
            from: Some(OwnerState::Stopped),
            from_lane: Some(lane("build", 2)),
            to: OwnerState::Active,
            lane: Some(lane("build", 2)),
        }]
    );

    let nothing_else = PickGraph::new(vec![id("a")], [w("a")]);
    assert_eq!(
        reasons(&pick_on(&nothing_else, &state, &table, &lane("build", 1))),
        vec![(
            "a".to_string(),
            WaitReason::Owner {
                lane: lane("build", 2),
                state: OwnerState::Stopped
            }
        )]
    );
}

#[test]
fn a_stopped_task_frees_once_its_lane_is_gone() {
    let graph = graph(&["a", "b"], vec![w("a"), w("b")]);
    let table = builds(2);
    let state = owners(&[("a", OwnerState::Stopped, Some(lane("build", 2)))]);
    let lowered = PickOptions {
        counts: BTreeMap::from([("build".to_string(), 1)]),
        ..PickOptions::default()
    };
    let outcome = pick(&graph, &state, &table, &lane("build", 1), &lowered);
    // A stopped owner counts as started, so it goes first.
    assert_eq!(picked(&outcome), pair("a", "w"));
    assert_eq!(
        assignment(&outcome).owner_change[0].from_lane,
        Some(lane("build", 2))
    );

    let removed_kind = owners(&[("a", OwnerState::Stopped, Some(lane("docs", 1)))]);
    assert_eq!(
        picked(&pick_on(&graph, &removed_kind, &table, &lane("build", 1))),
        pair("a", "w")
    );
}

#[test]
fn a_halted_task_is_never_a_candidate() {
    let graph = graph(&["a", "b"], vec![w("a"), w("b")]);
    let table = builds(2);
    let state = owners(&[("a", OwnerState::Halted, Some(lane("build", 2)))]);
    let lowered = PickOptions {
        counts: BTreeMap::from([("build".to_string(), 1)]),
        ..PickOptions::default()
    };
    for opts in [PickOptions::default(), lowered] {
        let outcome = pick(&graph, &state, &table, &lane("build", 1), &opts);
        assert_eq!(picked(&outcome), pair("b", "w"));
    }
    let mut halted = state;
    halted.owners.get_mut(&id("a")).unwrap().reason = Some("tests failed".to_string());
    assert_eq!(
        pick_on(&graph, &halted, &table, &lane("build", 2)),
        PickOutcome::Hold(LaneHold {
            task: id("a"),
            reason: "tests failed".to_string()
        })
    );
}

#[test]
fn a_prepared_task_is_free_for_any_lane() {
    let graph = graph(&["a"], vec![w("a")]);
    let state = owners(&[("a", OwnerState::Prepared, None)]);
    let outcome = pick_on(&graph, &state, &builds(1), &lane("build", 1));
    assert_eq!(picked(&outcome), pair("a", "w"));
    assert_eq!(
        assignment(&outcome).owner_change[0].from,
        Some(OwnerState::Prepared)
    );
}

// A leaf with an active run is left alone.
#[test]
fn a_leaf_with_an_active_run_is_skipped() {
    let mut a = w("a");
    a.active_run = Some("run-1".to_string());
    let graph = graph(&["a", "b"], vec![a, w("b")]);
    assert_eq!(
        picked(&pick_on(
            &graph,
            &LanesState::default(),
            &builds(1),
            &lane("build", 1)
        )),
        pair("b", "w")
    );
}

#[test]
fn held_tasks_are_skipped() {
    let mut a = w("a");
    a.held = Some("moving".to_string());
    let held_group = {
        let mut group = PickTask::group("G", &["g"]);
        group.held = Some("under a moving claim".to_string());
        group
    };
    let graph = graph(&["a", "G", "b"], vec![a, held_group, w("g"), w("b")]);
    let outcome = pick_on(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
    );
    assert_eq!(picked(&outcome), pair("b", "w"));

    let only_held = PickGraph::new(vec![id("a"), id("G")], {
        let mut a = w("a");
        a.held = Some("moving".to_string());
        let mut group = PickTask::group("G", &["g"]);
        group.held = Some("under a moving claim".to_string());
        vec![a, group, w("g")]
    });
    assert_eq!(
        reasons(&pick_on(
            &only_held,
            &LanesState::default(),
            &builds(1),
            &lane("build", 1)
        )),
        vec![
            (
                "a".to_string(),
                WaitReason::Held {
                    reason: "moving".to_string()
                }
            ),
            (
                "g".to_string(),
                WaitReason::Held {
                    reason: "under a moving claim".to_string()
                }
            ),
        ]
    );
}

#[test]
fn a_dependency_names_the_first_unreached_target_and_its_progress() {
    let b = after(
        after(w("b"), "x", true, ""),
        "a",
        false,
        "step 5 of 8 not done",
    );
    let group = after(
        PickTask::group("G", &["g"]),
        "a",
        false,
        "step 5 of 8 not done",
    );
    let mut a = w("a");
    a.active_run = Some("run-1".to_string());
    let graph = graph(&["a", "x", "b", "G"], {
        let mut x = w("x");
        x.state = TaskState::Done;
        vec![a, x, b, group, w("g")]
    });
    let dependency = WaitReason::Dependency {
        on: id("a"),
        progress: "step 5 of 8 not done".to_string(),
    };
    let found = reasons(&pick_on(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
    ));
    assert!(found.contains(&("b".to_string(), dependency.clone())));
    assert!(found.contains(&("g".to_string(), dependency.clone())));
    assert_eq!(dependency.to_string(), "waits on a: step 5 of 8 not done");
}

#[test]
fn pins_keep_tasks_on_their_lane() {
    let graph = graph(&["a"], vec![w("a")]);
    let pinned = table(
        "[[kind]]\nname = \"build\"\ncount = 2\n[[kind]]\nname = \"design\"\n[[pin]]\ntask = \"a\"\nlane = \"build-2\"\n",
    );
    let state = LanesState::default();
    assert_eq!(
        reasons(&pick_on(&graph, &state, &pinned, &lane("build", 1))),
        vec![(
            "a".to_string(),
            WaitReason::Pin {
                to: "build-2".to_string()
            }
        )]
    );
    assert_eq!(
        picked(&pick_on(&graph, &state, &pinned, &lane("build", 2))),
        pair("a", "w")
    );

    let mut paused = state.clone();
    paused.lanes.insert(
        lane("build", 2),
        LaneRecord::new(&lane("build", 2), LaneState::Paused),
    );
    assert_eq!(
        reasons(&pick_on(&graph, &paused, &pinned, &lane("build", 1))),
        vec![(
            "a".to_string(),
            WaitReason::PinNotRunning {
                to: "build-2".to_string()
            }
        )]
    );

    let by_kind = table(
        "[[kind]]\nname = \"build\"\n[[kind]]\nname = \"design\"\n[[pin]]\ntask = \"a\"\nkind = \"design\"\n",
    );
    assert_eq!(
        reasons(&pick_on(&graph, &state, &by_kind, &lane("build", 1))),
        vec![(
            "a".to_string(),
            WaitReason::Pin {
                to: "design".to_string()
            }
        )]
    );
}

#[test]
fn the_instance_limit_counts_runs_reservations_and_earlier_assignments() {
    let graph = graph(&["a"], vec![w("a")]);
    let opts = PickOptions {
        instances: BTreeMap::from([(
            "w".to_string(),
            InstanceUse {
                limit: Some(2),
                active_runs: vec!["run-9".to_string()],
                reserved: 1,
            },
        )]),
        ..PickOptions::default()
    };
    let outcome = pick(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
        &opts,
    );
    assert_eq!(
        reasons(&outcome),
        vec![(
            "a".to_string(),
            WaitReason::InstanceLimit {
                workflow: "w".to_string(),
                active: 2,
                limit: 2,
                run: Some("run-9".to_string())
            }
        )]
    );

    let roomy = PickOptions {
        instances: BTreeMap::from([(
            "w".to_string(),
            InstanceUse {
                limit: Some(1),
                ..InstanceUse::default()
            },
        )]),
        ..PickOptions::default()
    };
    let graph = graph_two();
    let dry = explain(&graph, &LanesState::default(), &builds(2), &roomy);
    assert_eq!(picked(&dry.per_lane[0].1), pair("a", "w"));
    assert!(reasons(&dry.per_lane[1].1).contains(&(
        "b".to_string(),
        WaitReason::InstanceLimit {
            workflow: "w".to_string(),
            active: 1,
            limit: 1,
            run: None
        }
    )));
}

fn graph_two() -> PickGraph {
    graph(&["a", "b"], vec![w("a"), w("b")])
}

#[test]
fn the_bound_keeps_picks_inside_a_group() {
    let graph = graph(
        &["a", "G"],
        vec![w("a"), PickTask::group("G", &["g"]), w("g")],
    );
    let opts = PickOptions {
        bound: Some(PickBound::Within(id("G"))),
        ..PickOptions::default()
    };
    let outcome = pick(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
        &opts,
    );
    assert_eq!(picked(&outcome), pair("g", "w"));

    let until = PickOptions {
        bound: Some(PickBound::Until(id("a"))),
        ..PickOptions::default()
    };
    let mut a = w("a");
    a.active_run = Some("run-1".to_string());
    let graph = PickGraph::new(
        vec![id("a"), id("G")],
        [a, PickTask::group("G", &["g"]), w("g")],
    );
    let found = reasons(&pick(
        &graph,
        &LanesState::default(),
        &builds(1),
        &lane("build", 1),
        &until,
    ));
    assert!(found.contains(&("g".to_string(), WaitReason::Bound)));
}

#[test]
fn the_dry_run_lets_later_lanes_see_earlier_claims() {
    let graph = graph(&["a", "b", "c"], vec![w("a"), w("b"), w("c")]);
    let dry = explain(
        &graph,
        &LanesState::default(),
        &builds(2),
        &PickOptions::default(),
    );
    assert_eq!(dry.per_lane.len(), 2);
    assert_eq!(dry.per_lane[0].0, lane("build", 1));
    assert_eq!(picked(&dry.per_lane[0].1), pair("a", "w"));
    assert_eq!(picked(&dry.per_lane[1].1), pair("b", "w"));
    assert_eq!(dry.ready, 3);
    assert!(dry.waiting.is_empty());
    assert_eq!(dry.critical_path, 1);
    assert_eq!(dry.parallel, 2, "capped by the two live lanes");
}

// First use: with no ready leaf, `pick` gives every reason and the dry run gives each lane its
// idle reason.
#[test]
fn with_no_ready_leaf_every_reason_is_reported() {
    let mut a = w("a");
    a.active_run = Some("run-1".to_string());
    let mut b = w("b");
    b.held = Some("moving".to_string());
    let c = after(w("c"), "a", false, "in-progress; needs merged");
    let d = todo_leaf("d", None);
    let mut e = w("e");
    e.state = TaskState::Done;
    let graph = graph(&["a", "b", "c", "d", "e"], vec![a, b, c, d, e]);
    let table = builds(2);
    let state = LanesState::default();

    let expected = vec![
        (
            "a".to_string(),
            WaitReason::ActiveRun {
                run: "run-1".to_string(),
            },
        ),
        (
            "b".to_string(),
            WaitReason::Held {
                reason: "moving".to_string(),
            },
        ),
        (
            "c".to_string(),
            WaitReason::Dependency {
                on: id("a"),
                progress: "in-progress; needs merged".to_string(),
            },
        ),
        ("d".to_string(), WaitReason::NoWorkflow),
    ];
    // a has the longest chain (c depends on it), so it ranks first.
    assert_eq!(
        reasons(&pick_on(&graph, &state, &table, &lane("build", 1))),
        expected
    );

    let dry = explain(&graph, &state, &table, &PickOptions::default());
    assert_eq!(dry.per_lane.len(), 2);
    for (_, outcome) in &dry.per_lane {
        assert_eq!(reasons(outcome), expected);
    }
    let waiting: Vec<(String, WaitReason)> = dry
        .waiting
        .iter()
        .map(|waiting| (waiting.task.0.clone(), waiting.reason.clone()))
        .collect();
    assert_eq!(waiting, expected);
    assert_eq!(dry.ready, 0);
    assert_eq!(dry.critical_path, 2);

    let limited = explain(
        &graph,
        &state,
        &table,
        &PickOptions {
            waiting_limit: Some(2),
            ..PickOptions::default()
        },
    );
    assert_eq!(limited.waiting.len(), 2);

    let empty = PickGraph::new(Vec::new(), Vec::new());
    let dry = explain(&empty, &state, &table, &PickOptions::default());
    assert!(
        dry.per_lane
            .iter()
            .all(|(_, outcome)| reasons(outcome).is_empty())
    );
    assert_eq!((dry.ready, dry.critical_path, dry.parallel), (0, 0, 0));
}

// ---------------------------------------------------------------------------------------------
// Property: the result depends on the input, never its order
// ---------------------------------------------------------------------------------------------

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below(index + 1);
            items.swap(index, other);
        }
    }
}

struct Case {
    top: Vec<TaskId>,
    tasks: Vec<PickTask>,
    table: LaneTable,
    state: LanesState,
    opts: PickOptions,
}

fn random_task(rng: &mut Rng, name: String, siblings: &[String]) -> PickTask {
    let mut task = PickTask::leaf(&name);
    task.state = *rng.pick(&[
        TaskState::Ready,
        TaskState::Ready,
        TaskState::InProgress,
        TaskState::Blocked,
        TaskState::Done,
        TaskState::Cancelled,
    ]);
    task.started = rng.chance(25);
    task.active_run = rng.chance(10).then(|| format!("run-{name}"));
    task.held = rng.chance(8).then(|| "moving".to_string());
    task.work_kind = (*rng.pick(&[None, Some("step"), Some("design")])).map(str::to_string);
    task.size = *rng.pick(&[None, None, Some(1), Some(3)]);
    for tag in ["ui", "lsp", "db"] {
        if rng.chance(30) {
            task.areas.push(tag.to_string());
        }
    }
    task.open_todo = rng.chance(85).then(|| OpenTodo {
        index: rng.below(3),
        name: rng.pick(&["implement", "review"]).to_string(),
        workflow: (*rng.pick(&[None, Some("w1"), Some("w2")])).map(str::to_string),
    });
    for target in siblings {
        if rng.chance(30) {
            task.edges.push(PickEdge {
                kind: if rng.chance(50) {
                    EdgeKind::After
                } else {
                    EdgeKind::StackedOn
                },
                target: TaskId::new(target.clone()),
                unblock: "merged".to_string(),
                reached: rng.chance(50),
                progress: format!("step 1 of 2 not done ({target})"),
            });
        }
    }
    task
}

fn random_case(rng: &mut Rng) -> Case {
    let mut tasks = Vec::new();
    let mut top: Vec<String> = Vec::new();
    for index in 0..1 + rng.below(6) {
        let name = format!("t{index}");
        let mut task = random_task(rng, name.clone(), &top);
        if rng.chance(50) {
            let mut kids: Vec<String> = Vec::new();
            for child in 0..1 + rng.below(4) {
                let kid = random_task(rng, format!("{name}.{child}"), &kids);
                kids.push(kid.id.0.clone());
                tasks.push(kid);
            }
            task.shape = Shape::Group;
            task.state = TaskState::Ready;
            task.children = kids.iter().map(|kid| TaskId::new(kid.clone())).collect();
        }
        top.push(name);
        tasks.push(task);
    }

    let build_rules = rng.pick(&["[\"step\", \"any\"]", "[]", "[\"any\"]"]);
    let build_areas = rng.pick(&["", "areas = false\n"]);
    let design_ahead = rng.pick(&["", "ahead = { cap = 1 }\n", "ahead = { cap = 2 }\n"]);
    let mut text = format!(
        "[[kind]]\nname = \"build\"\ncount = {}\nrules = {build_rules}\n{build_areas}[[kind]]\nname = \"design\"\ncount = {}\nrules = [\"prepare\"]\n{design_ahead}",
        1 + rng.below(3),
        rng.below(3),
    );
    text.push_str("[rules.step]\nwork_kind = \"step\"\nworkflow = \"w1\"\n");
    text.push_str("[rules.any]\nstate = [\"ready\", \"in-progress\"]\nuse_todo_workflow = true\n");
    text.push_str("[rules.prepare]\nwork_kind = \"design\"\nworkflow = \"w2\"\n");
    for _ in 0..rng.below(3) {
        let task = rng.pick(&top).clone();
        let target = rng.pick(&[
            "lane = \"build-1\"",
            "lane = \"build-2\"",
            "kind = \"design\"",
        ]);
        text.push_str(&format!("[[pin]]\ntask = \"{task}\"\n{target}\n"));
    }
    for _ in 0..rng.below(3) {
        if rng.chance(50) {
            let tag = rng.pick(&["ui", "lsp", "db"]);
            text.push_str(&format!("[[exempt]]\ntags = [\"{tag}\"]\n"));
        } else {
            let a = rng.pick(&top).clone();
            let b = rng.pick(&top).clone();
            text.push_str(&format!("[[exempt]]\ntasks = [\"{a}\", \"{b}\"]\n"));
        }
    }
    let table = table(&text);

    let lanes = [
        Some(lane("build", 1)),
        Some(lane("build", 2)),
        Some(lane("build", 3)),
        Some(lane("design", 1)),
        Some(lane("docs", 1)),
        None,
    ];
    let mut state = LanesState::default();
    for name in &top {
        if rng.chance(40) {
            let owner_state = *rng.pick(&[
                OwnerState::Active,
                OwnerState::Stopped,
                OwnerState::Halted,
                OwnerState::Ahead,
                OwnerState::Prepared,
                OwnerState::Done,
            ]);
            state.owners.insert(
                TaskId::new(name.clone()),
                owner(owner_state, rng.pick(&lanes).clone()),
            );
        }
    }
    for on in [lane("build", 1), lane("build", 2)] {
        if rng.chance(50) {
            let lane_state = *rng.pick(&[LaneState::Idle, LaneState::Running, LaneState::Paused]);
            state
                .lanes
                .insert(on.clone(), LaneRecord::new(&on, lane_state));
        }
    }

    let mut opts = PickOptions {
        waiting_limit: Some(1 + rng.below(6)),
        ..PickOptions::default()
    };
    if rng.chance(50) {
        opts.instances.insert(
            "w1".to_string(),
            InstanceUse {
                limit: Some(1 + rng.below(2) as u32),
                active_runs: if rng.chance(30) {
                    vec!["run-x".to_string()]
                } else {
                    Vec::new()
                },
                reserved: rng.below(2) as u32,
            },
        );
    }
    if rng.chance(20) {
        let bound_task = TaskId::new(rng.pick(&top).clone());
        opts.bound = Some(if rng.chance(50) {
            PickBound::Within(bound_task)
        } else {
            PickBound::Until(bound_task)
        });
    }
    if rng.chance(30) {
        opts.counts.insert("build".to_string(), rng.below(3) as u32);
    }

    // Sometimes a recorded stamp: the graph's own, or one from before an edge was added.
    if rng.chance(30) {
        let mut recorded_tasks = tasks.clone();
        if rng.chance(50)
            && let Some(task) = recorded_tasks.last_mut()
        {
            task.edges.push(PickEdge {
                kind: EdgeKind::After,
                target: TaskId::new("elsewhere"),
                unblock: "merged".to_string(),
                reached: true,
                progress: String::new(),
            });
        }
        let recorded = PickGraph::new(
            top.iter().map(|name| TaskId::new(name.clone())).collect(),
            recorded_tasks,
        );
        opts.stamp = Some(StampFile::from_stamp(&stamp::compute(&recorded, &table)));
    }

    // Sometimes list a task id a second time, with different data.
    if rng.chance(25) {
        let mut copy = rng.pick(&tasks).clone();
        copy.name.push_str(" (copy)");
        copy.state = if copy.state == TaskState::Done {
            TaskState::Ready
        } else {
            TaskState::Done
        };
        tasks.push(copy);
    }

    Case {
        top: top.into_iter().map(TaskId::new).collect(),
        tasks,
        table,
        state,
        opts,
    }
}

type Results = (Vec<(LaneId, PickOutcome)>, DryRun);

fn results(case: &Case, tasks: Vec<PickTask>, table: &LaneTable) -> Results {
    let graph = PickGraph::new(case.top.clone(), tasks);
    let mut lanes = table_lanes(table, &case.opts);
    lanes.push(lane("build", 3));
    lanes.push(lane("docs", 1));
    let picks = lanes
        .into_iter()
        .map(|on| {
            let outcome = pick(&graph, &case.state, table, &on, &case.opts);
            (on, outcome)
        })
        .collect();
    (picks, explain(&graph, &case.state, table, &case.opts))
}

#[test]
fn results_do_not_depend_on_input_order() {
    let mut rng = Rng(0x5eed_1a9e_2026_1121);
    let mut assigned = 0;
    for _ in 0..300 {
        let case = random_case(&mut rng);
        let expected = results(&case, case.tasks.clone(), &case.table);
        for _ in 0..3 {
            let mut tasks = case.tasks.clone();
            rng.shuffle(&mut tasks);
            let mut table = case.table.clone();
            rng.shuffle(&mut table.kinds);
            rng.shuffle(&mut table.rules);
            rng.shuffle(&mut table.pins);
            rng.shuffle(&mut table.exemptions);
            assert_eq!(results(&case, tasks, &table), expected);
        }
        assigned += expected
            .0
            .iter()
            .filter(|(_, outcome)| matches!(outcome, PickOutcome::Assign(_)))
            .count();
    }
    // The generator reaches assignments as well as waits.
    assert!(assigned > 0);
}

// ---------------------------------------------------------------------------------------------
// Areas, pins, ahead kinds, staleness and the stamp
// ---------------------------------------------------------------------------------------------

fn tagged(mut task: PickTask, tags: &[&str]) -> PickTask {
    task.areas = tags.iter().map(|tag| tag.to_string()).collect();
    task
}

fn area(tag: &str, with: &str, where_: &str) -> WaitReason {
    WaitReason::Area {
        tag: tag.to_string(),
        with: id(with),
        where_: where_.to_string(),
    }
}

fn with_lane(mut state: LanesState, on: LaneId, lane_state: LaneState) -> LanesState {
    state
        .lanes
        .insert(on.clone(), LaneRecord::new(&on, lane_state));
    state
}

fn area_graph() -> PickGraph {
    graph(
        &["a", "b", "c"],
        vec![
            tagged(w("a"), &["ui"]),
            tagged(w("b"), &["db", "ui"]),
            tagged(w("c"), &["db"]),
        ],
    )
}

fn area_pair() -> PickGraph {
    graph(
        &["a", "b"],
        vec![tagged(w("a"), &["ui"]), tagged(w("b"), &["db", "ui"])],
    )
}

// First area rule: a lane takes a new task only when no conflicting task is in flight on
// another lane, and the reason names the tag, the task and where it is.
#[test]
fn a_new_task_waits_for_a_conflicting_task_in_flight_on_another_lane() {
    let table = builds(2);
    let state = owners(&[("a", OwnerState::Active, Some(lane("build", 2)))]);
    assert_eq!(
        picked(&pick_on(&area_graph(), &state, &table, &lane("build", 1))),
        pair("c", "w")
    );
    let found = reasons(&pick_on(&area_pair(), &state, &table, &lane("build", 1)));
    let expected = area("ui", "a", "in flight on build-2");
    assert!(
        found.contains(&("b".to_string(), expected.clone())),
        "{found:?}"
    );
    assert_eq!(
        expected.to_string(),
        "area ui shared with a, in flight on build-2"
    );

    // From an empty state the dry run lets the second lane see the first one's claim.
    let dry = explain(
        &area_pair(),
        &LanesState::default(),
        &table,
        &PickOptions::default(),
    );
    assert_eq!(picked(&dry.per_lane[0].1), pair("a", "w"));
    assert!(
        reasons(&dry.per_lane[1].1)
            .contains(&("b".to_string(), area("ui", "a", "in flight on build-1")))
    );
    assert_eq!(dry.waiting.len(), 1);
    assert_eq!(
        dry.waiting[0].reason,
        area("ui", "a", "in flight on build-1")
    );
}

// Halted and stopped tasks count as in flight, and so does an unowned task started past
// its first job.
#[test]
fn halted_and_stopped_tasks_count_as_in_flight() {
    let table = builds(2);
    for (owner_state, where_) in [
        (OwnerState::Halted, "halted on build-2"),
        (OwnerState::Stopped, "stopped on build-2"),
    ] {
        let state = owners(&[("a", owner_state, Some(lane("build", 2)))]);
        let found = reasons(&pick_on(&area_pair(), &state, &table, &lane("build", 1)));
        assert!(
            found.contains(&("b".to_string(), area("ui", "a", where_))),
            "{owner_state}: {found:?}"
        );
    }

    let mut a1 = w("a1");
    a1.state = TaskState::Done;
    let mut a2 = w("a2");
    a2.active_run = Some("run-1".to_string());
    let started = graph(
        &["A", "b"],
        vec![
            tagged(PickTask::group("A", &["a1", "a2"]), &["ui"]),
            a1,
            a2,
            tagged(w("b"), &["ui"]),
        ],
    );
    let found = reasons(&pick_on(
        &started,
        &LanesState::default(),
        &table,
        &lane("build", 1),
    ));
    assert!(
        found.contains(&("b".to_string(), area("ui", "A", "in flight"))),
        "{found:?}"
    );

    // A task stopped on a lane that is gone is picked up first; the conflicting new task waits.
    let state = owners(&[("a", OwnerState::Stopped, Some(lane("build", 3)))]);
    let dry = explain(&area_pair(), &state, &table, &PickOptions::default());
    assert_eq!(picked(&dry.per_lane[0].1), pair("a", "w"));
    assert!(
        reasons(&dry.per_lane[1].1)
            .contains(&("b".to_string(), area("ui", "a", "in flight on build-1")))
    );
}

/// A: group a1, a2; B: group b1, b2; both tagged ui, A first in order.
fn two_owned() -> PickGraph {
    graph(
        &["A", "B"],
        vec![
            tagged(PickTask::group("A", &["a1", "a2"]), &["ui"]),
            w("a1"),
            w("a2"),
            tagged(PickTask::group("B", &["b1", "b2"]), &["ui"]),
            w("b1"),
            w("b2"),
        ],
    )
}

// Second area rule: a job of an owned task waits while a conflicting in-flight task on another
// lane is earlier in order, or has a live job.
#[test]
fn an_owned_job_waits_for_an_earlier_conflicting_task_or_a_live_job() {
    let table = builds(2);
    let state = owners(&[
        ("A", OwnerState::Active, Some(lane("build", 1))),
        ("B", OwnerState::Active, Some(lane("build", 2))),
    ]);
    let graph = two_owned();
    assert_eq!(
        reasons(&pick_on(&graph, &state, &table, &lane("build", 2)))[0],
        ("b1".to_string(), area("ui", "A", "in flight on build-1"))
    );
    assert_eq!(
        picked(&pick_on(&graph, &state, &table, &lane("build", 1))),
        pair("a1", "w")
    );

    let busy = with_lane(state.clone(), lane("build", 2), LaneState::Running);
    assert_eq!(
        reasons(&pick_on(&graph, &busy, &table, &lane("build", 1)))[0],
        ("a1".to_string(), area("ui", "B", "live job on build-2"))
    );

    // A task pair exemption lets both run.
    let exempt = table_with_exemption("tasks = [\"A\", \"B\"]");
    assert_eq!(
        picked(&pick_on(&graph, &state, &exempt, &lane("build", 2))),
        pair("b1", "w")
    );
    assert_eq!(
        picked(&pick_on(&graph, &busy, &exempt, &lane("build", 1))),
        pair("a1", "w")
    );
}

fn table_with_exemption(exempt: &str) -> LaneTable {
    table(&format!(
        "[[kind]]\nname = \"build\"\ncount = 2\n[[exempt]]\n{exempt}\n"
    ))
}

// A kind with `areas = false` ignores the area rule; an exemption by tags lets the
// listed tags run together, one by tasks lets the two tasks run together.
#[test]
fn areas_off_and_exemptions_let_conflicting_tasks_run() {
    let state = owners(&[("a", OwnerState::Active, Some(lane("build", 2)))]);
    let off = table("[[kind]]\nname = \"build\"\ncount = 2\nareas = false\n");
    assert_eq!(
        picked(&pick_on(&area_graph(), &state, &off, &lane("build", 1))),
        pair("b", "w")
    );

    let by_tags = table_with_exemption("tags = [\"ui\", \"billing\"]");
    assert_eq!(
        picked(&pick_on(&area_graph(), &state, &by_tags, &lane("build", 1))),
        pair("b", "w")
    );
    let by_tasks = table_with_exemption("tasks = [\"a\", \"b\"]");
    assert_eq!(
        picked(&pick_on(
            &area_graph(),
            &state,
            &by_tasks,
            &lane("build", 1)
        )),
        pair("b", "w")
    );

    // An exemption of a tag the two do not share, or of another task pair, does not help.
    for other in [
        "tags = [\"db\"]",
        "tasks = [\"a\", \"c\"]",
        "tasks = [\"b\"]",
    ] {
        let table = table_with_exemption(other);
        assert_eq!(
            picked(&pick_on(&area_graph(), &state, &table, &lane("build", 1))),
            pair("c", "w"),
            "{other}"
        );
    }
}

// Pins to a lane and to a kind; a pin to a lane that is not running waits, and no other
// lane takes the task.
#[test]
fn a_pin_to_a_lane_that_is_not_running_never_goes_elsewhere() {
    let graph = graph(&["a"], vec![w("a")]);
    let pinned = |target: &str| {
        table(&format!(
            "[[kind]]\nname = \"build\"\ncount = 2\n[[kind]]\nname = \"design\"\n[[pin]]\ntask = \"a\"\n{target}\n"
        ))
    };
    let not_running = WaitReason::PinNotRunning {
        to: "build-2".to_string(),
    };
    let stopped = with_lane(LanesState::default(), lane("build", 2), LaneState::Stopped);
    let to_lane = pinned("lane = \"build-2\"");
    let dry = explain(&graph, &stopped, &to_lane, &PickOptions::default());
    for (on, outcome) in &dry.per_lane {
        if *on == lane("build", 2) {
            continue;
        }
        let found = reasons(outcome);
        assert_eq!(found, vec![("a".to_string(), not_running.clone())], "{on}");
    }
    assert_eq!(
        not_running.to_string(),
        "pinned to build-2, which is not running"
    );

    // A pin to a lane above the count waits the same way.
    let gone = pinned("lane = \"build-3\"");
    let dry = explain(
        &graph,
        &LanesState::default(),
        &gone,
        &PickOptions::default(),
    );
    assert!(dry.per_lane.iter().all(|(_, outcome)| reasons(outcome)
        == vec![(
            "a".to_string(),
            WaitReason::PinNotRunning {
                to: "build-3".to_string()
            }
        )]));

    // A pin to a kind: only a lane of that kind takes it.
    let to_kind = pinned("kind = \"design\"");
    let dry = explain(
        &graph,
        &LanesState::default(),
        &to_kind,
        &PickOptions::default(),
    );
    assert_eq!(
        reasons(&dry.per_lane[0].1),
        vec![(
            "a".to_string(),
            WaitReason::Pin {
                to: "design".to_string()
            }
        )]
    );
    assert_eq!(dry.per_lane[2].0, lane("design", 1));
    assert_eq!(picked(&dry.per_lane[2].1), pair("a", "w"));
}

fn ahead_table(cap: u32, design_count: u32) -> LaneTable {
    table(&format!(
        r#"
[[kind]]
name = "build"
rules = ["step"]

[[kind]]
name = "design"
count = {design_count}
ahead = {{ cap = {cap} }}
rules = ["prepare"]

[rules.step]
work_kind = "step"
workflow = "implement"

[rules.prepare]
work_kind = "design"
workflow = "prepare"
"#
    ))
}

/// Features F1 to F4, each a design leaf then a step leaf, every one tagged ui.
fn features() -> Vec<PickTask> {
    let mut tasks = Vec::new();
    for n in 1..=4 {
        let name = format!("F{n}");
        let design = format!("{name}.d");
        let step = format!("{name}.s");
        tasks.push(tagged(
            PickTask::group(&name, &[design.as_str(), step.as_str()]),
            &["ui"],
        ));
        tasks.push(with_kind(PickTask::leaf(&design), "design"));
        tasks.push(after(
            with_kind(PickTask::leaf(&step), "step"),
            &design,
            false,
            "ready; needs merged",
        ));
    }
    tasks
}

fn feature_graph(tasks: Vec<PickTask>) -> PickGraph {
    graph(&["F1", "F2", "F3", "F4"], tasks)
}

// The cap counts prepared tasks not yet started plus preparations running.
#[test]
fn the_ahead_cap_counts_prepared_unstarted_and_running_preparations() {
    let table = ahead_table(2, 3);
    let state = owners(&[
        ("F1", OwnerState::Prepared, None),
        ("F2", OwnerState::Ahead, Some(lane("design", 2))),
    ]);
    let cap = WaitReason::AheadCap { n: 2, cap: 2 };
    let found = reasons(&pick_on(
        &feature_graph(features()),
        &state,
        &table,
        &lane("design", 1),
    ));
    assert!(
        found.contains(&("F3.d".to_string(), cap.clone())),
        "{found:?}"
    );
    assert!(found.contains(&("F4.d".to_string(), cap.clone())));
    assert!(found.contains(&(
        "F2.d".to_string(),
        WaitReason::Owner {
            lane: lane("design", 2),
            state: OwnerState::Ahead
        }
    )));
    assert_eq!(
        cap.to_string(),
        "ahead cap reached: 2 prepared or preparing (cap 2)"
    );

    // Once F1 has a started leaf it no longer counts, so one preparation may start.
    let mut tasks = features();
    tasks[1].started = true;
    let graph = feature_graph(tasks);
    let dry = explain(&graph, &state, &table, &PickOptions::default());
    let (_, first) = &dry.per_lane[1];
    assert_eq!(dry.per_lane[1].0, lane("design", 1));
    assert_eq!(picked(first), pair("F3.d", "prepare"));
    assert_eq!(
        assignment(first).owner_change,
        vec![OwnerChange {
            task: id("F3"),
            from: None,
            from_lane: None,
            to: OwnerState::Ahead,
            lane: Some(lane("design", 1)),
        }]
    );
    // design-2 resumes its own preparation, and design-3 sees the cap full again.
    assert_eq!(picked(&dry.per_lane[2].1), pair("F2.d", "prepare"));
    assert!(reasons(&dry.per_lane[3].1).contains(&("F4.d".to_string(), cap)));
}

// An ahead lane needs every edge target of the top-level task finished, not merely
// reached; and area rules do not apply to it.
#[test]
fn an_ahead_lane_needs_finished_targets_and_ignores_areas() {
    let table = ahead_table(2, 1);
    let mut tasks = features();
    // F1 is merged (its edge from F2 is reached) but not done; F2 comes after F1.
    tasks[0].state = TaskState::Merged;
    tasks[1].state = TaskState::Done;
    tasks[3] = after(tasks[3].clone(), "F1", true, "");
    let state = owners(&[("F3", OwnerState::Active, Some(lane("build", 1)))]);

    // F2 ranks before F4 but waits; F4 shares area ui with F3, in flight on build-1, yet the
    // ahead lane prepares it.
    let outcome = pick_on(
        &feature_graph(tasks.clone()),
        &state,
        &table,
        &lane("design", 1),
    );
    assert_eq!(picked(&outcome), pair("F4.d", "prepare"));

    let first_two = graph(&["F1", "F2"], tasks[..6].to_vec());
    let found = reasons(&pick_on(&first_two, &state, &table, &lane("design", 1)));
    assert!(
        found.contains(&(
            "F2.d".to_string(),
            WaitReason::Dependency {
                on: id("F1"),
                progress: "merged reached; an ahead lane waits until it is finished".to_string()
            }
        )),
        "{found:?}"
    );

    tasks[0].state = TaskState::Done;
    let first_two = graph(&["F1", "F2"], tasks[..6].to_vec());
    let outcome = pick_on(&first_two, &state, &table, &lane("design", 1));
    assert_eq!(picked(&outcome), pair("F2.d", "prepare"));
}

// Rule 8: a first job on a prepared, unrefreshed owner whose rule has staleness refreshes first.
#[test]
fn a_prepared_unrefreshed_owner_with_staleness_refreshes_first() {
    let with_staleness = table(
        "[[kind]]\nname = \"build\"\nrules = [\"step\"]\n[rules.step]\nwork_kind = \"step\"\nworkflow = \"implement\"\nstaleness = \"lanes/scripts/stale\"\nrefresh = \"refresh\"\n",
    );
    let without = table(
        "[[kind]]\nname = \"build\"\nrules = [\"step\"]\n[rules.step]\nwork_kind = \"step\"\nworkflow = \"implement\"\n",
    );
    let graph = graph(
        &["F"],
        vec![
            PickTask::group("F", &["F.s"]),
            with_kind(PickTask::leaf("F.s"), "step"),
        ],
    );
    let prepared = owners(&[("F", OwnerState::Prepared, None)]);
    let mut refreshed = prepared.clone();
    refreshed.owners.get_mut(&id("F")).unwrap().refreshed = true;
    let cases = [
        (&prepared, &with_staleness, true),
        (&refreshed, &with_staleness, false),
        (&prepared, &without, false),
        (&LanesState::default(), &with_staleness, false),
    ];
    for (state, table, expected) in cases {
        let outcome = pick_on(&graph, state, table, &lane("build", 1));
        assert_eq!(picked(&outcome), pair("F.s", "implement"));
        assert_eq!(assignment(&outcome).refresh_first, expected);
    }
}

// A stale stamp stops every lane with the diff; a matching stamp or none changes nothing;
// a status change does not make the stamp stale.
#[test]
fn a_stale_stamp_stops_every_lane() {
    let table = builds(2);
    let recorded = StampFile::from_stamp(&stamp::compute(&graph_two(), &table));
    let opts = PickOptions {
        stamp: Some(recorded),
        ..PickOptions::default()
    };
    let state = LanesState::default();
    let fresh = explain(&graph_two(), &state, &table, &opts);
    assert_eq!(
        fresh,
        explain(&graph_two(), &state, &table, &PickOptions::default())
    );

    let mut progressed = w("a");
    progressed.state = TaskState::InProgress;
    progressed.started = true;
    let moved_on = graph(&["a", "b"], vec![progressed, w("b")]);
    assert_eq!(
        picked(&pick(&moved_on, &state, &table, &lane("build", 1), &opts)),
        pair("a", "w")
    );

    let changed = graph(&["a", "b"], vec![w("a"), after(w("b"), "a", true, "")]);
    let halted = owners(&[("a", OwnerState::Halted, Some(lane("build", 2)))]);
    for state in [LanesState::default(), halted] {
        let dry = explain(&changed, &state, &table, &opts);
        for (_, outcome) in &dry.per_lane {
            let found = reasons(outcome);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].0, "b");
            let WaitReason::StaleGraph { diff } = &found[0].1 else {
                panic!("expected a stale graph, got {found:?}");
            };
            assert_eq!(diff.to_string(), "graph changed since the stamp: changed b");
        }
        assert_eq!(dry.ready, 0);
        assert!(matches!(
            dry.waiting[0].reason,
            WaitReason::StaleGraph { .. }
        ));
    }
    // Without a stamp the check is off.
    assert_eq!(
        picked(&pick_on(
            &changed,
            &LanesState::default(),
            &table,
            &lane("build", 1)
        )),
        pair("a", "w")
    );
}

fn reason_kind(reason: &WaitReason) -> &'static str {
    match reason {
        WaitReason::Dependency { .. } => "dependency",
        WaitReason::Area { .. } => "area",
        WaitReason::Owner { .. } => "owner",
        WaitReason::Pin { .. } => "pin",
        WaitReason::PinNotRunning { .. } => "pin not running",
        WaitReason::AheadCap { .. } => "ahead cap",
        WaitReason::StaleGraph { .. } => "stale graph",
        WaitReason::InstanceLimit { .. } => "instance limit",
        WaitReason::NoWorkflow => "no workflow",
        WaitReason::LaneKind { .. } => "lane kind",
        WaitReason::ActiveRun { .. } => "active run",
        WaitReason::Held { .. } => "held",
        WaitReason::Bound => "bound",
        WaitReason::Finished => "finished",
        WaitReason::Environment(_) => "environment",
        WaitReason::Hook { .. } => "hook",
        WaitReason::Failed(_) => "failed",
        WaitReason::Other(_) => "other",
    }
}

fn dry_run_reasons(dry: &DryRun) -> BTreeSet<&'static str> {
    let mut found: BTreeSet<&'static str> = dry
        .waiting
        .iter()
        .map(|waiting| reason_kind(&waiting.reason))
        .collect();
    for (_, outcome) in &dry.per_lane {
        if let PickOutcome::Nothing { reasons, .. } = outcome {
            found.extend(reasons.iter().map(|waiting| reason_kind(&waiting.reason)));
        }
    }
    found
}

// The dry run surfaces every reason the picker gives. Environment and Failed are lane
// record reasons the dispatcher sets; the picker never gives them.
#[test]
fn the_dry_run_surfaces_every_wait_reason() {
    let step = |name: &str| with_kind(PickTask::leaf(name), "step");
    let mut running = step("run");
    running.active_run = Some("run-1".to_string());
    let mut held = step("hold");
    held.held = Some("moving".to_string());
    let tasks = vec![
        tagged(step("own"), &["ui"]),
        tagged(step("ar"), &["ui"]),
        after(step("dep"), "x", false, "ready; needs merged"),
        step("x"),
        step("pn"),
        step("pnr"),
        with_kind(PickTask::leaf("nw"), "docs"),
        running,
        held,
        step("fin"),
        PickTask::group("prep", &["prep.d"]),
        with_kind(PickTask::leaf("prep.d"), "design"),
        PickTask::group("feat", &["feat.d"]),
        with_kind(PickTask::leaf("feat.d"), "design"),
    ];
    let top = [
        "own", "ar", "dep", "x", "pn", "pnr", "nw", "run", "hold", "fin", "prep", "feat",
    ];
    let graph = graph(&top, tasks);
    let mut table = ahead_table(1, 1);
    table.kinds[0].count = 2;
    let pins = parse(
        "schema = 1\n[[pin]]\ntask = \"pn\"\nkind = \"design\"\n[[pin]]\ntask = \"pnr\"\nlane = \"build-3\"\n",
    )
    .unwrap();
    table.pins = pins.pins;
    let state = owners(&[
        ("own", OwnerState::Active, Some(lane("build", 2))),
        ("prep", OwnerState::Prepared, None),
        ("fin", OwnerState::Done, None),
    ]);
    let opts = PickOptions {
        instances: BTreeMap::from([(
            "implement".to_string(),
            InstanceUse {
                limit: Some(1),
                active_runs: vec!["run-9".to_string()],
                reserved: 0,
            },
        )]),
        ..PickOptions::default()
    };
    let dry = explain(&graph, &state, &table, &opts);
    let mut found = dry_run_reasons(&dry);

    // The bound and a stale graph, each in its own dry run.
    let bounded = PickOptions {
        bound: Some(PickBound::Within(id("feat"))),
        ..opts.clone()
    };
    found.extend(dry_run_reasons(&explain(&graph, &state, &table, &bounded)));
    let stale = PickOptions {
        stamp: Some(StampFile::from_stamp(&stamp::compute(
            &PickGraph::new(Vec::new(), Vec::new()),
            &table,
        ))),
        ..opts
    };
    found.extend(dry_run_reasons(&explain(&graph, &state, &table, &stale)));

    let expected: BTreeSet<&'static str> = [
        "dependency",
        "area",
        "owner",
        "pin",
        "pin not running",
        "ahead cap",
        "stale graph",
        "instance limit",
        "no workflow",
        "lane kind",
        "active run",
        "held",
        "bound",
        "finished",
        "other",
    ]
    .into_iter()
    .collect();
    assert_eq!(found, expected);
    // The waiting list itself names the area and the cap.
    let waiting: Vec<&'static str> = dry
        .waiting
        .iter()
        .map(|waiting| reason_kind(&waiting.reason))
        .collect();
    assert!(waiting.contains(&"area"), "{waiting:?}");
}

// ---------------------------------------------------------------------------------------------
// Property: two lanes picking one after the other never take conflicting work
// ---------------------------------------------------------------------------------------------

struct Concurrency {
    graph: PickGraph,
    table: LaneTable,
    state: LanesState,
    opts: PickOptions,
}

const PICKING: [(&str, u32); 4] = [("build", 1), ("build", 2), ("design", 1), ("design", 2)];

fn concurrency_case(rng: &mut Rng) -> Concurrency {
    let mut tasks = Vec::new();
    let mut top: Vec<String> = Vec::new();
    for index in 0..2 + rng.below(4) {
        let name = format!("t{index}");
        let mut task = PickTask::leaf(&name);
        for tag in ["ui", "db"] {
            if rng.chance(35) {
                task.areas.push(tag.to_string());
            }
        }
        for target in &top {
            if rng.chance(25) {
                task.edges.push(PickEdge {
                    kind: EdgeKind::After,
                    target: TaskId::new(target.clone()),
                    unblock: "merged".to_string(),
                    reached: rng.chance(60),
                    progress: String::new(),
                });
            }
        }
        let kids = if rng.chance(60) { 1 + rng.below(3) } else { 0 };
        let mut leaves = Vec::new();
        for child in 0..kids.max(1) {
            let mut leaf = if kids == 0 {
                task.clone()
            } else {
                PickTask::leaf(&format!("{name}.{child}"))
            };
            leaf.work_kind = Some(if child == 0 && kids > 0 && rng.chance(60) {
                "design".to_string()
            } else {
                "step".to_string()
            });
            leaf.state = *rng.pick(&[
                TaskState::Ready,
                TaskState::Ready,
                TaskState::InProgress,
                TaskState::Done,
            ]);
            leaf.started = leaf.state != TaskState::Ready || rng.chance(15);
            leaf.active_run = rng.chance(8).then(|| format!("run-{name}.{child}"));
            leaf.open_todo = rng.chance(80).then(|| OpenTodo {
                index: 0,
                name: "implement".to_string(),
                workflow: Some("w1".to_string()),
            });
            if kids > 0 {
                for tag in ["ui", "db"] {
                    if rng.chance(15) {
                        leaf.areas.push(tag.to_string());
                    }
                }
            }
            leaves.push(leaf);
        }
        if kids > 0 {
            task.shape = Shape::Group;
            task.children = leaves.iter().map(|leaf| leaf.id.clone()).collect();
            task.state = if leaves.iter().all(|leaf| leaf.is_finished()) {
                TaskState::Done
            } else {
                TaskState::InProgress
            };
            tasks.push(task);
            tasks.extend(leaves);
        } else {
            tasks.extend(leaves);
        }
        top.push(name);
    }
    let graph = PickGraph::new(top.iter().map(|t| TaskId::new(t.clone())).collect(), tasks);

    let mut text = format!(
        "{}[[kind]]\nname = \"build\"\ncount = 2\nrules = [\"step\", \"any\"]\n{}[[kind]]\nname = \"design\"\ncount = 2\nrules = [\"prepare\"]\nahead = {{ cap = {} }}\n",
        if rng.chance(15) {
            "owner = \"self\"\n"
        } else {
            ""
        },
        if rng.chance(20) {
            "areas = false\n"
        } else {
            ""
        },
        1 + rng.below(2),
    );
    text.push_str("[rules.step]\nwork_kind = \"step\"\nworkflow = \"w1\"\n");
    text.push_str("[rules.any]\nuse_todo_workflow = true\n");
    text.push_str("[rules.prepare]\nwork_kind = \"design\"\nworkflow = \"w2\"\n");
    if rng.chance(20) {
        text.push_str("[[exempt]]\ntags = [\"ui\"]\n");
    }
    if rng.chance(20) {
        let a = rng.pick(&top).clone();
        let b = rng.pick(&top).clone();
        text.push_str(&format!("[[exempt]]\ntasks = [\"{a}\", \"{b}\"]\n"));
    }
    if rng.chance(15) {
        let task = rng.pick(&top).clone();
        let target = rng.pick(&[
            "lane = \"build-1\"",
            "lane = \"build-2\"",
            "kind = \"build\"",
        ]);
        text.push_str(&format!("[[pin]]\ntask = \"{task}\"\n{target}\n"));
    }
    let table = table(&text);

    // Owner records: at most one holding and one ahead record per lane.
    let units: BTreeSet<TaskId> = graph
        .leaves()
        .map(|leaf| unit_of(&graph, &table, &leaf.id))
        .collect();
    let mut free_lanes = vec![
        Some(lane("build", 1)),
        Some(lane("build", 2)),
        Some(lane("design", 1)),
        Some(lane("design", 2)),
        Some(lane("build", 3)),
        None,
    ];
    rng.shuffle(&mut free_lanes);
    let mut state = LanesState::default();
    for unit in units {
        if !rng.chance(45) {
            continue;
        }
        let owner_state = *rng.pick(&[
            OwnerState::Active,
            OwnerState::Stopped,
            OwnerState::Halted,
            OwnerState::Ahead,
            OwnerState::Prepared,
            OwnerState::Done,
        ]);
        let on = match owner_state {
            OwnerState::Prepared | OwnerState::Done => None,
            _ => match free_lanes.pop() {
                Some(on) => on,
                None => continue,
            },
        };
        state.owners.insert(unit, owner(owner_state, on));
    }
    for (kind, index) in PICKING {
        if rng.chance(40) {
            let on = lane(kind, index);
            let lane_state = *rng.pick(&[LaneState::Idle, LaneState::Running, LaneState::Paused]);
            state
                .lanes
                .insert(on.clone(), LaneRecord::new(&on, lane_state));
        }
    }
    let mut opts = PickOptions::default();
    if rng.chance(30) {
        opts.instances.insert(
            "w1".to_string(),
            InstanceUse {
                limit: Some(1 + rng.below(2) as u32),
                ..InstanceUse::default()
            },
        );
    }
    Concurrency {
        graph,
        table,
        state,
        opts,
    }
}

fn unit_of(graph: &PickGraph, table: &LaneTable, leaf: &TaskId) -> TaskId {
    match table.owner {
        OwnerMode::Top => graph.top_level_of(leaf),
        OwnerMode::SelfOwned => leaf.clone(),
    }
}

fn ahead_count(graph: &PickGraph, state: &LanesState) -> usize {
    state
        .owners
        .iter()
        .filter(|(task, owner)| match owner.state {
            OwnerState::Ahead => true,
            OwnerState::Prepared => graph.task(task).is_some() && !graph.has_started_leaf(task),
            _ => false,
        })
        .count()
}

/// Applies what the dispatcher's transaction writes for an outcome: the owner changes, the
/// instance count and, when `running`, the lane's live job.
fn apply(
    state: &mut LanesState,
    opts: &mut PickOptions,
    on: &LaneId,
    outcome: &PickOutcome,
    running: bool,
) {
    let changes = match outcome {
        PickOutcome::Assign(assignment) => {
            *opts
                .assigned
                .entry(assignment.workflow.clone())
                .or_insert(0) += 1;
            if running {
                state
                    .lanes
                    .insert(on.clone(), LaneRecord::new(on, LaneState::Running));
            }
            &assignment.owner_change
        }
        PickOutcome::Nothing { owner_change, .. } => owner_change,
        PickOutcome::Hold(_) => return,
    };
    for change in changes {
        let record = state
            .owners
            .entry(change.task.clone())
            .or_insert_with(|| owner(change.to, None));
        record.state = change.to;
        record.lane = change.lane.clone();
    }
}

fn leaf_of(outcome: &PickOutcome) -> Option<TaskId> {
    match outcome {
        PickOutcome::Assign(Assignment {
            work: AssignedWork::Leaf { task, .. },
            ..
        }) => Some(task.clone()),
        _ => None,
    }
}

fn area_lane(table: &LaneTable, on: &LaneId) -> bool {
    table
        .kinds
        .iter()
        .find(|kind| kind.name == on.kind)
        .is_some_and(|kind| kind.areas && kind.ahead.is_none())
}

#[test]
fn two_lanes_in_either_order_never_take_conflicting_work() {
    let mut rng = Rng(0x2026_1121_a5ea_0004);
    let mut both_assigned = 0;
    let mut area_checked = 0;
    let mut ahead_started = 0;
    for case_index in 0..400 {
        let case = concurrency_case(&mut rng);
        let cap = case
            .table
            .kinds
            .iter()
            .find_map(|kind| kind.ahead.as_ref())
            .map_or(0, |ahead| ahead.cap as usize);
        for (first, second) in PICKING
            .iter()
            .flat_map(|a| PICKING.iter().map(move |b| (a, b)))
            .filter(|(a, b)| a != b)
        {
            let a = lane(first.0, first.1);
            let b = lane(second.0, second.1);
            for running in [false, true] {
                let mut state = case.state.clone();
                for on in [&a, &b] {
                    state
                        .lanes
                        .insert(on.clone(), LaneRecord::new(on, LaneState::Idle));
                }
                let before = ahead_count(&case.graph, &state);
                let mut opts = case.opts.clone();
                let out_a = pick(&case.graph, &state, &case.table, &a, &opts);
                apply(&mut state, &mut opts, &a, &out_a, running);
                let out_b = pick(&case.graph, &state, &case.table, &b, &opts);
                apply(&mut state, &mut opts, &b, &out_b, running);
                let context = format!("case {case_index}, {a} then {b}, running {running}");

                let after = ahead_count(&case.graph, &state);
                assert!(
                    after <= before.max(cap),
                    "{context}: ahead count {before} -> {after} with cap {cap}"
                );
                if after > before {
                    ahead_started += 1;
                }
                let (Some(leaf_a), Some(leaf_b)) = (leaf_of(&out_a), leaf_of(&out_b)) else {
                    continue;
                };
                both_assigned += 1;
                assert_ne!(leaf_a, leaf_b, "{context}: same leaf");
                let unit_a = unit_of(&case.graph, &case.table, &leaf_a);
                let unit_b = unit_of(&case.graph, &case.table, &leaf_b);
                assert_ne!(unit_a, unit_b, "{context}: same task");
                if area_lane(&case.table, &a) && area_lane(&case.table, &b) {
                    let conflict = area_conflict(
                        &case.table,
                        &unit_a,
                        &case.graph.subtree_areas(&unit_a),
                        &unit_b,
                        &case.graph.subtree_areas(&unit_b),
                    );
                    assert_eq!(conflict, None, "{context}: {leaf_a} and {leaf_b} conflict");
                    if !case.graph.subtree_areas(&unit_a).is_empty() {
                        area_checked += 1;
                    }
                }
            }
        }
    }
    // The generator reaches the interesting cases.
    assert!(both_assigned > 100, "{both_assigned}");
    assert!(area_checked > 20, "{area_checked}");
    assert!(ahead_started > 20, "{ahead_started}");
}
