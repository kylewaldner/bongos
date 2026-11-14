use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bongos_work_model::TodoState;

use crate::checks::{
    CheckOutcome, HoldReason, LiveFacts, LiveSource, MoveHold, StagedTodoMove, StartedInput,
    started_unchanged, strip_move_meta, strip_move_text,
};
use crate::test_trees::{todo, tree};
use crate::{
    Author, ChangeMeta, ChangeSummary, DefinitionTree, TaskStateRecord, TodoStateRecord, summarize,
};

fn record(todos: &[(&str, TodoState)]) -> TaskStateRecord {
    TaskStateRecord {
        todos: todos
            .iter()
            .map(|(id, state)| TodoStateRecord {
                id: Some(id.to_string()),
                state: *state,
                started: None,
            })
            .collect(),
        ..TaskStateRecord::default()
    }
}

/// Task `a` is started: its todo `t1` is done and `t3` skipped; `t2` is open.
fn live() -> LiveFacts {
    LiveFacts::from_states(BTreeMap::from([(
        "a".to_string(),
        record(&[
            ("t1", TodoState::Done),
            ("t2", TodoState::Open),
            ("t3", TodoState::Skipped),
        ]),
    )]))
}

fn todos(ids: &[&str]) -> String {
    ids.iter().map(|id| todo(id, &format!("do {id}"))).collect()
}

fn master() -> DefinitionTree {
    tree(&[
        (
            "tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t1", "t2", "t3"])),
        ),
        ("tasks/b", ""),
    ])
}

fn run(
    new: &DefinitionTree,
    summary: &ChangeSummary,
    live: &LiveFacts,
    change_id: &str,
    meta: &ChangeMeta,
    staged: &[StagedTodoMove],
) -> CheckOutcome {
    started_unchanged(&StartedInput {
        master: &master(),
        new,
        summary,
        live,
        change_id,
        meta,
        staged_moves: staged,
    })
}

fn check(new: &DefinitionTree) -> CheckOutcome {
    let summary = summarize(&master(), new);
    let meta = ChangeMeta::new("change", Author::agent("agent"));
    run(new, &summary, &live(), "c1", &meta, &[])
}

#[test]
fn a_started_task_keeps_its_shape_and_its_todos_with_state() {
    let retagged = tree(&[
        (
            "tasks/a",
            &format!("areas = [\"core\"]\n{}", todos(&["t1", "t2", "t3"])),
        ),
        ("tasks/b", ""),
    ]);
    assert_eq!(
        check(&retagged).message(),
        Some("started task a: its areas may not change")
    );

    let added_and_dropped_open = tree(&[
        (
            "tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t1", "t3", "t4"])),
        ),
        ("tasks/b", "areas = [\"anything\"]\n"),
    ]);
    assert_eq!(check(&added_and_dropped_open), CheckOutcome::Pass);

    let dropped = tree(&[
        (
            "tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t2", "t3"])),
        ),
        ("tasks/b", ""),
    ]);
    assert_eq!(
        check(&dropped).message(),
        Some("started task a: todo do t1 has state and may not be removed")
    );

    let reordered = tree(&[
        (
            "tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t3", "t2", "t1"])),
        ),
        ("tasks/b", ""),
    ]);
    assert_eq!(
        check(&reordered).message(),
        Some("started task a: todos with state may not be reordered")
    );

    let removed = tree(&[("tasks/b", "")]);
    assert_eq!(
        check(&removed).message(),
        Some("started task a was removed")
    );

    let moved = tree(&[
        ("tasks/b", ""),
        (
            "tasks/b/tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t1", "t2", "t3"])),
        ),
    ]);
    assert_eq!(
        check(&moved).message(),
        Some("started task a: its folder may not change")
    );
}

struct ActiveRun;

impl LiveSource for ActiveRun {
    fn name(&self) -> &str {
        "runs"
    }
    fn started(&self, task_id: &str) -> bool {
        task_id == "b"
    }
}

#[test]
fn a_live_source_makes_a_task_started() {
    let mut facts = live();
    facts.register(Arc::new(ActiveRun));
    assert!(facts.started("b"));
    assert_eq!(
        facts.started_set(["a", "b", "top"]),
        BTreeSet::from(["a".into(), "b".into()])
    );
    let new = tree(&[
        (
            "tasks/a",
            &format!("areas = [\"ui\"]\n{}", todos(&["t1", "t2", "t3"])),
        ),
        ("tasks/b", "size = 3\n"),
    ]);
    let meta = ChangeMeta::new("change", Author::agent("agent"));
    let outcome = run(&new, &summarize(&master(), &new), &facts, "c1", &meta, &[]);
    assert_eq!(
        outcome.message(),
        Some("started task b: its size may not change")
    );
}

