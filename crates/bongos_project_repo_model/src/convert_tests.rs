use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bongos_work_model::fixtures::{activity, after, at, file_mut, leaf, project, todo};
use bongos_work_model::store::{FileError, Project, Task};
use bongos_work_model::{Status, TodoState, UnblockPoint};

use crate::convert::*;

fn ids() -> impl FnMut() -> String {
    let mut next = 0;
    move || {
        next += 1;
        format!("new-{next}")
    }
}

fn with_files(mut task: Task, files: &[&str]) -> Task {
    for file in files {
        task.files
            .insert(PathBuf::from(file), SystemTime::UNIX_EPOCH);
    }
    task
}

/// The project `invoicing`: top group with `backfill` (two todos, one done, `after` report) and
/// `report`, with files of every kind.
fn fixture() -> Project {
    let mut backfill = leaf("backfill", Status::Cancelled);
    file_mut(&mut backfill).todos = vec![todo("implement", None), todo("review", None)];
    file_mut(&mut backfill).todos[0].state = TodoState::Done;
    let backfill = with_files(
        after(backfill, "report", UnblockPoint::Merged),
        &[
            "task.toml",
            "prompts.txt",
            "artifacts.toml",
            "chats/c1.toml",
            "runs/r1/log.txt",
            "proposals/1.proposal.txt",
            "history.old.toml",
            "notes/design.md",
        ],
    );
    let report = with_files(leaf("report", Status::Open), &["task.toml"]);
    let mut project = project("invoicing", vec![backfill, report]);
    project.task = with_files(
        project.task,
        &[
            "task.toml",
            "project.toml",
            "plan.md",
            "workflows/ship/workflow.toml",
            "workflows/ship/prompt.md",
        ],
    );
    project
}

#[test]
fn a_project_plans_every_task_with_its_definition_state_and_files() {
    let old = fixture();
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers, Vec::<String>::new());
    assert_eq!(plan.slug, "invoicing");
    assert_eq!(plan.project_id(), "new-1");
    let folders: Vec<&str> = plan.tasks.iter().map(|task| task.folder.as_str()).collect();
    assert_eq!(folders, ["", "tasks/backfill", "tasks/report"]);

    let backfill = &plan.tasks[1];
    assert_eq!(backfill.state.status, Status::Cancelled);
    assert_eq!(backfill.definition.after[0].task, "report");
    // Todos without ids get new ones, in the definition and the state alike.
    let todo_ids: Vec<_> = backfill
        .definition
        .todos
        .iter()
        .map(|todo| todo.id.clone().unwrap())
        .collect();
    assert_eq!(todo_ids, ["new-2", "new-3"]);
    assert_eq!(backfill.state.todos[0].id.as_deref(), Some("new-2"));
    assert_eq!(backfill.state.todos[0].state, TodoState::Done);
    assert!(backfill.fold_old_history);
    assert_eq!(backfill.consumed, ["history.old.toml", "task.toml"]);
    let targets: Vec<&Target> = backfill.copies.iter().map(|copy| &copy.to).collect();
    assert!(targets.contains(&&Target::Files("chats/c1.toml".to_string())));
    assert!(targets.contains(&&Target::Files("runs/r1/log.txt".to_string())));
    assert!(targets.contains(&&Target::Files("notes/design.md".to_string())));

    let top = &plan.tasks[0];
    assert_eq!(top.consumed, ["project.toml", "task.toml"]);
    let targets: Vec<&Target> = top.copies.iter().map(|copy| &copy.to).collect();
    assert!(targets.contains(&&Target::Definitions("plan.md".to_string())));
    assert!(targets.contains(&&Target::Definitions(
        "workflows/ship/workflow.toml".to_string()
    )));

    let summary = plan.summary();
    assert_eq!(
        (
            summary.tasks,
            summary.todos,
            summary.workflows,
            summary.chats
        ),
        (3, 2, 1, 1)
    );
    assert!(
        summary
            .creates
            .iter()
            .any(|line| line.contains(CONVERSION_COMMIT_MESSAGE))
    );
}

