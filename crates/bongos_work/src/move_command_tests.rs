//! `task: move to project` and `task: make one off` between project repositories: the preview,
//! a refusal that changes nothing, a confirmed move, a move into the one-offs project and the
//! resume of an interrupted move. Real git and real SQLite, each project in its own `TempDir`.

use std::process::Command;
use std::sync::Arc;

use bongos_project_repo::{
    ApprovalHook, ApprovalInput, ApprovalOpinion, AutomaticRule, Git, Identity, LoadedProject,
    Location, MAIN_BRANCH, MoveCoordinator, MoveOutcome, MoveRequest, ProjectRepo, StateApplyHooks,
    StateHandle, StateStore, StateWriter,
};
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::{Author, PersonAtKeyboard};
use chrono::{DateTime, Utc};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::move_commands::{
    confirm_and_run, outcome_message, parent_choices, render_preview, repository_id, resume_moves,
};
use crate::store_tests::{canonical, write};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-14T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn person() -> WorkContext {
    WorkContext::at_keyboard(PersonAtKeyboard::for_test())
}

fn task(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n{extra}\
         created = 2026-01-01T10:00:00Z\n\n[[todos]]\nid = \"{id}-t\"\nname = \"do it\"\n"
    )
}

struct Repo {
    repo: ProjectRepo,
    writer: Arc<StateWriter>,
    _temp: TempDir,
}

/// A project repository `id` titled `title`, its definitions `files` committed on main.
fn project(id: &str, title: &str, repositories: &str, files: &[(&str, String)]) -> Repo {
    let temp = TempDir::new().unwrap();
    let folder = canonical(&temp).join("project");
    let location = Location {
        id: id.to_string(),
        folder: folder.clone(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        state: folder.join("state"),
        files: folder.join("files"),
        remote: None,
        converted_from: None,
    };
    let definitions = location.repository.join("definitions");
    write(
        &definitions.join("project.toml"),
        &format!(
            "schema = 1\nid = \"{id}\"\ntitle = \"{title}\"\nrepositories = [{repositories}]\n"
        ),
    );
    write(&definitions.join("plan.md"), "");
    for (path, text) in files {
        write(&definitions.join(path), text);
    }
    std::fs::create_dir_all(&location.state).unwrap();
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
    let repo = ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer.clone()));
    Repo {
        repo,
        writer,
        _temp: temp,
    }
}

/// Invoicing: `backfill` (l1, in the repository ledger, after `report`) with the child `child`
/// (l3), and `report` (l2), after `backfill`.
fn invoicing() -> Repo {
    project(
        "src-1",
        "Invoicing",
        "\"ledger\"",
        &[
            ("task.toml", task("g1", "Invoicing", "")),
            (
                "tasks/backfill/task.toml",
                task(
                    "l1",
                    "Backfill",
                    "repository = \"ledger\"\nafter = [{ task = \"report\", unblock = \"merged\" }]\n",
                ),
            ),
            (
                "tasks/backfill/tasks/child/task.toml",
                task("l3", "Child", ""),
            ),
            (
                "tasks/report/task.toml",
                task(
                    "l2",
                    "Report",
                    "after = [{ task = \"backfill\", unblock = \"merged\" }]\n",
                ),
            ),
        ],
    )
}

impl Repo {
    /// The files committed on main, relative to `definitions/`.
    fn on_main(&self) -> Vec<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo.location().repository)
            .args(["ls-tree", "-r", "--name-only", MAIN_BRANCH])
            .output()
            .unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("definitions/").map(str::to_string))
            .collect()
    }

    fn main_commit(&self) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo.location().repository)
            .args(["rev-parse", MAIN_BRANCH])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }
}

/// Billing, which already has a task folder `backfill` (b9).
fn billing() -> Repo {
    project(
        "dst-1",
        "Billing",
        "\"billing-api\"",
        &[
            ("task.toml", task("g2", "Billing", "")),
            ("tasks/backfill/task.toml", task("b9", "Old backfill", "")),
        ],
    )
}

/// Approves every change it is asked about under `rule`.
struct ApproveRule(AutomaticRule);

