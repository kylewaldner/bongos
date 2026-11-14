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

pub(crate) fn git() -> crate::Git {
    crate::Git::locate().expect("git is installed").isolated()
}

pub(crate) const PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n";

/// A definition `task.toml`: a leaf of `kind` with the given todos (id, name).
pub(crate) fn definition_toml(id: &str, goal: &str, kind: &str, todos: &[(&str, &str)]) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"{kind}\"\n\
         created = 2026-01-01T10:00:00Z\n"
    );
    for (todo_id, name) in todos {
        text.push_str(&format!(
            "\n[[todos]]\nid = \"{todo_id}\"\nname = \"{name}\"\n"
        ));
    }
    text
}

/// The definitions of the fixture project: the top group `g1` with the leaves `backfill` (`l1`,
/// three todos) and `report` (`l2`, one todo).
pub(crate) fn fixture_definitions() -> Vec<(&'static str, String)> {
    vec![
        ("project.toml", PROJECT_TOML.to_string()),
        (
            "task.toml",
            definition_toml("g1", "Invoicing", "other", &[]),
        ),
        ("plan.md", String::new()),
        (
            "tasks/backfill/task.toml",
            definition_toml(
                "l1",
                "Backfill",
                "other",
                &[("t1", "implement"), ("t2", "review"), ("t3", "check")],
            ),
        ),
        (
            "tasks/report/task.toml",
            definition_toml("l2", "Report", "other", &[("r1", "write")]),
        ),
    ]
}

/// A new-format project in a real git repository inside `root`: the fixture definitions
/// committed on `main`, the state store open with its writer. Returns the repository with its
/// state open.
pub(crate) fn fixture_project(root: &TempDir) -> crate::ProjectRepo {
    let location = location(root);
    let definitions = location.repository.join("definitions");
    for (path, text) in fixture_definitions() {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(&location.files).unwrap();
    let git = git();
    futures::executor::block_on(async {
        git.init_repo(&location.repository, crate::MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &crate::Identity::default())
            .await
            .unwrap();
    });
    let writer = writer(&location, &StateApplyHooks::new());
    crate::ProjectRepo::new(location, git).with_state(crate::StateHandle::Writer(writer))
}

/// The commits of every branch and the branch names of the repository.
pub(crate) fn refs(repo: &crate::ProjectRepo) -> String {
    futures::executor::block_on(repo.git().run(
        &repo.location().repository,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    ))
    .unwrap()
}