#[test]
fn hostile_names_are_rejected_and_never_planned() {
    let mut old = fixture();
    old.task.children[1] = with_files(
        old.task.children[1].clone(),
        &[
            "../escape.txt",
            "/etc/passwd",
            ".git/config",
            "files/.GIT/HEAD",
            "line\nbreak.txt",
            "./dot.txt",
        ],
    );
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers, Vec::<String>::new());
    let rejected: Vec<String> = plan
        .rejected
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(rejected.len(), 6, "{rejected:?}");
    for task in &plan.tasks {
        for copy in &task.copies {
            let (Target::Files(path) | Target::Definitions(path)) = &copy.to;
            assert!(relative_path(Path::new(path)).is_ok(), "{path}");
            assert!(!path.to_ascii_lowercase().contains(".git"), "{path}");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_name_that_is_not_utf8_is_rejected() {
    use std::os::unix::ffi::OsStrExt as _;
    let name = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
    assert_eq!(
        relative_path(Path::new(name)),
        Err("the name is not UTF-8".to_string())
    );
}

#[test]
fn hostile_ids_and_slugs_block_the_conversion() {
    let mut old = fixture();
    file_mut(&mut old.task.children[0]).id = "../../etc".to_string();
    old.task.children[1].slug = "a\nb".to_string();
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers.len(), 2, "{:?}", plan.blockers);
    assert!(plan.is_blocked());
}

#[test]
fn colliding_ids_block_and_repeated_todo_ids_are_renewed() {
    let mut old = fixture();
    let id = old.task.children[0].id().unwrap().to_string();
    file_mut(&mut old.task.children[1]).id = id;
    file_mut(&mut old.task.children[1]).todos = vec![todo("a", None), todo("b", None)];
    file_mut(&mut old.task.children[1]).todos[0].id = Some("same".to_string());
    file_mut(&mut old.task.children[1]).todos[1].id = Some("same".to_string());
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers.len(), 1, "{:?}", plan.blockers);
    assert!(plan.blockers[0].contains("duplicate id"));
    let report = plan
        .tasks
        .iter()
        .find(|task| task.folder == "tasks/report")
        .unwrap();
    let todo_ids: Vec<_> = report
        .definition
        .todos
        .iter()
        .map(|todo| todo.id.clone().unwrap())
        .collect();
    assert_eq!(todo_ids[0], "same");
    assert_ne!(todo_ids[1], "same");
}

#[test]
fn a_task_with_an_error_badge_or_no_parse_blocks_the_conversion() {
    let mut old = fixture();
    old.task.children[0].errors.push(FileError {
        path: PathBuf::from("/w/projects/invoicing/tasks/backfill/task.toml"),
        line: Some(3),
        reason: "expected a value".to_string(),
    });
    old.task.children[1].file = None;
    old.file = None;
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers.len(), 3, "{:?}", plan.blockers);
}

#[test]
fn an_empty_project_plans_only_its_top_group() {
    let old = project("empty", Vec::new());
    let plan = plan(&old, &mut ids());
    assert_eq!(plan.blockers, Vec::<String>::new());
    assert_eq!(plan.tasks.len(), 1);
    let summary = plan.summary();
    assert_eq!((summary.tasks, summary.files), (1, 0));
    assert!(
        summary
            .creates
            .iter()
            .any(|line| line.contains("an empty plan.md"))
    );
}

#[test]
fn a_project_keeps_its_id() {
    let mut old = fixture();
    old.file.as_mut().unwrap().id = Some("kept".to_string());
    assert_eq!(plan(&old, &mut ids()).project_id(), "kept");
}

#[test]
fn oneoffs_become_activity_groups_of_one_project() {
    let oneoff = with_files(
        leaf("2026-01-02-fix", Status::Done),
        &["task.toml", "chats/c.toml"],
    );
    let writing = activity("writing", vec![oneoff]);
    let reading = activity("reading", vec![leaf("2026-01-03-book", Status::Open)]);
    let now = at("2026-01-05T10:00:00Z");
    let plan = plan_oneoffs(
        Path::new("/w/oneoffs"),
        &[writing, reading],
        DEFAULT_ONEOFFS_TITLE,
        now,
        &mut ids(),
    );
    assert_eq!(plan.blockers, Vec::<String>::new());
    assert_eq!(plan.slug, "one-offs");
    assert_eq!(plan.project.title, DEFAULT_ONEOFFS_TITLE);
    let folders: Vec<(&str, bool)> = plan
        .tasks
        .iter()
        .map(|task| (task.folder.as_str(), task.synthetic))
        .collect();
    assert_eq!(
        folders,
        [
            ("", true),
            ("tasks/writing", true),
            ("tasks/writing/tasks/2026-01-02-fix", false),
            ("tasks/reading", true),
            ("tasks/reading/tasks/2026-01-03-book", false),
        ]
    );
    assert_eq!(
        plan.tasks[1].definition.activity.as_deref(),
        Some("writing")
    );
    assert_eq!(plan.tasks[1].definition.created, now);
    assert_eq!(plan.tasks[2].state.status, Status::Done);
    assert_eq!(plan.summary().chats, 1);
}

#[test]
fn a_summary_survives_serde() {
    let summary = plan(&fixture(), &mut ids()).summary();
    let text = toml::to_string(&summary).unwrap();
    let back: ConversionSummary = toml::from_str(&text).unwrap();
    assert_eq!(back, summary);
}

#[test]
fn names_are_checked() {
    for bad in ["", ".", "..", "a/b", "a\\b", "a\nb", "a\0b", ".git", ".Git"] {
        assert!(name_problem(bad).is_some(), "{bad:?}");
    }
    assert!(name_problem(&"x".repeat(256)).is_some());
    for good in ["task", "2026-01-01-x", ".hidden", "naïve name"] {
        assert_eq!(name_problem(good), None, "{good:?}");
    }
}
