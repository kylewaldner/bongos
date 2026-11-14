//! `task: split` through the router and a quick edit: the new child's definition, the parent's
//! edit and the move of its state and files land together, or none of them does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_project_repo_model::Author;
use bongos_work_model::TodoState;
use bongos_work_model::commands::{self, Change, Context};
use bongos_work_model::store::WorkTree;
use futures::FutureExt as _;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::quick_edit_tests::{
    copy_path, landing, loaded, quick_edit_of, staged, start_edit, unattributed, until_waiting,
};
use crate::router::{FileOp, Routed, apply_state_only, route};
use crate::test_support::{fixture_project, keyboard, main_commit, refs, run_git, start};
use crate::{
    CheckOutcome, LandingResult, LoadedProject, ProjectRepo, QuickEdit, StateOp,
    landing::ProbePoint,
};

fn split_l1(tree: &WorkTree, ctx: &Context) -> commands::Outcome {
    commands::split(tree, ctx, "l1", Some("Keeper"))
}

fn state_only(repo: &ProjectRepo, ops: &[StateOp]) {
    apply_state_only(
        &repo.writer_or_error().unwrap(),
        &repo.location().files,
        ops,
        &[],
        &Author::unattributed("engine"),
    )
    .unwrap();
}

fn set_branch(repo: &ProjectRepo, branch: &str) {
    state_only(
        repo,
        &[StateOp::Set {
            task: "l1".to_string(),
            field: "branch".to_string(),
            expected: None,
            value: Some(toml::Value::String(branch.to_string())),
        }],
    );
}

/// Starts `l1`: its first todo done, a branch, and chats and runs in its files folder.
fn start_leaf(repo: &ProjectRepo, loaded: &LoadedProject) {
    let tree = loaded.work_tree().unwrap();
    let change = commands::advance(&tree, &unattributed(), "l1", false).unwrap();
    let Routed::StateOnly { state, files } = route(&tree, &change).unwrap() else {
        panic!("advance is state only");
    };
    apply_state_only(
        &repo.writer_or_error().unwrap(),
        &repo.location().files,
        &state,
        &files,
        &Author::unattributed("tester"),
    )
    .unwrap();
    set_branch(repo, "work/l1");
    let files = repo.location().files.join("l1");
    std::fs::create_dir_all(files.join("chats")).unwrap();
    std::fs::write(files.join("chats/a.txt"), "chat").unwrap();
    std::fs::create_dir_all(files.join("runs")).unwrap();
    std::fs::write(files.join("runs/r.txt"), "run").unwrap();
}

fn keeper_id(loaded: &LoadedProject) -> Option<String> {
    block_on(loaded.load()).unwrap();
    let tree = loaded.work_tree().unwrap();
    tree.task_by_id("l1")?
        .children
        .first()
        .and_then(|child| child.id().map(str::to_string))
}

fn task_state(repo: &ProjectRepo, id: &str) -> bongos_project_repo_model::TaskStateRecord {
    repo.read_state(|read| read.task(id))
        .unwrap()
        .unwrap_or_default()
}

fn todo_state(state: &bongos_project_repo_model::TaskStateRecord, id: &str) -> TodoState {
    state
        .todos
        .iter()
        .find(|todo| todo.id.as_deref() == Some(id))
        .map(|todo| todo.state)
        .unwrap_or(TodoState::Open)
}

fn files_of(repo: &ProjectRepo, id: &str) -> Vec<String> {
    let dir = repo.location().files.join(id);
    let mut found = Vec::new();
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                found.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    walk(&dir, &dir, &mut found);
    found.sort();
    found
}

