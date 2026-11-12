use bongos_work_model::store::Task;
use bongos_work_model::{HistoryEntry, LinearCancel, Status, TodoRole};

use crate::api::StateKind;
use crate::status::*;
use crate::test_support::*;

const KINDS: [StateKind; 8] = [
    StateKind::Triage,
    StateKind::Backlog,
    StateKind::Unstarted,
    StateKind::Started,
    StateKind::Completed,
    StateKind::Canceled,
    StateKind::Duplicate,
    StateKind::Other(String::new()),
];

fn linked_leaf() -> Disk {
    Disk::new(vec![begin_plan(synced(plan_leaf("a"), "SEQ-1"))])
}

#[test]
fn started_only_from_backlog_unstarted_and_triage() {
    let disk = linked_leaf();
    let tree = disk.tree();
    let a = id(&tree, "a");
    for kind in KINDS {
        let ticket = state("ticket", kind.clone(), 1.0);
        let decision = started_decision(&tree, &a, &ticket, &team_states());
        match kind {
            StateKind::Triage | StateKind::Backlog | StateKind::Unstarted => {
                let StartedDecision::Write(write) = decision else {
                    panic!("{kind:?}: {decision:?}");
                };
                assert_eq!(write.state_id, "doing");
                assert_eq!(write.issue_id, "uuid-SEQ-1");
                assert_eq!(write.task_id, a);
                assert_eq!(
                    started_write(&tree, &a, &ticket, &team_states()),
                    Some(write)
                );
            }
            other => {
                assert_eq!(
                    decision,
                    StartedDecision::Record(NoWrite::State("ticket".to_string())),
                    "{other:?}"
                );
                assert_eq!(started_write(&tree, &a, &ticket, &team_states()), None);
            }
        }
    }
}

#[test]
fn started_goes_to_the_started_state_with_the_lowest_position() {
    let disk = linked_leaf();
    let tree = disk.tree();
    let a = id(&tree, "a");
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    // `doing` is at 3.0 and `review` at 5.0, listed first.
    let write = started_write(&tree, &a, &backlog, &team_states()).unwrap();
    assert_eq!(write.state_id, "doing");
    let without: Vec<_> = team_states()
        .into_iter()
        .filter(|state| state.kind != StateKind::Started)
        .collect();
    assert_eq!(
        started_decision(&tree, &a, &backlog, &without),
        StartedDecision::Record(NoWrite::NoStartedState)
    );
}

#[test]
fn started_waits_for_the_plan_todo() {
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let mut unstarted = synced(plan_leaf("a"), "SEQ-1");
    // The first todo is started but it is not the plan: the plans todo decides.
    let mut file = file_mut(&mut unstarted).clone();
    file.todos.insert(0, todo_named("implement", None));
    file.todos[0].started = Some(timestamp("2026-11-09T11:00:00Z"));
    *file_mut(&mut unstarted) = file;
    let disk = Disk::new(vec![unstarted]);
    let tree = disk.tree();
    assert_eq!(
        started_decision(&tree, &id(&tree, "a"), &backlog, &team_states()),
        StartedDecision::Nothing
    );

    // With no plans todo, the first todo's start is the plan beginning.
    let mut task = synced(leaf("b"), "SEQ-2");
    assert!(
        !file_mut(&mut task)
            .todos
            .iter()
            .any(|todo| todo.role == Some(TodoRole::Plans))
    );
    let task = begin_plan(task);
    let disk = Disk::new(vec![task]);
    let tree = disk.tree();
    assert!(matches!(
        started_decision(&tree, &id(&tree, "b"), &backlog, &team_states()),
        StartedDecision::Write(_)
    ));
}

fn todo_named(name: &str, role: Option<TodoRole>) -> bongos_work_model::Todo {
    bongos_work_model::Todo {
        id: None,
        name: name.to_string(),
        role,
        workflow: None,
        state: bongos_work_model::TodoState::Open,
        started: None,
        extra: Default::default(),
    }
}

