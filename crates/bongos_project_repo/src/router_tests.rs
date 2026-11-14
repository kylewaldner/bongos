use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_project_repo_model::{Author, TaskDefinition, join};
use bongos_work_model::commands::{self, Change, Context, NewTask, Outcome};
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, ResultKind, Status, TaskResult, TodoState};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::router::{DefinitionOp, FileOp, Routed, apply_state_only, owns, project_into, route};
use crate::test_support::{fixture_project, refs};
use crate::{LoadedProject, ProjectRepo, StateOp, StateWriter};

fn ctx() -> Context {
    Context::new("tester", "2026-11-13T10:00:00Z".parse().unwrap())
}

fn loaded(root: &TempDir) -> (ProjectRepo, Arc<LoadedProject>) {
    let repo = fixture_project(root);
    let loaded = LoadedProject::new(repo.clone());
    block_on(loaded.load()).unwrap();
    (repo, loaded)
}

fn writer(repo: &ProjectRepo) -> &Arc<StateWriter> {
    repo.state().unwrap().writer().unwrap()
}

/// Everything git knows that a commit or a branch would change, and the checkout's status.
fn git_snapshot(repo: &ProjectRepo) -> (String, String) {
    let status = block_on(
        repo.git()
            .run(&repo.location().repository, &["status", "--porcelain"]),
    )
    .unwrap();
    (refs(repo), status)
}

/// Runs `command` on the loaded tree, routes its change, and applies it as a state-only change.
fn run_state_only(
    repo: &ProjectRepo,
    loaded: &LoadedProject,
    command: impl Fn(&WorkTree, &Context) -> Outcome,
) -> WorkTree {
    let tree = loaded.work_tree().unwrap();
    let change = command(&tree, &ctx()).unwrap();
    let Routed::StateOnly { state, files } = route(&tree, &change).unwrap() else {
        panic!("expected a state-only change for {}", change.entry.event);
    };
    apply_state_only(
        writer(repo),
        &repo.location().files,
        &state,
        &files,
        &Author::unattributed("tester"),
    )
    .unwrap();
    loaded.work_tree().unwrap()
}

fn status(tree: &WorkTree, id: &str) -> Status {
    tree.task_by_id(id).unwrap().file.as_ref().unwrap().status
}

fn todo_states(tree: &WorkTree, id: &str) -> Vec<TodoState> {
    tree.task_by_id(id)
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .todos
        .iter()
        .map(|todo| todo.state)
        .collect()
}

fn link_result() -> TaskResult {
    TaskResult {
        kind: ResultKind::Link,
        url: Some("https://example.com/report".to_string()),
        state: None,
        urls: Vec::new(),
        extra: Default::default(),
    }
}

#[test]
fn state_only_commands_write_one_transaction_and_no_commit_or_branch() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    let git_before = git_snapshot(&repo);

    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::advance(tree, ctx, "l1", false)
    });
    assert_eq!(
        todo_states(&tree, "l1"),
        [TodoState::Done, TodoState::Open, TodoState::Open]
    );

    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::todo_start(tree, ctx, "l1", None)
    });
    let started = tree.task_by_id("l1").unwrap().file.as_ref().unwrap().todos[1].started;
    assert!(started.is_some());

    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::todo_skip(tree, ctx, "l1", None)
    });
    assert_eq!(
        todo_states(&tree, "l1"),
        [TodoState::Done, TodoState::Skipped, TodoState::Open]
    );

    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::todo_reopen(tree, ctx, "l1", 1)
    });
    assert_eq!(todo_states(&tree, "l1")[1], TodoState::Open);

    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::set_result(tree, ctx, "l1", link_result())
    });
    assert_eq!(
        tree.task_by_id("l1").unwrap().file.as_ref().unwrap().result,
        Some(link_result())
    );

    let tree = run_state_only(&repo, &loaded, |tree, ctx| commands::close(tree, ctx, "l1"));
    assert_eq!(status(&tree, "l1"), Status::Done);
    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::reopen(tree, ctx, "l1")
    });
    assert_eq!(status(&tree, "l1"), Status::Open);
    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::drop_task(tree, ctx, "l2", false)
    });
    assert_eq!(status(&tree, "l2"), Status::Cancelled);

    // Every command left its history entry in the state store, by its author.
    let history = &tree
        .task_by_id("l1")
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .history;
    let events: Vec<&str> = history.iter().map(|entry| entry.event.as_str()).collect();
    assert_eq!(
        events,
        [
            "todo done",
            "todo started",
            "todo skipped",
            "todo reopened",
            "result set",
            "closed",
            "reopened"
        ]
    );
    assert!(history.iter().all(|entry| entry.by == "tester"));

    // No commit, no branch, no change in the checkout, and no working copy.
    assert_eq!(git_snapshot(&repo), git_before);
    assert!(!repo.location().working_copies.exists());
}

