use std::collections::BTreeMap;

use crate::graph::*;
use crate::picker::*;
use crate::state::*;
use crate::table::{LaneTable, parse};

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

// PRD 7: a leaf, its workflow and the reason, reading only the graph and run state.
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

// PRD 8: changing one edge changes the next pick; rank is never stored.
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

// PRD 15: first rule wins, the todo's workflow as fallback, "no workflow".
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

// PRD 10: an owning lane picks only under its task.
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

// PRD 11: a stopped task only for its lane, or once that lane no longer exists; halted never.
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

// PRD 21: a leaf with an active run is left alone.
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
    let mut text = format!(
        "[[kind]]\nname = \"build\"\ncount = {}\nrules = {build_rules}\n[[kind]]\nname = \"design\"\ncount = {}\nrules = [\"prepare\"]\n",
        1 + rng.below(3),
        rng.below(2),
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