impl ApprovalHook for ApproveRule {
    fn name(&self) -> &str {
        "test rule"
    }

    fn rule(&self) -> Option<AutomaticRule> {
        Some(self.0)
    }

    fn judge(&self, _input: &ApprovalInput) -> ApprovalOpinion {
        ApprovalOpinion::Approve("test rule".to_string())
    }
}

fn request() -> MoveRequest {
    MoveRequest {
        task_id: "l1".to_string(),
        parent_id: "g2".to_string(),
    }
}

#[test]
fn the_preview_shows_the_subtree_the_target_folder_and_what_the_move_drops_and_adds() {
    let (source, target) = (invoicing(), billing());
    let preview = block_on(MoveCoordinator::preview(
        &source.repo,
        &target.repo,
        &request(),
    ))
    .unwrap();
    let text = render_preview(&preview);
    for line in [
        "From Invoicing to Billing\n",
        "Target folder: definitions/tasks/backfill-2\n",
        "Renamed: backfill becomes backfill-2, because Billing already has a task named backfill there\n",
        "Moves 2 tasks:\n",
        "  - l1 (definitions/tasks/backfill becomes definitions/tasks/backfill-2)\n",
        "  - l3 (definitions/tasks/backfill/tasks/child becomes definitions/tasks/backfill-2/tasks/child)\n",
        "  - backfill after report\n",
        "  - report after backfill\n",
        "Repositories Billing lacks, added to its project.toml:\n  - ledger\n",
    ] {
        assert!(text.contains(line), "{line:?} in\n{text}");
    }
    assert!(!text.contains("Cannot move"), "{text}");
}

#[test]
fn a_started_child_refuses_the_move_and_changes_nothing() {
    let (source, target) = (invoicing(), billing());
    let plan = block_on(MoveCoordinator::preview(
        &source.repo,
        &target.repo,
        &request(),
    ))
    .unwrap()
    .unwrap();
    // The child starts after the preview was shown.
    source
        .writer
        .transact(|tx| {
            tx.set_task(
                "l3",
                TaskStateRecord {
                    branch: Some("work/child".to_string()),
                    ..TaskStateRecord::default()
                },
            )
        })
        .unwrap();
    let preview = block_on(MoveCoordinator::preview(
        &source.repo,
        &target.repo,
        &request(),
    ))
    .unwrap();
    let text = render_preview(&preview);
    assert!(
        text.contains(
            "Cannot move:\n  - child task child is started; only tasks that have not started can move\n"
        ),
        "{text}"
    );
    let (source_main, target_main) = (source.main_commit(), target.main_commit());
    let message = block_on(confirm_and_run(
        &source.repo,
        &target.repo,
        &request(),
        &plan,
        person(),
        now(),
    ))
    .unwrap();
    assert_eq!(
        message,
        "Not moved: child task child is started; only tasks that have not started can move"
    );
    assert_eq!(source.main_commit(), source_main);
    assert_eq!(target.main_commit(), target_main);
    assert!(source.repo.moves_in_flight().unwrap().is_empty());
    assert!(
        source
            .on_main()
            .contains(&"tasks/backfill/task.toml".to_string())
    );
}

#[test]
fn confirming_moves_the_subtree() {
    let (source, target) = (invoicing(), billing());
    let plan = block_on(MoveCoordinator::preview(
        &source.repo,
        &target.repo,
        &request(),
    ))
    .unwrap()
    .unwrap();
    let message = block_on(confirm_and_run(
        &source.repo,
        &target.repo,
        &request(),
        &plan,
        person(),
        now(),
    ))
    .unwrap();
    assert_eq!(message, "Moved backfill-2 to Billing");
    let moved = target.on_main();
    assert!(
        moved.contains(&"tasks/backfill-2/task.toml".to_string()),
        "{moved:?}"
    );
    assert!(
        moved.contains(&"tasks/backfill-2/tasks/child/task.toml".to_string()),
        "{moved:?}"
    );
    let left = source.on_main();
    assert!(
        !left.iter().any(|path| path.starts_with("tasks/backfill/")),
        "{left:?}"
    );
    assert!(source.repo.moves_in_flight().unwrap().is_empty());
}