#[test]
fn renaming_a_goal_is_a_definition_change_that_keeps_the_state() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::advance(tree, ctx, "l1", false)
    });
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::set_result(tree, ctx, "l1", link_result())
    });
    let tree = loaded.work_tree().unwrap();
    let state_before = writer(&repo).read(|read| read.task("l1")).unwrap();

    let edit = commands::TaskEdit {
        goal: Some("Backfill usage events".to_string()),
        kind: None,
        repository: None,
    };
    let change = commands::edit(&tree, &ctx(), "l1", edit).unwrap();
    let Routed::Definition {
        definition, state, ..
    } = route(&tree, &change).unwrap()
    else {
        panic!("a goal is a definition");
    };
    assert_eq!(definition.len(), 1);
    let DefinitionOp::Write { path, bytes } = &definition[0] else {
        panic!("expected a write");
    };
    assert_eq!(path.as_str(), "tasks/backfill/task.toml");
    // Only the history entry is state; it waits for the landing.
    assert!(
        state
            .iter()
            .all(|op| matches!(op, StateOp::AppendHistory { .. }))
    );
    // Routing writes nothing.
    assert_eq!(
        writer(&repo).read(|read| read.task("l1")).unwrap(),
        state_before
    );

    // Joined with the state as it is, the new definition keeps status, todos and result.
    let new_definition: TaskDefinition =
        toml::from_str(std::str::from_utf8(bytes).unwrap()).unwrap();
    let joined = join(new_definition, state_before.unwrap());
    assert_eq!(joined.goal, "Backfill usage events");
    assert_eq!(joined.todos[0].state, TodoState::Done);
    assert_eq!(joined.result, Some(link_result()));
}

#[test]
fn moving_a_todo_is_a_definition_change_and_its_state_follows_its_id() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::advance(tree, ctx, "l1", false)
    });
    let tree = loaded.work_tree().unwrap();
    let change = commands::todo_move(&tree, &ctx(), "l1", 0, 2).unwrap();
    let Routed::Definition {
        definition, state, ..
    } = route(&tree, &change).unwrap()
    else {
        panic!("todo order is a definition");
    };
    assert!(
        state
            .iter()
            .all(|op| matches!(op, StateOp::AppendHistory { .. }))
    );
    let DefinitionOp::Write { bytes, .. } = &definition[0] else {
        panic!("expected a write");
    };
    let moved: TaskDefinition = toml::from_str(std::str::from_utf8(bytes).unwrap()).unwrap();
    let state_now = writer(&repo).read(|read| read.task("l1")).unwrap().unwrap();
    let joined = join(moved, state_now);
    let todos: Vec<(&str, TodoState)> = joined
        .todos
        .iter()
        .map(|todo| (todo.name.as_str(), todo.state))
        .collect();
    assert_eq!(
        todos,
        [
            ("review", TodoState::Open),
            ("check", TodoState::Open),
            ("implement", TodoState::Done),
        ]
    );
}

