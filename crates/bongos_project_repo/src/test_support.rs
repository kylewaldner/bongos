use std::sync::Arc;

use bongos_project_repo_model::{TaskStateRecord, TodoStateRecord};
use bongos_work_model::{Status, TodoState};
use tempfile::TempDir;

use crate::{Location, StateApplyHooks, StateStore, StateWriter};

/// A project location whose parts are all inside `root`, with an empty state directory.
pub(crate) fn location(root: &TempDir) -> Location {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    }
}

/// A writer with its own hook registry, so tests never share the global one.
pub(crate) fn writer(location: &Location, hooks: &StateApplyHooks) -> Arc<StateWriter> {
    Arc::new(StateStore::open_writer_with_hooks(location, hooks).unwrap())
}

pub(crate) fn todo(id: &str, state: TodoState) -> TodoStateRecord {
    TodoStateRecord {
        id: Some(id.to_string()),
        state,
        started: None,
    }
}

pub(crate) fn task_with_todos(todos: &[(&str, TodoState)]) -> TaskStateRecord {
    TaskStateRecord {
        status: Status::Open,
        todos: todos.iter().map(|(id, state)| todo(id, *state)).collect(),
        ..TaskStateRecord::default()
    }
}