#[test]
fn a_refused_split_leaves_state_and_files_unchanged() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_landing_check(Arc::new(
        crate::check_fakes::FakeCheck::new(
            "refuser",
            bongos_project_repo_model::checks::CheckPhase::BeforeQueue,
        )
        .answering(vec![CheckOutcome::fail("not like this")]),
    ));
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let before = main_commit(&repo);
    let state_before = task_state(&repo, "l1");
    let files_before = files_of(&repo, "l1");
    assert_eq!(files_before, ["chats/a.txt", "runs/r.txt"]);

    let started = start_edit(&repo, &loaded, keyboard(), split_l1);
    // Nothing moved while the change waits.
    assert_eq!(files_of(&repo, "l1"), files_before);
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(
        matches!(result, LandingResult::Refused { .. }),
        "{result:?}"
    );
    assert_eq!(main_commit(&repo), before);
    assert_eq!(staged(&repo, &started.change_id), None);
    assert_eq!(task_state(&repo, "l1"), state_before);
    assert_eq!(files_of(&repo, "l1"), files_before);
    assert!(keeper_id(&loaded).is_none());
    // The state of the work stays on the parent: its branch is untouched, no row for a child.
    assert_eq!(state_before.branch.as_deref(), Some("work/l1"));
    let dirs: Vec<_> = std::fs::read_dir(&repo.location().files)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(dirs, ["l1"]);
    // Only the repair copy of the refused change is checked out.
    assert!(copy_path(&repo, &started.change_id).exists());
}

#[test]
fn a_split_of_a_started_leaf_lands_with_its_state_and_files() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let before = main_commit(&repo);

    let started = start_edit(&repo, &loaded, keyboard(), split_l1);
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
    assert_ne!(main_commit(&repo), before);

    let keeper = keeper_id(&loaded).expect("the keeper child exists");
    let parent = task_state(&repo, "l1");
    let child = task_state(&repo, &keeper);
    assert_eq!(child.branch.as_deref(), Some("work/l1"));
    assert_eq!(parent.branch, None);
    assert!(parent.todos.is_empty());
    assert_eq!(todo_state(&child, "t1"), TodoState::Done);
    assert_eq!(child.todos.len(), 1);
    assert_eq!(files_of(&repo, "l1"), Vec::<String>::new());
    assert_eq!(
        files_of(&repo, &keeper),
        ["chats/a.txt", "prompts.txt", "runs/r.txt"]
    );
}

#[test]
fn a_skipped_todo_arrives_skipped_and_a_newer_set_is_kept() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let tree = loaded.work_tree().unwrap();
    let mut ctx = keyboard();
    let marker = ctx.take_attestation();
    let change: Change = split_l1(&tree, &ctx).unwrap();
    let mut edit: QuickEdit = quick_edit_of(&loaded, &tree, &change, &ctx.author);
    let set = |value: &str| StateOp::Set {
        task: "l1".to_string(),
        field: "status".to_string(),
        expected: Some(toml::Value::String("open".to_string())),
        value: Some(toml::Value::String(value.to_string())),
    };
    edit.state.push(set("done"));
    let started = block_on(repo.quick_edit(edit, marker, start())).unwrap();

    // While it waits, the next todo is skipped and the status is set.
    let tree = loaded.work_tree().unwrap();
    let skip = commands::todo_skip(&tree, &unattributed(), "l1", None).unwrap();
    let Routed::StateOnly { state, files } = route(&tree, &skip).unwrap() else {
        panic!("skip is state only");
    };
    apply_state_only(
        &repo.writer_or_error().unwrap(),
        &repo.location().files,
        &state,
        &files,
        &Author::unattributed("tester"),
    )
    .unwrap();
    state_only(
        &repo,
        &[StateOp::Set {
            task: "l1".to_string(),
            field: "status".to_string(),
            expected: Some(toml::Value::String("open".to_string())),
            value: Some(toml::Value::String("cancelled".to_string())),
        }],
    );

    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
    let keeper = keeper_id(&loaded).unwrap();
    let child = task_state(&repo, &keeper);
    assert_eq!(todo_state(&child, "t1"), TodoState::Done);
    assert_eq!(todo_state(&child, "t2"), TodoState::Skipped);
    assert_eq!(todo_state(&child, "t3"), TodoState::Open);
    let parent = task_state(&repo, "l1");
    assert_eq!(
        serde_json::to_value(parent.status).unwrap(),
        serde_json::json!("cancelled")
    );
}