#[test]
fn a_new_task_is_a_definition_write_in_its_parent_folder_with_prompts() {
    let root = TempDir::new().unwrap();
    let (_repo, loaded) = loaded(&root);
    let tree = loaded.work_tree().unwrap();
    let new = NewTask {
        goal: "Write the migration".to_string(),
        kind: Some(Kind::Other),
        repository: None,
        todos: None,
    };
    let change = commands::new_child(&tree, &ctx(), "l1", new).unwrap();
    let Routed::Definition {
        definition,
        state,
        files,
    } = route(&tree, &change).unwrap()
    else {
        panic!("a new task is a definition");
    };
    let paths: Vec<&str> = definition
        .iter()
        .filter_map(|op| match op {
            DefinitionOp::Write { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        paths.contains(&"tasks/backfill/tasks/write-the-migration/task.toml"),
        "{paths:?}"
    );
    let new_id = change.file().map(|file| file.id.clone());
    let new_id = new_id.unwrap_or_else(|| {
        change
            .writes
            .iter()
            .find_map(|write| match write {
                commands::Write::Task { file, .. } if file.goal == "Write the migration" => {
                    Some(file.id.clone())
                }
                _ => None,
            })
            .unwrap()
    });
    assert!(files.contains(&FileOp::CreatePrompts { task: new_id }));
    assert!(
        state
            .iter()
            .any(|op| matches!(op, StateOp::AppendHistory { .. }))
    );
}

#[test]
fn moving_a_task_is_a_definition_move() {
    let root = TempDir::new().unwrap();
    let (_repo, loaded) = loaded(&root);
    let tree = loaded.work_tree().unwrap();
    let change = commands::move_task(&tree, &ctx(), "l2", "l1");
    let change = match change {
        Ok(change) => change,
        // A leaf with todos may refuse to become a group; move under the top group's sibling
        // is the same routing either way.
        Err(reason) => panic!("move refused: {reason}"),
    };
    let Routed::Definition { definition, .. } = route(&tree, &change).unwrap() else {
        panic!("a move is a definition");
    };
    assert!(
        definition.contains(&DefinitionOp::Move {
            from: "tasks/report".to_string(),
            to: "tasks/backfill/tasks/report".to_string(),
        }),
        "{definition:?}"
    );
}

#[test]
fn a_link_is_written_into_the_files_folder_at_once() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    let git_before = git_snapshot(&repo);
    let tree = run_state_only(&repo, &loaded, |tree, ctx| {
        commands::artifact_link(tree, ctx, "l1", "https://example.com/doc", "Design doc")
    });
    let link = repo.location().files.join("l1/design-doc.link.toml");
    assert!(link.exists());
    assert!(
        tree.task_by_id("l1")
            .unwrap()
            .files
            .contains_key(Path::new("design-doc.link.toml"))
    );
    assert_eq!(git_snapshot(&repo), git_before);
}

#[test]
fn a_move_of_task_files_moves_between_files_folders() {
    let root = TempDir::new().unwrap();
    let (_repo, loaded) = loaded(&root);
    let tree = loaded.work_tree().unwrap();
    let files = tree.root.clone();
    let change = Change {
        dir: files.join("l1"),
        writes: Vec::new(),
        entry: bongos_work_model::HistoryEntry {
            at: "2026-11-13T10:00:00Z".parse().unwrap(),
            by: "tester".to_string(),
            event: "moved".to_string(),
            detail: None,
            extra: Default::default(),
        },
        moves: vec![commands::Move {
            from: files.join("l1/chats"),
            to: files.join("l2/chats"),
        }],
    };
    let Routed::StateOnly { files: ops, .. } = route(&tree, &change).unwrap() else {
        panic!("moving files is not a definition");
    };
    assert_eq!(
        ops,
        [FileOp::Move {
            from: PathBuf::from("l1/chats"),
            to: PathBuf::from("l2/chats"),
        }]
    );
}

#[test]
fn owns_only_changes_in_its_project_and_projection_uses_definition_folders() {
    let root = TempDir::new().unwrap();
    let (_repo, loaded) = loaded(&root);
    let tree = loaded.work_tree().unwrap();
    let change = commands::close(&tree, &ctx(), "l1").unwrap();
    assert!(owns(&tree, "project-1", &change));
    assert!(!owns(&tree, "another-project", &change));

    let copy = Path::new("/w/copy/definitions");
    let projected = project_into(&tree, copy);
    assert_eq!(projected.task_by_id("g1").unwrap().dir, copy);
    assert_eq!(
        projected.task_by_id("l2").unwrap().dir,
        copy.join("tasks/report")
    );
}

#[test]
fn a_set_of_an_unknown_or_reserved_field_is_refused() {
    let root = TempDir::new().unwrap();
    let (repo, _loaded) = loaded(&root);
    for field in ["todos", "history", "goal", "nonsense"] {
        let op = StateOp::Set {
            task: "l1".to_string(),
            field: field.to_string(),
            expected: None,
            value: Some(toml::Value::String("x".to_string())),
        };
        let error = writer(&repo)
            .transact(|tx| tx.apply_ops(std::slice::from_ref(&op)))
            .unwrap_err();
        assert!(error.to_string().contains(field), "{error}");
    }
    assert_eq!(writer(&repo).read(|read| read.task("l1")).unwrap(), None);
}

/// The state of a todo in a state row; a todo with no record there is open.
fn todo_state(record: &bongos_project_repo_model::TaskStateRecord, todo_id: &str) -> TodoState {
    record
        .todos
        .iter()
        .find(|todo| todo.id.as_deref() == Some(todo_id))
        .map_or(TodoState::Open, |todo| todo.state)
}

#[test]
fn a_todo_moved_to_another_task_keeps_its_state() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::todo_skip(tree, ctx, "l1", Some(1))
    });
    let tree = loaded.work_tree().unwrap();
    let mut from = tree.task_by_id("l1").unwrap().file.clone().unwrap();
    let mut to = tree.task_by_id("l2").unwrap().file.clone().unwrap();
    let moved = from.todos.remove(1);
    assert_eq!(moved.id.as_deref(), Some("t2"));
    to.todos.push(moved);
    let change = Change {
        dir: tree.task_by_id("l1").unwrap().dir.clone(),
        writes: vec![
            commands::Write::Task {
                dir: tree.task_by_id("l1").unwrap().dir.clone(),
                file: from,
            },
            commands::Write::Task {
                dir: tree.task_by_id("l2").unwrap().dir.clone(),
                file: to,
            },
        ],
        entry: bongos_work_model::HistoryEntry {
            at: "2026-11-13T10:00:00Z".parse().unwrap(),
            by: "tester".to_string(),
            event: "todo moved".to_string(),
            detail: None,
            extra: Default::default(),
        },
        moves: Vec::new(),
    };
    let Routed::Definition { state, .. } = route(&tree, &change).unwrap() else {
        panic!("a todo's task is a definition");
    };
    assert_eq!(
        state,
        [StateOp::MoveTodo {
            from_task: "l1".to_string(),
            to_task: "l2".to_string(),
            todo_id: "t2".to_string(),
        }]
    );
    writer(&repo)
        .transact(|tx| tx.apply_ops(&state))
        .unwrap();
    let l2 = writer(&repo).read(|read| read.task("l2")).unwrap().unwrap();
    assert_eq!(todo_state(&l2, "t2"), TodoState::Skipped);
    let l1 = writer(&repo).read(|read| read.task("l1")).unwrap().unwrap();
    assert!(l1.todos.iter().all(|todo| todo.id.as_deref() != Some("t2")));
}