/// `task: split` of the started leaf `a`: its todos move to the new keeper child.
fn split_tree() -> DefinitionTree {
    tree(&[
        ("tasks/a", "areas = [\"ui\"]\n"),
        ("tasks/a/tasks/keeper", &todos(&["t1", "t2", "t3"])),
        ("tasks/b", ""),
    ])
}

#[test]
fn a_split_of_a_started_leaf_passes_only_with_its_staged_moves() {
    let new = split_tree();
    let summary = ChangeSummary {
        tasks_added: vec!["keeper".to_string()],
        tasks_redefined: vec!["a".to_string()],
        todos_changed: vec!["a".to_string()],
        ..ChangeSummary::default()
    };
    let meta = ChangeMeta::new("split a", Author::agent("agent"));
    let staged: Vec<StagedTodoMove> = ["t1", "t2", "t3"]
        .iter()
        .map(|id| StagedTodoMove {
            from_task: "a".to_string(),
            to_task: "keeper".to_string(),
            todo_id: id.to_string(),
        })
        .collect();
    assert_eq!(
        run(&new, &summary, &live(), "c1", &meta, &staged),
        CheckOutcome::Pass
    );

    let refused = run(&new, &summary, &live(), "c1", &meta, &[]);
    assert_eq!(
        refused.message(),
        Some(
            "started task a: todo do t1 has state and may not be removed\n\
             started task a: todo do t3 has state and may not be removed"
        )
    );

    // A move into a task that is not a new child of `a` is no split.
    let elsewhere: Vec<StagedTodoMove> = staged
        .iter()
        .map(|op| StagedTodoMove {
            to_task: "b".to_string(),
            ..op.clone()
        })
        .collect();
    assert!(matches!(
        run(&new, &summary, &live(), "c1", &meta, &elsewhere),
        CheckOutcome::Fail { .. }
    ));
}

struct MoveClaims;

impl LiveSource for MoveClaims {
    fn name(&self) -> &str {
        "moves"
    }
    fn started(&self, _: &str) -> bool {
        false
    }
    fn held(&self, task_id: &str) -> Option<HoldReason> {
        (task_id == "b").then(|| {
            HoldReason::Moving(MoveHold {
                move_id: "move-7".to_string(),
                label: "moving to Other".to_string(),
                coordinator_changes: BTreeSet::from(["coordinator-change".to_string()]),
            })
        })
    }
}

#[test]
fn only_the_moves_own_change_may_touch_a_held_task() {
    let mut facts = live();
    facts.register(Arc::new(MoveClaims));
    assert!(facts.started("b"));
    let new = tree(&[(
        "tasks/a",
        &format!("areas = [\"ui\"]\n{}", todos(&["t1", "t2", "t3"])),
    )]);
    let summary = summarize(&master(), &new);
    let meta = ChangeMeta::new("move b", Author::workflow("move"));
    assert_eq!(
        run(&new, &summary, &facts, "coordinator-change", &meta, &[]),
        CheckOutcome::Pass
    );

    let plain = run(&new, &summary, &facts, "other", &meta, &[]);
    assert_eq!(
        plain.message(),
        Some("task b is held: moving to Other (move move-7); only the move itself may change it")
    );

    let mut posing = ChangeMeta::new("part of move-7", Author::agent("agent"));
    posing.body = "Removes b.\nBongos-Move: move-7".to_string();
    posing.notes = "bongos-move move-7".to_string();
    let refused = run(&new, &summary, &facts, "agent-change", &posing, &[]);
    assert_eq!(
        refused.message(),
        Some(
            "task b is held: moving to Other (move move-7); naming the move in a change does not \
             make the change part of it"
        )
    );
    let stripped = strip_move_meta(&posing);
    assert_eq!(stripped.body, "Removes b.");
    assert_eq!(stripped.notes, "");
    assert_eq!(stripped.subject, "part of move-7");
    assert_eq!(strip_move_text("a\nsee Bongos-Move: x\nb"), "a\nb");
}