#[test]
fn started_is_decided_once() {
    let mut task = begin_plan(synced(plan_leaf("a"), "SEQ-1"));
    state_mut(&mut task).started_sent = Some(timestamp("2026-11-09T11:30:00Z"));
    let disk = Disk::new(vec![task]);
    let tree = disk.tree();
    assert_eq!(
        started_decision(
            &tree,
            &id(&tree, "a"),
            &state("backlog", StateKind::Backlog, 1.0),
            &team_states()
        ),
        StartedDecision::Nothing
    );
}

#[test]
fn started_never_for_a_stacked_leaf() {
    let disk = Disk::new(vec![
        begin_plan(synced(plan_leaf("a"), "SEQ-1")),
        begin_plan(stacked_on(synced(plan_leaf("b"), "SEQ-2"), "a")),
    ]);
    let tree = disk.tree();
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let b = id(&tree, "b");
    let decision = started_decision(&tree, &b, &backlog, &team_states());
    assert_eq!(decision, StartedDecision::Record(NoWrite::Stacked));
    assert_eq!(
        NoWrite::Stacked.detail(),
        "no Linear status for a stacked leaf"
    );
    assert_eq!(started_write(&tree, &b, &backlog, &team_states()), None);
    // The unstacked one still writes.
    assert!(started_write(&tree, &id(&tree, "a"), &backlog, &team_states()).is_some());

    // Recording it writes started_sent with that history detail and nothing else.
    let change = record_started(&tree, &ctx(), &b, Some(&NoWrite::Stacked)).unwrap();
    let written = change.file().unwrap();
    assert!(
        written
            .linear_state
            .as_ref()
            .unwrap()
            .started_sent
            .is_some()
    );
    let entry = written.history.last().unwrap();
    assert_eq!(
        entry.detail.as_deref(),
        Some("no Linear status for a stacked leaf")
    );
}

#[test]
fn started_ignores_tasks_that_are_not_synced() {
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let disk = Disk::new(vec![
        begin_plan(plan_leaf("plain")),
        begin_plan(hand_linked(plan_leaf("hand"), "SEQ-3")),
        begin_plan(pending(
            plan_leaf("pend"),
            "0196f1c2-0000-4000-8000-000000000000",
        )),
        synced(plan_leaf("idle"), "SEQ-4"),
    ]);
    let tree = disk.tree();
    for slug in ["plain", "hand", "pend", "idle"] {
        assert_eq!(
            started_decision(&tree, &id(&tree, slug), &backlog, &team_states()),
            StartedDecision::Nothing,
            "{slug}"
        );
    }
}

/// A leaf whose started decision was made: `reason` `None` is an actual write, `Some` a recorded
/// decision with that reason.
fn decided(mut task: Task, reason: Option<&str>) -> Task {
    let at = timestamp("2026-11-09T11:30:00Z");
    let file = file_mut(&mut task);
    file.linear_state.as_mut().unwrap().started_sent = Some(at);
    file.history.push(HistoryEntry {
        at,
        by: "kyle".to_string(),
        event: "linear started".to_string(),
        detail: reason.map(str::to_string),
        extra: Default::default(),
    });
    task
}

#[test]
fn a_group_moves_when_a_linked_unstacked_leaf_begins() {
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let quiet = Disk::new(vec![synced(
        group(
            "g",
            vec![
                synced(plan_leaf("x"), "SEQ-11"),
                begin_plan(plan_leaf("unlinked")),
                begin_plan(stacked_on(synced(plan_leaf("y"), "SEQ-12"), "x")),
            ],
        ),
        "SEQ-10",
    )]);
    let tree = quiet.tree();
    let g = id(&tree, "g");
    assert_eq!(
        started_decision(&tree, &g, &backlog, &team_states()),
        StartedDecision::Nothing
    );

    let begun = Disk::new(vec![synced(
        group(
            "g",
            vec![decided(begin_plan(synced(plan_leaf("x"), "SEQ-11")), None)],
        ),
        "SEQ-10",
    )]);
    let tree = begun.tree();
    let write = started_write(&tree, &id(&tree, "g"), &backlog, &team_states()).unwrap();
    assert_eq!(write.issue_id, "uuid-SEQ-10");
    // Only once, and never backwards.
    let started = state("doing", StateKind::Started, 3.0);
    assert_eq!(
        started_decision(&tree, &id(&tree, "g"), &started, &team_states()),
        StartedDecision::Record(NoWrite::State("doing".to_string()))
    );
}

