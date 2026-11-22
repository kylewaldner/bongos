use std::sync::Arc;

use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState,
    RunSubject, TaskId,
};
use bongos_project_repo::{Location, StateApplyHooks, StateStore, StateWriter};
use bongos_project_repo_model::{DefinitionTree, RelPath};
use tempfile::TempDir;

/// A writer over a state directory inside `root`, with its own hook registry, so tests never
/// share the global one.
pub(crate) fn writer(root: &TempDir) -> Arc<StateWriter> {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    };
    Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap())
}

/// A pinned time; nothing here reads the clock.
pub(crate) const SINCE: &str = "2026-03-01T10:00:00Z";

/// The lanes state where lane `build-1` owns the top-level task `feature` and runs `run-lane` on
/// its leaf `leaf`, and lane `docs-1` is idle.
pub(crate) fn lanes_state() -> LanesState {
    let build = LaneId::new("build", 1);
    let mut running = LaneRecord::new(&build, LaneState::Running);
    running.job = Some(JobRecord {
        run_id: "run-lane".to_string(),
        subject: RunSubject::Task(TaskId::new("leaf")),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: Some("c1".to_string()),
    });
    running.jobs_done = 2;
    let docs = LaneId::new("docs", 1);
    let idle = LaneRecord::new(&docs, LaneState::Idle);
    let owner = OwnerRecord {
        lane: Some(build.clone()),
        state: OwnerState::Active,
        since: SINCE.to_string(),
        by: Author::Lane(build.clone()),
        reason: None,
        prepared_commit: None,
        refreshed: false,
    };
    let done = OwnerRecord {
        lane: None,
        state: OwnerState::Done,
        since: SINCE.to_string(),
        by: Author::Person("kim".to_string()),
        reason: Some("finished".to_string()),
        prepared_commit: Some("c0".to_string()),
        refreshed: true,
    };
    LanesState {
        owners: [
            (TaskId::new("feature"), owner),
            (TaskId::new("shipped"), done),
        ]
        .into(),
        lanes: [(build, running), (docs, idle)].into(),
    }
}

/// The `task.toml` of a task with id and goal `id`, followed by `extra` lines.
pub(crate) fn task_toml(id: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{id}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// A tree of the top group `top` and the tasks at `folders`, each with the id of its last part.
pub(crate) fn tree(folders: &[&str]) -> DefinitionTree {
    let tasks: Vec<(&str, &str)> = folders.iter().map(|folder| (*folder, "")).collect();
    tree_with(&tasks)
}

/// [`tree`] whose tasks are `(folder, extra lines)`.
pub(crate) fn tree_with(tasks: &[(&str, &str)]) -> DefinitionTree {
    let mut files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            b"schema = 1\nid = \"p\"\ntitle = \"P\"\n".to_vec(),
        ),
        (
            RelPath::new("task.toml").unwrap(),
            task_toml("top", "").into_bytes(),
        ),
    ];
    for (folder, extra) in tasks {
        let id = folder.rsplit('/').next().unwrap();
        files.push((
            RelPath::new(format!("{folder}/task.toml")).unwrap(),
            task_toml(id, extra).into_bytes(),
        ));
    }
    DefinitionTree::parse_files(files).expect("the fixture parses")
}
