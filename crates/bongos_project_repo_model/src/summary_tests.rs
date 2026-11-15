use crate::summary::{ChangeSummary, EdgeChange, EdgeKind, FileChange, summarize};
use crate::test_trees::{after, todo, tree};

#[test]
fn a_summary_names_added_removed_and_redefined_tasks_and_edges() {
    let master = tree(&[
        ("tasks/build", ""),
        ("tasks/deploy", &after(&["build"])),
        ("tasks/old", ""),
        ("tasks/tagged", "areas = [\"ui\"]\n"),
        ("tasks/todo", &todo("t1", "one")),
    ]);
    let new = tree(&[
        ("tasks/build", ""),
        ("tasks/deploy", ""),
        ("tasks/tests", &after(&["build", "deploy"])),
        ("tasks/tagged", "areas = [\"core\"]\n"),
        (
            "tasks/todo",
            &format!("{}{}", todo("t1", "one"), todo("t2", "two")),
        ),
        ("tasks/build/tasks/old", ""),
    ]);
    let summary = summarize(&master, &new);
    assert_eq!(summary.tasks_added, ["tests"]);
    assert!(summary.tasks_removed.is_empty());
    assert_eq!(summary.tasks_redefined, ["deploy", "old", "tagged", "todo"]);
    assert_eq!(summary.retagged, ["tagged"]);
    assert_eq!(summary.reparented, ["old"]);
    assert_eq!(summary.todos_changed, ["todo"]);
    let edge = |task: &str, target: &str| EdgeChange {
        task: task.to_string(),
        kind: EdgeKind::After,
        target: target.to_string(),
    };
    assert_eq!(
        summary.edges_added,
        [edge("tests", "build"), edge("tests", "deploy")]
    );
    assert_eq!(summary.edges_removed, [edge("deploy", "build")]);
    assert!(
        summary
            .files
            .contains(&("tasks/tests/task.toml".to_string(), FileChange::Added))
    );
    assert!(
        summary
            .files
            .contains(&("tasks/old/task.toml".to_string(), FileChange::Removed))
    );
    assert_eq!(
        summary.row_text(),
        "adds 1 task, 2 edges; removes 1 edge; retags 1; moves 1; changes todos of 1"
    );
    assert_eq!(
        summary.generated_subject(),
        "Add 1 task, 2 edges; remove 1 edge; retag 1; move 1; change todos of 1"
    );
}

#[test]
fn the_row_text_counts_like_the_landing_view() {
    let summary = ChangeSummary {
        tasks_added: (1..=5).map(|n| n.to_string()).collect(),
        edges_added: vec![
            EdgeChange {
                task: "1".into(),
                kind: EdgeKind::After,
                target: "a".into(),
            },
            EdgeChange {
                task: "2".into(),
                kind: EdgeKind::StackedOn,
                target: "b".into(),
            },
        ],
        retagged: vec!["9".into()],
        tasks_redefined: vec!["9".into()],
        ..ChangeSummary::default()
    };
    assert_eq!(summary.row_text(), "adds 5 tasks, 2 edges; retags 1");
    assert_eq!(ChangeSummary::default().row_text(), "no changes");
    assert_eq!(
        ChangeSummary::default().generated_subject(),
        "Change definitions"
    );
}

#[test]
fn files_name_the_workflows_they_touch() {
    let summary = ChangeSummary::default().with_files(vec![
        (
            "workflows/review/workflow.toml".into(),
            FileChange::Modified,
        ),
        ("workflows/review/prompt.md".into(), FileChange::Added),
        ("documents/notes.md".into(), FileChange::Added),
    ]);
    assert_eq!(summary.workflows_touched, ["review"]);
    assert_eq!(summary.row_text(), "touches 1 workflow");
    let documents =
        ChangeSummary::default().with_files(vec![("documents/notes.md".into(), FileChange::Added)]);
    assert_eq!(documents.row_text(), "changes 1 file");
}