#[test]
fn a_group_moves_only_for_a_leaf_whose_decision_was_a_write() {
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let decision = |leaves: Vec<Task>| {
        let disk = Disk::new(vec![synced(group("g", leaves), "SEQ-10")]);
        let tree = disk.tree();
        started_decision(&tree, &id(&tree, "g"), &backlog, &team_states())
    };
    let begun =
        |slug: &str, number: u32| begin_plan(synced(plan_leaf(slug), &format!("SEQ-{number}")));
    // A begun plan, undecided: the leaf has not been written yet.
    assert_eq!(decision(vec![begun("x", 11)]), StartedDecision::Nothing);
    // Decided with a reason (the ticket was already started, or stacked): nothing was written.
    assert_eq!(
        decision(vec![decided(begun("x", 11), Some("already started"))]),
        StartedDecision::Nothing
    );
    assert_eq!(
        decision(vec![decided(
            begun("x", 11),
            Some(&NoWrite::Stacked.detail())
        )]),
        StartedDecision::Nothing
    );
    // An actual write fires the group.
    assert!(matches!(
        decision(vec![decided(begun("x", 11), None)]),
        StartedDecision::Write(_)
    ));
    // Only the leaves whose nearest linked group is this one count.
    let inner = synced(
        group("inner", vec![decided(begun("deep", 21), None)]),
        "SEQ-20",
    );
    assert_eq!(decision(vec![inner]), StartedDecision::Nothing);
    let plain = group("plain", vec![decided(begun("deep", 21), None)]);
    assert!(matches!(decision(vec![plain]), StartedDecision::Write(_)));
}

#[test]
fn recording_started_writes_one_file_once() {
    let disk = linked_leaf();
    let tree = disk.tree();
    let a = id(&tree, "a");
    let before = disk.snapshot();
    let change = record_started(&tree, &ctx(), &a, None).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    let after = disk.snapshot();
    assert_eq!(
        changed_paths(&before, &after),
        vec![
            task_toml(&tree, "a")
                .strip_prefix(disk.root())
                .unwrap()
                .to_path_buf()
        ]
    );
    let tree = disk.tree();
    assert_eq!(
        started_decision(
            &tree,
            &a,
            &state("backlog", StateKind::Backlog, 1.0),
            &team_states()
        ),
        StartedDecision::Nothing
    );
    assert!(record_started(&tree, &ctx(), &a, None).is_err());
}

fn cancel_tree() -> Disk {
    Disk::new(vec![
        cancelled(synced(plan_leaf("solo"), "SEQ-1")),
        cancelled(stacked_on(synced(plan_leaf("stacked"), "SEQ-2"), "solo")),
        cancelled(synced(
            group(
                "g",
                vec![
                    cancelled(synced(plan_leaf("x"), "SEQ-11")),
                    cancelled(synced(plan_leaf("y"), "SEQ-12")),
                    cancelled(plan_leaf("unlinked")),
                ],
            ),
            "SEQ-10",
        )),
        cancelled(synced(
            group(
                "open",
                vec![
                    cancelled(synced(plan_leaf("c"), "SEQ-21")),
                    synced(plan_leaf("d"), "SEQ-22"),
                ],
            ),
            "SEQ-20",
        )),
    ])
}

#[test]
fn cancel_never_from_completed_or_canceled() {
    let disk = cancel_tree();
    let tree = disk.tree();
    let solo = id(&tree, "solo");
    for kind in KINDS {
        let ticket = state("ticket", kind.clone(), 1.0);
        let write = cancel_write(&tree, &solo, &ticket, &team_states());
        match kind {
            StateKind::Completed | StateKind::Canceled | StateKind::Duplicate => {
                assert_eq!(write, None, "{kind:?}")
            }
            _ => {
                // The target is the canceled state with the lowest position, never a duplicate.
                assert_eq!(write.unwrap().state_id, "canceled", "{kind:?}");
            }
        }
    }
    // No canceled state in the team: nothing to write.
    let only_duplicate: Vec<_> = team_states()
        .into_iter()
        .filter(|state| state.kind != StateKind::Canceled)
        .collect();
    assert_eq!(
        cancel_write(
            &tree,
            &solo,
            &state("t", StateKind::Backlog, 1.0),
            &only_duplicate
        ),
        None
    );
}

