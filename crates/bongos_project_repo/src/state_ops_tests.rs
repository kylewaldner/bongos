use std::sync::Arc;

use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::{HistoryEntry, ProposalRecord, ProposalState, TodoState};
use tempfile::TempDir;

use crate::StateWriter;
use crate::test_support::{location, task_with_todos, writer};
use crate::{StateApplyHooks, StateOp};

fn store(root: &TempDir) -> Arc<StateWriter> {
    writer(&location(root), &StateApplyHooks::new())
}

fn apply(writer: &StateWriter, ops: Vec<StateOp>) -> Vec<String> {
    writer.transact(|tx| tx.apply_ops(&ops)).unwrap()
}

fn task(writer: &StateWriter, id: &str) -> Option<TaskStateRecord> {
    writer.read(|read| read.task(id)).unwrap()
}

fn entry(event: &str) -> HistoryEntry {
    HistoryEntry {
        at: "2026-11-12T10:00:00Z".parse().unwrap(),
        by: "agent".to_string(),
        event: event.to_string(),
        detail: Some("why".to_string()),
        extra: Default::default(),
    }
}

fn proposal(number: u32, state: ProposalState) -> ProposalRecord {
    ProposalRecord {
        number,
        state,
        at: "2026-11-12T10:00:00Z".parse().unwrap(),
        by: "agent".to_string(),
        by_kind: None,
        hash: "ab".repeat(32),
        extra: Default::default(),
    }
}

fn set_branch(task: &str, expected: Option<&str>, value: &str) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: "branch".to_string(),
        expected: expected.map(|expected| toml::Value::String(expected.to_string())),
        value: Some(toml::Value::String(value.to_string())),
    }
}

fn every_op() -> Vec<StateOp> {
    vec![
        StateOp::MoveTodo {
            from_task: "parent".into(),
            to_task: "child".into(),
            todo_id: "t2".into(),
        },
        StateOp::MoveWork {
            from_task: "parent".into(),
            to_task: "child".into(),
        },
        set_branch("a", Some("work/a"), "work/b"),
        StateOp::Set {
            task: "a".into(),
            field: "closed".into(),
            expected: None,
            value: Some(toml::Value::Datetime(
                "2026-11-12T10:00:00Z".parse().unwrap(),
            )),
        },
        StateOp::AppendHistory {
            task: "a".into(),
            entry: entry("split"),
        },
        StateOp::Delete { task: "a".into() },
        StateOp::RecordProposal {
            root: "root".into(),
            record: proposal(2, ProposalState::Applied),
        },
    ]
}

#[test]
fn every_state_op_round_trips_through_json() {
    for op in every_op() {
        let json = serde_json::to_string(&op).unwrap();
        let back: StateOp = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op, "{json}");
    }
}

#[test]
fn staged_ops_round_trip_through_the_store() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| tx.stage("change-1", &every_op()))
        .unwrap();
    assert_eq!(
        writer.read(|read| read.staged("change-1")).unwrap(),
        Some(every_op())
    );
    writer.transact(|tx| tx.drop_staged("change-1")).unwrap();
    assert_eq!(writer.read(|read| read.staged("change-1")).unwrap(), None);
}

#[test]
fn move_todo_moves_the_todo_as_it_is_now() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| {
            tx.set_task(
                "parent",
                task_with_todos(&[("t1", TodoState::Open), ("t2", TodoState::Open)]),
            )?;
            // The todo was skipped while the split waited.
            tx.set_todo(
                "parent",
                crate::test_support::todo("t2", TodoState::Skipped),
            )
        })
        .unwrap();

    let notes = apply(
        &writer,
        vec![StateOp::MoveTodo {
            from_task: "parent".into(),
            to_task: "child".into(),
            todo_id: "t2".into(),
        }],
    );

    assert!(notes.is_empty(), "{notes:?}");
    let parent = task(&writer, "parent").unwrap();
    assert_eq!(parent.todos.len(), 1);
    assert_eq!(parent.todos[0].id.as_deref(), Some("t1"));
    let child = task(&writer, "child").unwrap();
    assert_eq!(child.todos.len(), 1);
    assert_eq!(child.todos[0].id.as_deref(), Some("t2"));
    assert_eq!(child.todos[0].state, TodoState::Skipped);
}

