use std::sync::Arc;

use bongos_checks_model::EvidenceRecord;
use bongos_checks_model::evidence::{Output, StartedBy, TreeKind, Variant};
use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{
    Git, Identity, LandMode, LandRequest, LandingResult, Location, MAIN_BRANCH, ProjectRepo,
    StateApplyHooks, StateHandle, StateStore, StateWriter, WorkingCopy,
};
use bongos_project_repo_model::{Author, ChangeMeta, PersonAtKeyboard};
use bongos_work_model::commands::Context;
use futures::executor::block_on;
use tempfile::TempDir;

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
pub(crate) fn writer(root: &TempDir) -> Arc<StateWriter> {
    let location = location(root);
    Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap())
}

pub(crate) const PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n";

/// The text of a task definition of `kind` other, followed by `extra`.
pub(crate) fn definition_toml(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
         created = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// A new-format project in a real git repository inside `root`: the top group `g1` with the leaf
/// `backfill` (`l1`) committed on `main`, and the state store open with its writer.
pub(crate) fn fixture_project(root: &TempDir) -> ProjectRepo {
    let location = location(root);
    let definitions = location.repository.join("definitions");
    for (path, text) in [
        ("project.toml", PROJECT_TOML.to_string()),
        ("task.toml", definition_toml("g1", "Invoicing", "")),
        ("plan.md", String::new()),
        (
            "tasks/backfill/task.toml",
            definition_toml("l1", "Backfill", ""),
        ),
    ] {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    block_on(async {
        git.init_repo(&location.repository, MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer =
        Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap());
    ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer))
}

fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn keyboard() -> Context {
    Context::at_keyboard(PersonAtKeyboard::for_test())
}

pub(crate) fn person() -> Author {
    keyboard().author
}

pub(crate) fn new_copy(repo: &ProjectRepo, name: &str, author: &Author) -> WorkingCopy {
    block_on(repo.create_working_copy(name, "work/", author, start())).unwrap()
}

/// Writes `text` at `relative` under the working copy's `definitions/`.
pub(crate) fn write_definition(copy: &WorkingCopy, relative: &str, text: &str) {
    let path = copy.path.join("definitions").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Lands the working copy as the person at the keyboard.
pub(crate) fn land(repo: &ProjectRepo, copy: &WorkingCopy, author: &Author) -> LandingResult {
    block_on(repo.land_as_person(
        &copy.change_id,
        LandRequest {
            meta: ChangeMeta::new("land it", author.clone()),
            mode: LandMode::Land,
        },
        keyboard(),
        start(),
    ))
    .unwrap()
}

pub(crate) fn main_commit(repo: &ProjectRepo) -> String {
    block_on(
        repo.git()
            .run(&repo.location().repository, &["rev-parse", "main"]),
    )
    .unwrap()
}

/// The reasons of a refused landing, or a panic naming the result.
pub(crate) fn refusals(result: LandingResult) -> Vec<String> {
    match result {
        LandingResult::Refused { reasons } => reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A finished record of task `task_id`'s check `check` on tree `tree_key`.
pub(crate) fn record(task_id: &str, tree_key: &str, check: &str, id: &str) -> EvidenceRecord {
    EvidenceRecord {
        id: id.to_string(),
        task_id: task_id.to_string(),
        check: check.to_string(),
        digest: "d1".to_string(),
        tree_key: tree_key.to_string(),
        tree: TreeKind::Tip,
        key_inputs: "k1".to_string(),
        variant: Variant {
            scope: bongos_checks_model::evidence::Scope::Items {
                digest: "s1".to_string(),
                count: 6,
            },
            parts: 1,
            params: None,
        },
        pin_commit: "c1".to_string(),
        master_path: "/pin/master".to_string(),
        command: vec!["make".to_string(), "test".to_string()],
        started: "2026-03-01T10:00:00Z".to_string(),
        ended: Some("2026-03-01T10:00:02Z".to_string()),
        duration_ms: Some(2000),
        verdict: Verdict::Pass,
        reason: None,
        exit: Some(bongos_checks_model::Exit::Code(0)),
        summary: Some("6 passed".to_string()),
        failing: Vec::new(),
        scope_reason: None,
        started_by: StartedBy::Person,
        parts: Vec::new(),
        output: Output::Path("checks/out.txt".to_string()),
        cut: 0,
    }
}