#[test]
fn a_crash_after_the_state_transaction_still_completes_state_and_files() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let started = start_edit(&repo, &loaded, keyboard(), split_l1);
    repo.set_probe(Arc::new(|point, _| {
        async move { point == ProbePoint::AfterStateTransaction }.boxed()
    }));
    assert!(block_on(repo.land_quick_edit(&started.change_id, start())).is_err());
    // The state moved; the files and the ref did not.
    assert_eq!(task_state(&repo, "l1").branch, None);
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);

    drop(loaded);
    let repo = crate::test_support::reopen(repo, &Default::default());
    let recovery = block_on(repo.recover_landing()).unwrap();
    assert!(
        matches!(recovery, crate::Recovery::Completed { .. }),
        "{recovery:?}"
    );
    let loaded = crate::quick_edit_tests::loaded(&repo);
    let keeper = keeper_id(&loaded).expect("the keeper child landed");
    assert_eq!(files_of(&repo, "l1"), Vec::<String>::new());
    assert_eq!(
        files_of(&repo, &keeper),
        ["chats/a.txt", "prompts.txt", "runs/r.txt"]
    );
    assert_eq!(
        task_state(&repo, &keeper).branch.as_deref(),
        Some("work/l1")
    );
    // Running the moves again changes nothing.
    assert_eq!(
        block_on(repo.recover_landing()).unwrap(),
        crate::Recovery::Nothing
    );
}

#[test]
fn an_abandoned_split_drops_its_state_and_moves_no_files() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let before = main_commit(&repo);
    let state_before = task_state(&repo, "l1");
    let refs_before = refs(&repo);

    let started = start_edit(&repo, &loaded, unattributed(), split_l1);
    let mut events = repo.landing_events();
    let mut running = landing(&repo, &started.change_id);
    let result = block_on(async {
        until_waiting(&mut running, &mut events, &started.change_id).await;
        repo.abandon_now(&started.change_id, start()).await.unwrap();
        running.await.unwrap()
    });
    assert_eq!(result, LandingResult::Abandoned);
    assert_eq!(main_commit(&repo), before);
    assert_eq!(staged(&repo, &started.change_id), None);
    assert_eq!(task_state(&repo, "l1"), state_before);
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);
    assert!(keeper_id(&loaded).is_none());
    // The branch of the abandoned change is gone too.
    assert!(!run_git(&repo, &["branch", "--list"]).contains(&started.branch));
    assert_eq!(refs(&repo), refs_before);
}

fn edit_with_file_move(loaded: &LoadedProject, from: &str, to: &str) -> QuickEdit {
    let tree = loaded.work_tree().unwrap();
    let change = commands::edit(
        &tree,
        &keyboard(),
        "l1",
        commands::TaskEdit {
            goal: Some("Backfill events".to_string()),
            kind: None,
            repository: None,
        },
    )
    .unwrap();
    let mut edit = quick_edit_of(loaded, &tree, &change, &keyboard().author);
    edit.files.push(FileOp::Move {
        from: PathBuf::from(from),
        to: PathBuf::from(to),
    });
    edit
}

#[test]
fn a_file_move_onto_an_existing_destination_is_refused_with_nothing_moved() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let files = repo.location().files.clone();
    let before = main_commit(&repo);
    let moves = |edit: &mut QuickEdit| {
        // Two moves: the first is fine, the second collides; neither may happen.
        edit.files.push(FileOp::Move {
            from: PathBuf::from("l1/runs"),
            to: PathBuf::from("l2/runs"),
        });
        edit.files.push(FileOp::Move {
            from: PathBuf::from("l1/chats"),
            to: PathBuf::from("l2/chats"),
        });
    };

    // A destination that exists when the edit is made refuses it: no branch, no row.
    std::fs::create_dir_all(files.join("l2/chats")).unwrap();
    std::fs::write(files.join("l2/chats/b.txt"), "other").unwrap();
    let refs_before = refs(&repo);
    let mut edit = edit_with_file_move(&loaded, "l1/runs", "l2/other");
    edit.files.clear();
    moves(&mut edit);
    let error = block_on(repo.quick_edit(edit, None, start())).unwrap_err();
    assert!(format!("{error:#}").contains("already exists"), "{error:#}");
    assert_eq!(refs(&repo), refs_before);
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);

    // One made while the change waits refuses the landing.
    std::fs::remove_dir_all(files.join("l2/chats")).unwrap();
    let mut edit = edit_with_file_move(&loaded, "l1/runs", "l2/other");
    edit.files.clear();
    moves(&mut edit);
    let mut ctx = keyboard();
    let marker = ctx.take_attestation();
    let started = block_on(repo.quick_edit(edit, marker, start())).unwrap();
    std::fs::create_dir_all(files.join("l2/chats")).unwrap();
    std::fs::write(files.join("l2/chats/b.txt"), "other").unwrap();
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    let LandingResult::Refused { reasons } = result else {
        panic!("expected a refusal, got {result:?}");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("already exists")),
        "{reasons:?}"
    );
    assert_eq!(main_commit(&repo), before);
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);
    assert_eq!(files_of(&repo, "l2"), ["chats/b.txt"]);
    assert_eq!(staged(&repo, &started.change_id), None);
}