#[test]
fn make_one_off_moves_the_task_under_an_activity_of_the_oneoffs_project() {
    let source = invoicing();
    let oneoffs = project(
        "oneoffs-1",
        "One-offs",
        "",
        &[
            ("task.toml", task("g9", "One-offs", "")),
            (
                "tasks/general/task.toml",
                task("a1", "general", "activity = \"general\"\n"),
            ),
            ("tasks/general/tasks/tidy/task.toml", task("o1", "Tidy", "")),
        ],
    );
    let loaded = LoadedProject::new(oneoffs.repo.clone());
    block_on(loaded.load()).unwrap();
    let projects = loaded.projects().unwrap();
    let project = &projects[0];
    assert_eq!(repository_id(project), Some("oneoffs-1"));
    // The one-offs project offers its top level and its activities, not the one-offs in them.
    let choices = parent_choices(project, true);
    let ids: Vec<&str> = choices.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["g9", "a1"]);

    let request = MoveRequest {
        task_id: "l1".to_string(),
        parent_id: "a1".to_string(),
    };
    let plan = block_on(MoveCoordinator::preview(
        &source.repo,
        &oneoffs.repo,
        &request,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(plan.root_to(), "tasks/general/tasks/backfill");
    let message = block_on(confirm_and_run(
        &source.repo,
        &oneoffs.repo,
        &request,
        &plan,
        person(),
        now(),
    ))
    .unwrap();
    assert_eq!(message, "Moved backfill to One-offs");
    assert!(
        oneoffs
            .on_main()
            .contains(&"tasks/general/tasks/backfill/task.toml".to_string())
    );
}

#[test]
fn a_move_left_in_flight_is_resumed_once_both_projects_are_open() {
    // A resumed move has no person at a window: a workflow's move, landed by its rules.
    let (source, target) = (invoicing(), billing());
    for repo in [&source.repo, &target.repo] {
        for rule in [
            AutomaticRule::OwnSubtree,
            AutomaticRule::DeclaredScope,
            AutomaticRule::IndependentReview,
        ] {
            repo.add_approval_hook(Arc::new(ApproveRule(rule)));
        }
    }
    let coordinator = block_on(MoveCoordinator::confirm(
        &source.repo,
        &target.repo,
        &request(),
        Author::workflow("mover"),
        None,
    ))
    .unwrap()
    .unwrap();
    let move_id = coordinator.move_id().to_string();
    drop(coordinator);
    assert_eq!(source.repo.moves_in_flight().unwrap(), [move_id.clone()]);
    // With only the source open the move waits.
    assert!(block_on(resume_moves(&[source.repo.clone()], now())).is_empty());
    let outcomes = block_on(resume_moves(
        &[source.repo.clone(), target.repo.clone()],
        now(),
    ));
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, move_id);
    assert_eq!(outcomes[0].1.as_ref().unwrap(), &MoveOutcome::Moved);
    assert!(source.repo.moves_in_flight().unwrap().is_empty());
}

#[test]
fn the_outcome_names_what_happened() {
    let (source, target) = (invoicing(), billing());
    let plan = block_on(MoveCoordinator::preview(
        &source.repo,
        &target.repo,
        &request(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(
        outcome_message(
            &plan,
            "m1",
            &MoveOutcome::RolledBack {
                reasons: vec!["a check failed".to_string()]
            }
        ),
        "Rolled back: backfill-2 landed in Billing but not in Invoicing, so it was undone: \
         a check failed"
    );
    assert_eq!(
        outcome_message(
            &plan,
            "m1",
            &MoveOutcome::NeedsYou {
                reason: "the rollback conflicts".to_string()
            }
        ),
        "Move m1 needs you: the rollback conflicts"
    );
    assert_eq!(
        outcome_message(
            &plan,
            "m1",
            &MoveOutcome::Refused {
                reasons: vec!["no".to_string()]
            }
        ),
        "Not moved, neither project changed: no"
    );
}