#[test]
fn move_todo_of_a_todo_that_left_notes_it() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| tx.set_task("parent", task_with_todos(&[("t1", TodoState::Open)])))
        .unwrap();
    let notes = apply(
        &writer,
        vec![StateOp::MoveTodo {
            from_task: "parent".into(),
            to_task: "child".into(),
            todo_id: "gone".into(),
        }],
    );
    assert_eq!(notes, ["todo gone is no longer in parent"]);
    assert_eq!(task(&writer, "child"), None);
}

#[test]
fn move_work_moves_the_work_as_it_is_now() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| {
            tx.set_task(
                "parent",
                TaskStateRecord {
                    branch: Some("work/old".into()),
                    worktree: Some("/w/old".into()),
                    ..TaskStateRecord::default()
                },
            )
        })
        .unwrap();
    // The branch changed while the change waited.
    apply(
        &writer,
        vec![set_branch("parent", Some("work/old"), "work/new")],
    );

    apply(
        &writer,
        vec![StateOp::MoveWork {
            from_task: "parent".into(),
            to_task: "child".into(),
        }],
    );

    let parent = task(&writer, "parent").unwrap();
    assert_eq!((parent.branch, parent.worktree), (None, None));
    let child = task(&writer, "child").unwrap();
    assert_eq!(child.branch.as_deref(), Some("work/new"));
    assert_eq!(child.worktree.as_deref(), Some("/w/old"));
}

#[test]
fn set_applies_only_when_the_value_is_still_expected() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| {
            tx.set_task(
                "a",
                TaskStateRecord {
                    branch: Some("work/newer".into()),
                    ..TaskStateRecord::default()
                },
            )
        })
        .unwrap();

    let notes = apply(
        &writer,
        vec![set_branch("a", Some("work/old"), "work/staged")],
    );
    assert_eq!(notes, ["kept newer branch of a"]);
    assert_eq!(
        task(&writer, "a").unwrap().branch.as_deref(),
        Some("work/newer")
    );

    let notes = apply(
        &writer,
        vec![set_branch("a", Some("work/newer"), "work/staged")],
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        task(&writer, "a").unwrap().branch.as_deref(),
        Some("work/staged")
    );

    // An absent field is expected as `None`, and `None` removes it.
    let notes = apply(
        &writer,
        vec![StateOp::Set {
            task: "a".into(),
            field: "branch".into(),
            expected: None,
            value: None,
        }],
    );
    assert_eq!(notes, ["kept newer branch of a"]);
    let notes = apply(
        &writer,
        vec![StateOp::Set {
            task: "a".into(),
            field: "branch".into(),
            expected: Some(toml::Value::String("work/staged".into())),
            value: None,
        }],
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(task(&writer, "a").unwrap().branch, None);
}

#[test]
fn append_history_appends_after_newer_entries() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| tx.append_history("a", entry("claimed meanwhile")))
        .unwrap();
    apply(
        &writer,
        vec![StateOp::AppendHistory {
            task: "a".into(),
            entry: entry("split"),
        }],
    );
    let events: Vec<String> = task(&writer, "a")
        .unwrap()
        .history
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    assert_eq!(events, ["claimed meanwhile", "split"]);
}

#[test]
fn delete_removes_the_rows_as_they_are_now() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| {
            tx.set_task("a", task_with_todos(&[("t1", TodoState::Done)]))?;
            tx.append_history("a", entry("written meanwhile"))
        })
        .unwrap();
    apply(&writer, vec![StateOp::Delete { task: "a".into() }]);
    assert_eq!(task(&writer, "a"), None);
}

#[test]
fn record_proposal_keeps_a_newer_record_of_the_same_number() {
    let root = TempDir::new().unwrap();
    let writer = store(&root);
    writer
        .transact(|tx| {
            tx.set_task(
                "root",
                TaskStateRecord {
                    proposals: vec![proposal(1, ProposalState::Rejected)],
                    ..TaskStateRecord::default()
                },
            )
        })
        .unwrap();

    let notes = apply(
        &writer,
        vec![
            StateOp::RecordProposal {
                root: "root".into(),
                record: proposal(1, ProposalState::Applied),
            },
            StateOp::RecordProposal {
                root: "root".into(),
                record: proposal(2, ProposalState::Applied),
            },
        ],
    );

    assert_eq!(notes, ["kept newer proposal 1 of root"]);
    let proposals = task(&writer, "root").unwrap().proposals;
    assert_eq!(
        proposals,
        vec![
            proposal(1, ProposalState::Rejected),
            proposal(2, ProposalState::Applied)
        ]
    );
}