#[test]
fn a_split_moves_todos_and_work_as_the_rows_hold_them_when_applied() {
    let root = TempDir::new().unwrap();
    let (repo, loaded) = loaded(&root);
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::advance(tree, ctx, "l1", false)
    });
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::set_result(tree, ctx, "l1", link_result())
    });
    let tree = loaded.work_tree().unwrap();
    let change = commands::split(&tree, &ctx(), "l1", Some("Keeper")).unwrap();
    let keeper = change
        .writes
        .iter()
        .find_map(|write| match write {
            commands::Write::Task { file, .. } if file.goal == "Keeper" => Some(file.id.clone()),
            _ => None,
        })
        .unwrap();
    let Routed::Definition { state, .. } = route(&tree, &change).unwrap() else {
        panic!("a split is a definition");
    };
    for todo_id in ["t1", "t2", "t3"] {
        assert!(
            state.contains(&StateOp::MoveTodo {
                from_task: "l1".to_string(),
                to_task: keeper.clone(),
                todo_id: todo_id.to_string(),
            }),
            "{state:?}"
        );
    }
    assert!(state.contains(&StateOp::MoveWork {
        from_task: "l1".to_string(),
        to_task: keeper.clone(),
    }));
    // Nothing moved is also set.
    assert!(
        !state.iter().any(|op| match op {
            StateOp::SetTodo { .. } => true,
            StateOp::Set { field, .. } => field == "result",
            _ => false,
        }),
        "{state:?}"
    );

    // While the split waits to land, a todo is skipped and the result changes.
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::todo_skip(tree, ctx, "l1", None)
    });
    let mut newer = link_result();
    newer.url = Some("https://example.com/newer".to_string());
    let newer_result = newer.clone();
    run_state_only(&repo, &loaded, |tree, ctx| {
        commands::clear_result(tree, ctx, "l1")
    });
    run_state_only(&repo, &loaded, move |tree, ctx| {
        commands::set_result(tree, ctx, "l1", newer.clone())
    });

    writer(&repo)
        .transact(|tx| tx.apply_ops(&state))
        .unwrap();
    let kept = writer(&repo)
        .read(|read| read.task(&keeper))
        .unwrap()
        .unwrap();
    assert_eq!(todo_state(&kept, "t1"), TodoState::Done);
    assert_eq!(todo_state(&kept, "t2"), TodoState::Skipped);
    assert_eq!(todo_state(&kept, "t3"), TodoState::Open);
    assert_eq!(kept.result, Some(newer_result));
    let parent = writer(&repo).read(|read| read.task("l1")).unwrap().unwrap();
    assert!(parent.todos.is_empty());
    assert_eq!(parent.result, None);
}