#[test]
fn a_file_move_from_outside_the_files_folders_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let outside = root.path().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    let refs_before = refs(&repo);
    for (from, to) in [
        ("../secret.txt", "l1/secret.txt"),
        ("/etc/hostname", "l1/hostname"),
        ("l1/chats", "../escaped"),
    ] {
        let edit = edit_with_file_move(&loaded, from, to);
        assert!(
            block_on(repo.quick_edit(edit, None, start())).is_err(),
            "{from} to {to}"
        );
    }
    // None of those made a branch.
    assert_eq!(refs(&repo), refs_before);
    // A staged op written by someone else is refused at landing, with nothing moved.
    let edit = edit_with_file_move(&loaded, "l1/chats", "l2/chats");
    let mut ctx = keyboard();
    let marker = ctx.take_attestation();
    let started = block_on(repo.quick_edit(edit, marker, start())).unwrap();
    let forged = vec![StateOp::MoveFiles {
        from: PathBuf::from("../secret.txt"),
        to: PathBuf::from("l1/secret.txt"),
    }];
    repo.writer_or_error()
        .unwrap()
        .transact(|tx| tx.stage(&started.change_id, &forged))
        .unwrap();
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(
        matches!(result, LandingResult::Refused { .. }),
        "{result:?}"
    );
    assert!(outside.is_file());
    assert!(!repo.location().files.join("l1/secret.txt").exists());
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);
}

#[test]
fn file_moves_that_fail_after_the_landing_stay_recorded_for_the_recovery() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    start_leaf(&repo, &loaded);
    let started = start_edit(&repo, &loaded, keyboard(), split_l1);
    let keeper = staged(&repo, &started.change_id)
        .unwrap()
        .into_iter()
        .find_map(|op| match op {
            StateOp::CreatePrompts { task } => Some(task),
            _ => None,
        })
        .expect("the keeper's files folder waits for the landing");
    // A file where the keeper's files folder goes: the moves pass their checks (nothing is at
    // their destinations) but cannot be made.
    let files = repo.location().files.clone();
    std::fs::write(files.join(&keeper), "in the way").unwrap();

    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);
    let recorded = || {
        repo.read_state(|read| read.meta(crate::landing::LANDING_FILE_MOVES_KEY))
            .unwrap()
    };
    assert!(recorded().is_some());
    // The recovery tries again, fails, says so and keeps the record.
    let error = block_on(repo.recover_landing()).unwrap_err();
    assert!(
        format!("{error:#}").contains("could not be moved"),
        "{error:#}"
    );
    assert!(recorded().is_some());
    assert_eq!(files_of(&repo, "l1"), ["chats/a.txt", "runs/r.txt"]);

    // Once the way is clear, the next recovery moves them and clears the record.
    std::fs::remove_file(files.join(&keeper)).unwrap();
    assert_eq!(
        block_on(repo.recover_landing()).unwrap(),
        crate::Recovery::Nothing
    );
    assert_eq!(recorded(), None);
    assert_eq!(files_of(&repo, "l1"), Vec::<String>::new());
    assert_eq!(
        files_of(&repo, &keeper),
        ["chats/a.txt", "prompts.txt", "runs/r.txt"]
    );
}