#[test]
fn cancel_needs_a_cancelled_unstacked_linked_task() {
    let backlog = state("backlog", StateKind::Backlog, 1.0);
    let disk = Disk::new(vec![
        synced(plan_leaf("open"), "SEQ-1"),
        cancelled(plan_leaf("plain")),
        cancelled(stacked_on(synced(plan_leaf("stacked"), "SEQ-2"), "open")),
    ]);
    let tree = disk.tree();
    for slug in ["open", "plain", "stacked"] {
        assert_eq!(
            cancel_write(&tree, &id(&tree, slug), &backlog, &team_states()),
            None,
            "{slug}"
        );
    }
}

#[test]
fn cancel_question_for_a_leaf_and_a_stacked_leaf() {
    let disk = cancel_tree();
    let tree = disk.tree();
    match cancel_question(&tree, &id(&tree, "solo")).unwrap() {
        CancelQuestion::Leaf(target) => {
            assert_eq!(target.identifier, "SEQ-1");
            assert_eq!(target.issue_id, "uuid-SEQ-1");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        cancel_question(&tree, &id(&tree, "stacked")),
        Err(NoQuestion::Stacked("SEQ-2".to_string()))
    );
    assert_eq!(
        cancel_question(&tree, &id(&tree, "unlinked")),
        Err(NoQuestion::NotLinked)
    );
}

#[test]
fn cancel_group_never_with_an_open_leaf() {
    let disk = cancel_tree();
    let tree = disk.tree();
    let open = id(&tree, "open");
    assert_eq!(
        may_cancel_group(&tree, &open),
        Err(NoQuestion::OpenLeaf("d".to_string()))
    );
    assert_eq!(
        cancel_question(&tree, &open),
        Err(NoQuestion::OpenLeaf("d".to_string()))
    );
    assert_eq!(may_cancel_group(&tree, &id(&tree, "g")), Ok(()));
    // Not even the write: the group's own rule refuses while a leaf is open, cancelled or not.
    let ticket = state("backlog", StateKind::Backlog, 1.0);
    assert_eq!(cancel_write(&tree, &open, &ticket, &team_states()), None);
    assert!(cancel_write(&tree, &id(&tree, "g"), &ticket, &team_states()).is_some());
}

#[test]
fn a_leaf_reopened_while_the_question_waits_stops_the_group_write() {
    let tree_with = |leaf_cancelled: bool| {
        let x = synced(plan_leaf("x"), "SEQ-11");
        let x = if leaf_cancelled { cancelled(x) } else { x };
        Disk::new(vec![cancelled(synced(group("g", vec![x]), "SEQ-10"))])
    };
    let asked = tree_with(true);
    let tree = asked.tree();
    let question = cancel_question(&tree, &id(&tree, "g")).unwrap();
    // Kyle reopens the leaf before he answers.
    let reopened = tree_with(false);
    let tree = reopened.tree();
    let ticket = |_: &str| Some(state("backlog", StateKind::Backlog, 1.0));
    let team = |_: &str| team_states();
    for answer in [
        CancelAnswer::All,
        CancelAnswer::LeavesOnly,
        CancelAnswer::Leave,
    ] {
        let plan = cancel_plan(&tree, &question, answer, &ticket, &team);
        assert!(
            plan.writes.is_empty() && plan.cancelled.is_empty(),
            "{answer:?}"
        );
    }
}

#[test]
fn cancel_group_question_lists_the_cancelled_linked_leaves() {
    let disk = cancel_tree();
    let tree = disk.tree();
    let CancelQuestion::Group { group, leaves } = cancel_question(&tree, &id(&tree, "g")).unwrap()
    else {
        panic!("a group question");
    };
    assert_eq!(group.identifier, "SEQ-10");
    let names: Vec<_> = leaves.iter().map(|leaf| leaf.identifier.as_str()).collect();
    assert_eq!(names, vec!["SEQ-11", "SEQ-12"]);
}

#[test]
fn cancel_answers_become_writes_and_records() {
    let disk = cancel_tree();
    let tree = disk.tree();
    let ticket = |_: &str| Some(state("backlog", StateKind::Backlog, 1.0));
    let team = |_: &str| team_states();

    let leaf = cancel_question(&tree, &id(&tree, "solo")).unwrap();
    let cancel = cancel_plan(&tree, &leaf, CancelAnswer::Cancel, &ticket, &team);
    assert_eq!(cancel.writes.len(), 1);
    assert_eq!(cancel.writes[0].state_id, "canceled");
    assert_eq!(cancel.cancelled, vec![id(&tree, "solo")]);
    let leave = cancel_plan(&tree, &leaf, CancelAnswer::Leave, &ticket, &team);
    assert!(leave.writes.is_empty() && leave.cancelled.is_empty());
    assert_eq!(leave.declined, vec![id(&tree, "solo")]);

    let group = cancel_question(&tree, &id(&tree, "g")).unwrap();
    let (x, y, g) = (id(&tree, "x"), id(&tree, "y"), id(&tree, "g"));
    let leaves_only = cancel_plan(&tree, &group, CancelAnswer::LeavesOnly, &ticket, &team);
    assert_eq!(leaves_only.cancelled, vec![x.clone(), y.clone()]);
    assert_eq!(leaves_only.declined, vec![g.clone()]);
    let all = cancel_plan(&tree, &group, CancelAnswer::All, &ticket, &team);
    assert_eq!(all.cancelled, vec![x.clone(), y.clone(), g.clone()]);
    assert_eq!(all.writes.len(), 3);
    let none = cancel_plan(&tree, &group, CancelAnswer::Leave, &ticket, &team);
    assert!(none.writes.is_empty());
    assert_eq!(none.declined, vec![x, y, g]);

    // A ticket already completed is not written and not recorded as cancelled.
    let completed = |_: &str| Some(state("done", StateKind::Completed, 6.0));
    let skipped = cancel_plan(&tree, &leaf, CancelAnswer::Cancel, &completed, &team);
    assert!(skipped.writes.is_empty() && skipped.cancelled.is_empty());
}

#[test]
fn the_answer_is_recorded_so_it_is_asked_once() {
    let disk = cancel_tree();
    let tree = disk.tree();
    let solo = id(&tree, "solo");
    let before = disk.snapshot();
    let change = record_cancel(&tree, &ctx(), &solo, LinearCancel::Declined).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    assert_eq!(changed_paths(&before, &disk.snapshot()).len(), 1);
    let tree = disk.tree();
    assert_eq!(
        file(&tree, "solo").linear_state.as_ref().unwrap().cancel,
        Some(LinearCancel::Declined)
    );
    assert_eq!(cancel_question(&tree, &solo), Err(NoQuestion::Answered));

    // Reopening clears it and writes nothing to Linear: the change is one task file.
    let cleared = clear_cancel(&tree, &ctx(), &solo).unwrap();
    assert_eq!(cleared.writes.len(), 1);
    assert_eq!(
        cleared
            .file()
            .unwrap()
            .linear_state
            .as_ref()
            .unwrap()
            .cancel,
        None
    );
    assert!(clear_cancel(&tree, &ctx(), &id(&tree, "unlinked")).is_none());
}

#[test]
fn status_never_writes_a_task_that_is_not_synced() {
    let disk = Disk::new(vec![
        plan_leaf("plain"),
        hand_linked(plan_leaf("hand"), "SEQ-3"),
    ]);
    let tree = disk.tree();
    for slug in ["plain", "hand"] {
        let task = id(&tree, slug);
        assert!(record_started(&tree, &ctx(), &task, None).is_err());
        assert!(record_cancel(&tree, &ctx(), &task, LinearCancel::Cancelled).is_err());
        assert!(clear_cancel(&tree, &ctx(), &task).is_none());
    }
    assert_eq!(file(&tree, "plain").status, Status::Open);
}