#[test]
fn a_link_at_a_moved_tasks_new_place_goes_into_that_tasks_files_folder() {
    let root = TempDir::new().unwrap();
    let (_repo, loaded) = loaded(&root);
    let tree = loaded.work_tree().unwrap();
    let mut change = commands::move_task(&tree, &ctx(), "l2", "l1").unwrap();
    let old_dir = tree.task_by_id("l2").unwrap().dir.clone();
    let new_dir = change
        .moves
        .iter()
        .find(|moved| moved.from == old_dir)
        .unwrap()
        .to
        .clone();
    let link = commands::artifact_link(&tree, &ctx(), "l2", "https://example.com/doc", "Doc")
        .unwrap()
        .writes
        .into_iter()
        .find_map(|write| match write {
            commands::Write::Link { path, file } => Some((path, file)),
            _ => None,
        })
        .unwrap();
    let name = link.0.strip_prefix(&old_dir).unwrap().to_path_buf();
    change.writes.push(commands::Write::Link {
        path: new_dir.join(&name),
        file: link.1,
    });
    let Routed::Definition {
        definition, files, ..
    } = route(&tree, &change).unwrap()
    else {
        panic!("a move is a definition");
    };
    assert!(
        definition.contains(&DefinitionOp::Move {
            from: "tasks/report".to_string(),
            to: "tasks/backfill/tasks/report".to_string(),
        }),
        "{definition:?}"
    );
    let written: Vec<&PathBuf> = files
        .iter()
        .filter_map(|op| match op {
            FileOp::Write { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(written, [&Path::new("l2").join(&name)]);
}
