//! Moving a task subtree between two projects, each a real git repository and a real state store
//! in its own temporary folder.

use std::path::Path;
use std::sync::Arc;

use bongos_project_repo_model::checks::{APPROVAL_CHECK, CheckOutcome, STARTED_UNCHANGED_CHECK};
use bongos_project_repo_model::moves::MoveRequest;
use bongos_project_repo_model::{Author, ChangeMeta, TaskStateRecord};
use bongos_work_model::{Status, TodoState};
use futures::executor::block_on;
use futures::{FutureExt as _, StreamExt as _};
use tempfile::TempDir;

use crate::check_fakes::{FakeAmender, FakeCheck};
use crate::landing::LandingRecord;
use crate::moves::{MOVE_CLAIMS_NAMESPACE, MOVES_NAMESPACE};
use crate::test_support::{
    definition_toml, git, land_as, landed, main_commit, new_copy, person, reopen, request, run_git,
    start, task_with_todos, with_test_rules, write_definition, writer,
};
use crate::{
    CheckPhase, ClientKind, HostBoundary, LandMode, LandRequest, LandingEvent, LandingResult,
    Location, MoveCoordinator, MoveOutcome, MovePhase, ProjectRepo, RepoCommand, StateApplyHooks,
    StateOp, router,
};

const SOURCE_PROJECT: &str =
    "schema = 1\nid = \"src-1\"\ntitle = \"Invoicing\"\nrepositories = [\"ledger\"]\n";
const TARGET_PROJECT: &str =
    "schema = 1\nid = \"dst-1\"\ntitle = \"Billing\"\nrepositories = [\"billing-api\"]\n";
/// A document of the moved task: carriage returns, trailing spaces, no final newline.
const NOTES: &[u8] = b"Notes \xe2\x80\x94 kept as is\r\nline two   \r\n\tlast";

fn mover() -> Author {
    Author::workflow("mover")
}

/// A project with `files` (relative to `definitions/`) committed on `main`, in `root`.
fn project(root: &TempDir, id: &str, files: &[(&str, Vec<u8>)]) -> ProjectRepo {
    let folder = root.path().join("project");
    let location = Location {
        id: id.to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state: folder.join("state"),
        folder,
        remote: None,
        converted_from: None,
    };
    std::fs::create_dir_all(&location.state).unwrap();
    std::fs::create_dir_all(&location.files).unwrap();
    for (path, bytes) in files {
        let path = location.repository.join("definitions").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let git = git();
    block_on(async {
        git.init_repo(&location.repository, crate::MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &crate::Identity::default())
            .await
            .unwrap();
    });
    let writer = writer(&location, &StateApplyHooks::new());
    with_test_rules(ProjectRepo::new(location, git).with_state(crate::StateHandle::Writer(writer)))
}

fn task(id: &str, goal: &str, extra: &str) -> Vec<u8> {
    let text = definition_toml(id, goal, "other", &[]);
    let (head, rest) = text.split_at(text.find("created").unwrap());
    format!("{head}{extra}{rest}").into_bytes()
}

/// The source, Invoicing: `backfill` (l1, with the child `child`, l3, and a document) after
/// `report` (l2), which is after it too, and `other` (l4).
fn source_project(root: &TempDir) -> ProjectRepo {
    project(
        root,
        "src-1",
        &[
            ("project.toml", SOURCE_PROJECT.as_bytes().to_vec()),
            ("task.toml", task("g1", "Invoicing", "")),
            ("plan.md", Vec::new()),
            (
                "tasks/backfill/task.toml",
                task(
                    "l1",
                    "Backfill",
                    "repository = \"ledger\"\nafter = [{ task = \"report\", unblock = \"merged\" }]\n",
                ),
            ),
            ("tasks/backfill/documents/notes.md", NOTES.to_vec()),
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
            ("tasks/other/task.toml", task("l4", "Other", "")),
        ],
    )
}

/// The target, Billing; with `taken`, it already has a task folder `backfill` (id b9).
fn target_project(root: &TempDir, taken: bool) -> ProjectRepo {
    let mut files = vec![
        ("project.toml", TARGET_PROJECT.as_bytes().to_vec()),
        ("task.toml", task("g2", "Billing", "")),
        ("plan.md", Vec::new()),
    ];
    if taken {
        files.push(("tasks/backfill/task.toml", task("b9", "Old backfill", "")));
    }
    project(root, "dst-1", &files)
}

struct Pair {
    _roots: (TempDir, TempDir),
    source: ProjectRepo,
    target: ProjectRepo,
}

fn pair(taken: bool) -> Pair {
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let source = source_project(&a);
    let target = target_project(&b, taken);
    Pair {
        source,
        target,
        _roots: (a, b),
    }
}

fn move_request() -> MoveRequest {
    MoveRequest {
        task_id: "l1".to_string(),
        parent_id: "g2".to_string(),
    }
}

fn confirm(source: &ProjectRepo, target: &ProjectRepo) -> MoveCoordinator {
    block_on(MoveCoordinator::confirm(
        source,
        target,
        &move_request(),
        mover(),
        None,
    ))
    .unwrap()
    .unwrap()
}

fn set_state(repo: &ProjectRepo, id: &str, record: TaskStateRecord) {
    repo.writer_or_error()
        .unwrap()
        .transact(|tx| tx.set_task(id, record))
        .unwrap();
}

/// A state-only write, as the router applies `task: start`, `close`, `drop` and the engine's.
fn apply(repo: &ProjectRepo, ops: Vec<StateOp>) -> anyhow::Result<()> {
    router::apply_state_only(
        &repo.writer_or_error().unwrap(),
        &repo.location().files,
        &ops,
        &[],
        &mover(),
    )
}

fn set(task: &str, field: &str, value: toml::Value) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: field.to_string(),
        expected: None,
        value: Some(value),
    }
}

fn status(task: &str, status: Status) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: "status".to_string(),
        expected: Some(toml::Value::try_from(Status::Open).unwrap()),
        value: Some(toml::Value::try_from(status).unwrap()),
    }
}

fn claims(repo: &ProjectRepo) -> Vec<(String, String)> {
    repo.read_state(|read| read.kv(MOVE_CLAIMS_NAMESPACE).scan(""))
        .unwrap()
        .into_iter()
        .map(|(task, value)| (task, value.as_str().unwrap().to_string()))
        .collect()
}

/// Everything of a project a move could change: master, every state row, the claims and the
/// files folders.
fn fingerprint(repo: &ProjectRepo) -> String {
    let mut text = main_commit(repo);
    text.push_str(&format!("\n{:?}", verified(repo)));
    let rows = repo
        .read_state(|read| {
            let mut rows = Vec::new();
            for id in read.task_ids()? {
                rows.push((id.clone(), read.task(&id)?));
            }
            Ok(rows)
        })
        .unwrap();
    text.push_str(&format!("\n{rows:?}\n{:?}", claims(repo)));
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            out.push_str(&format!("\n{}", entry.path().display()));
            if entry.path().is_dir() {
                walk(&entry.path(), out);
            } else {
                out.push_str(&format!(" {:?}", std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    walk(&repo.location().files, &mut text);
    text
}

fn verified(repo: &ProjectRepo) -> Option<String> {
    repo.verified_commit().unwrap().map(|commit| commit.0)
}

fn blob(repo: &ProjectRepo, path: &str) -> String {
    run_git(repo, &["rev-parse", &format!("main:definitions/{path}")])
}

fn has_path(repo: &ProjectRepo, path: &str) -> bool {
    block_on(repo.git().run(
        &repo.location().repository,
        &["cat-file", "-e", &format!("main:definitions/{path}")],
    ))
    .is_ok()
}

fn last_message(repo: &ProjectRepo) -> String {
    run_git(repo, &["log", "-1", "--format=%B", "main"])
}

/// Puts the moved tasks' state and files in the source: l1 has an open todo and a note, l3 a
/// chat file.
fn seed_state(source: &ProjectRepo) {
    let mut record = task_with_todos(&[("t1", TodoState::Open)]);
    record.history = Vec::new();
    set_state(source, "l1", record);
    let files = &source.location().files;
    std::fs::create_dir_all(files.join("l1")).unwrap();
    std::fs::write(files.join("l1/prompts.txt"), "prompt\n").unwrap();
    std::fs::create_dir_all(files.join("l3/chats")).unwrap();
    std::fs::write(files.join("l3/chats/one.toml"), b"\x00\x01binary").unwrap();
}

fn run(coordinator: &MoveCoordinator) -> MoveOutcome {
    block_on(coordinator.run(start())).unwrap()
}

#[test]
fn a_move_lands_both_sides_copies_byte_for_byte_and_clears_every_claim() {
    let pair = pair(true);
    let (source, target) = (&pair.source, &pair.target);
    seed_state(source);
    let notes_blob = blob(source, "tasks/backfill/documents/notes.md");
    let child_blob = blob(source, "tasks/backfill/tasks/child/task.toml");

    let preview = block_on(MoveCoordinator::preview(source, target, &move_request()))
        .unwrap()
        .unwrap();
    assert_eq!(preview.slug, "backfill-2");
    assert_eq!(preview.missing_repositories, ["ledger"]);
    let coordinator = confirm(source, target);
    let move_id = coordinator.move_id().to_string();
    assert_eq!(run(&coordinator), MoveOutcome::Moved);

    // The target has the subtree, renamed, its document and child byte for byte.
    assert_eq!(
        blob(target, "tasks/backfill-2/documents/notes.md"),
        notes_blob
    );
    assert_eq!(
        blob(target, "tasks/backfill-2/tasks/child/task.toml"),
        child_blob
    );
    let root = run_git(
        target,
        &["show", "main:definitions/tasks/backfill-2/task.toml"],
    );
    assert!(root.contains("id = \"l1\""));
    assert!(
        !root.contains("after"),
        "the crossing edge stays behind: {root}"
    );
    let project = run_git(target, &["show", "main:definitions/project.toml"]);
    assert!(project.contains("ledger") && project.contains("billing-api"));
    // The source no longer has it, and its sibling's edge to it is gone.
    assert!(!has_path(source, "tasks/backfill/task.toml"));
    let report = run_git(source, &["show", "main:definitions/tasks/report/task.toml"]);
    assert!(!report.contains("after"), "{report}");

    // Both messages name the dropped edges and carry the coordinator's trailer.
    for repo in [source, target] {
        let message = last_message(repo);
        assert!(
            message.contains("Dropped edges: backfill after report, report after backfill"),
            "{message}"
        );
        assert!(
            message.contains(&format!("Bongos-Move: {move_id}")),
            "{message}"
        );
    }

    // The state rows and the files folders moved; no claim is left.
    assert_eq!(
        target.read_state(|read| read.task("l1")).unwrap(),
        Some(task_with_todos(&[("t1", TodoState::Open)]))
    );
    assert_eq!(source.read_state(|read| read.task("l1")).unwrap(), None);
    let target_files = &target.location().files;
    assert_eq!(
        std::fs::read(target_files.join("l1/prompts.txt")).unwrap(),
        b"prompt\n"
    );
    assert_eq!(
        std::fs::read(target_files.join("l3/chats/one.toml")).unwrap(),
        b"\x00\x01binary"
    );
    assert!(!source.location().files.join("l1").exists());
    assert!(!target_files.join(format!(".move-{move_id}")).exists());
    assert!(claims(source).is_empty());
    assert!(claims(target).is_empty());
    assert!(source.moves_in_flight().unwrap().is_empty());
    assert_eq!(
        target.move_record(&move_id).unwrap().unwrap().phase,
        MovePhase::Done
    );
    // The moved task takes state again in the target.
    apply(target, vec![set("l1", "branch", "work/x".into())]).unwrap();
}

#[test]
fn a_started_child_refuses_the_move() {
    let pair = pair(false);
    set_state(
        &pair.source,
        "l3",
        task_with_todos(&[("c1", TodoState::Done)]),
    );
    let refusal = block_on(MoveCoordinator::preview(
        &pair.source,
        &pair.target,
        &move_request(),
    ))
    .unwrap()
    .unwrap_err();
    assert_eq!(
        refusal.reasons,
        ["child task child is started; only tasks that have not started can move"]
    );
}

#[test]
fn an_id_already_in_the_target_refuses_the_move() {
    let pair = pair(false);
    let copy = new_copy(&pair.target, "amber-otter-0001", &person());
    write_definition(
        &copy,
        "tasks/clash/task.toml",
        &definition_toml("l3", "Clash", "other", &[]),
    );
    landed(
        block_on(land_as(
            &pair.target,
            &copy.change_id,
            request("clash", &person()),
            start(),
        ))
        .unwrap(),
    );
    let refusal = block_on(MoveCoordinator::preview(
        &pair.source,
        &pair.target,
        &move_request(),
    ))
    .unwrap()
    .unwrap_err();
    assert_eq!(refusal.reasons, ["task l3 is already in Billing"]);
}

#[test]
fn a_failing_target_check_changes_neither_project() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    seed_state(source);
    target.add_landing_check(Arc::new(
        FakeCheck::new("target-check", CheckPhase::BeforeQueue)
            .answering(vec![CheckOutcome::fail("no room")]),
    ));
    let before = (fingerprint(source), fingerprint(target));
    let coordinator = confirm(source, target);
    let outcome = run(&coordinator);
    assert_eq!(
        outcome,
        MoveOutcome::Refused {
            reasons: vec!["target-check: no room".to_string()]
        }
    );
    assert_eq!((fingerprint(source), fingerprint(target)), before);
    // The source's tasks are free again.
    apply(source, vec![status("l1", Status::Done)]).unwrap();
}

#[test]
fn source_tasks_are_held_from_the_confirmation_on() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let coordinator = confirm(source, target);
    for op in [status("l1", Status::Cancelled), status("l1", Status::Done)] {
        let error = apply(source, vec![op]).unwrap_err().to_string();
        assert!(error.contains("moving to Billing"), "{error}");
    }
    // The child is held too, and the engine's writes are refused like the router's.
    let error = apply(source, vec![set("l3", "branch", "work/child".into())])
        .unwrap_err()
        .to_string();
    assert!(error.contains("moving to Billing"), "{error}");
    // An untouched task is not held.
    apply(source, vec![status("l4", Status::Done)]).unwrap();
    let hold = source.hold("l1").unwrap().unwrap();
    assert_eq!(hold.move_id(), coordinator.move_id());
}

#[test]
fn while_the_source_waits_the_moved_tasks_are_held_in_the_target() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let coordinator = confirm(source, target);
    block_on(coordinator.prepare(start())).unwrap();
    landed(block_on(coordinator.land_target(start())).unwrap());

    let error = apply(target, vec![set("l1", "branch", "work/start".into())])
        .unwrap_err()
        .to_string();
    assert!(error.contains("moving from Invoicing"), "{error}");
    let live = target.live_facts_for(&["l1".to_string()]).unwrap();
    let hold = live.held("l1").expect("held in the target");
    assert_eq!(
        hold.to_string(),
        format!("moving from Invoicing (move {})", coordinator.move_id())
    );
    assert!(live.started("l1"));

    landed(block_on(coordinator.land_source(start())).unwrap());
    assert_eq!(run(&coordinator), MoveOutcome::Moved);
    assert!(target.hold("l1").unwrap().is_none());
    apply(target, vec![set("l1", "branch", "work/start".into())]).unwrap();
}

#[test]
fn the_source_landing_waits_for_the_target_then_lands() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let coordinator = confirm(source, target);
    block_on(coordinator.prepare(start())).unwrap();
    let source_change = coordinator.record().unwrap().source_change.unwrap();
    let mut events = source.landing_events();
    let (source_result, target_result) = block_on(async {
        futures::join!(coordinator.land_source(start()), async {
            // The target lands only once the source shows it waits for it.
            loop {
                let (id, event) = events.next().await.unwrap();
                if id == source_change
                    && matches!(&event, LandingEvent::Waiting { reason, .. }
                        if reason.contains("lands in Billing first"))
                {
                    break;
                }
            }
            coordinator.land_target(start()).await
        })
    });
    landed(target_result.unwrap());
    landed(source_result.unwrap());
    assert_eq!(run(&coordinator), MoveOutcome::Moved);
}

/// The source's in-queue check refuses once: the source is refused after the target landed.
fn refusing_source(source: &ProjectRepo) {
    source.add_landing_check(Arc::new(
        FakeCheck::new("source-check", CheckPhase::InQueue)
            .answering(vec![CheckOutcome::fail("not today")]),
    ));
}

fn approval_line(repo: &ProjectRepo, change_id: &str) -> Option<String> {
    let landings = repo.read_state(|read| read.landings()).unwrap();
    landings
        .into_iter()
        .find(|(id, _)| id == change_id)
        .and_then(|(_, text)| toml::from_str::<LandingRecord>(&text).unwrap().approval)
}

fn rolls_back_with_no_person(amend: bool) {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    seed_state(source);
    refusing_source(source);
    if amend {
        // Amends only the rollback's tree (the one without the moved folder).
        let mut amender = FakeAmender::new(
            "stamp",
            1,
            Arc::new(|writable: &Path| {
                if writable.join("definitions/tasks/backfill").exists() {
                    return false;
                }
                std::fs::write(writable.join("definitions/stamp.txt"), "rolled\n").unwrap();
                true
            }),
        );
        amender.stamp_paths = vec!["stamp.txt"];
        target.add_amending_check(Arc::new(amender));
    }
    let target_before = main_commit(target);
    let tree_before = run_git(target, &["rev-parse", "main^{tree}"]);
    let source_before = fingerprint(source);
    let coordinator = confirm(source, target);
    let move_id = coordinator.move_id().to_string();
    let outcome = run(&coordinator);
    assert_eq!(
        outcome,
        MoveOutcome::RolledBack {
            reasons: vec!["source-check: not today".to_string()]
        }
    );
    let record = coordinator.record().unwrap();
    let rollback = record.rollback_change.unwrap();
    assert_eq!(
        approval_line(target, &rollback).as_deref(),
        Some(
            format!("approved: rollback of move {move_id}, covered by the move's approval")
                .as_str()
        )
    );
    let row = target.change(&rollback).unwrap().unwrap();
    assert_eq!(row.author, mover());
    assert_eq!(!row.amend_commits.is_empty(), amend);
    let message = last_message(target);
    assert!(
        message.contains(&format!("Bongos-Move: {move_id}")),
        "{message}"
    );
    assert!(
        message.contains(&format!("Bongos-Move-Rollback: {move_id}")),
        "{message}"
    );
    assert_ne!(main_commit(target), target_before);
    if !amend {
        assert_eq!(run_git(target, &["rev-parse", "main^{tree}"]), tree_before);
    }
    // The target holds nothing of the move, and the source is as it was, its tasks free.
    assert_eq!(target.read_state(|read| read.task("l1")).unwrap(), None);
    assert!(!target.location().files.join("l1").exists());
    assert!(claims(target).is_empty());
    assert_eq!(fingerprint(source), source_before);
    apply(source, vec![status("l1", Status::Done)]).unwrap();
}

#[test]
fn a_source_failure_after_the_target_landed_lands_the_rollback_with_no_person() {
    rolls_back_with_no_person(false);
}

#[test]
fn a_rollback_amended_by_an_amending_check_keeps_the_moves_approval() {
    rolls_back_with_no_person(true);
}

/// The coordinator up to a recorded rollback: the target landed, the source was refused.
fn up_to_the_rollback(pair: &Pair) -> (MoveCoordinator, String) {
    let (source, target) = (&pair.source, &pair.target);
    refusing_source(source);
    let coordinator = confirm(source, target);
    block_on(coordinator.prepare(start())).unwrap();
    landed(block_on(coordinator.land_target(start())).unwrap());
    let refused = block_on(coordinator.land_source(start())).unwrap();
    assert!(
        matches!(refused, LandingResult::Refused { .. }),
        "{refused:?}"
    );
    assert_eq!(coordinator.record().unwrap().phase, MovePhase::RollingBack);
    let rollback = block_on(coordinator.make_rollback(start())).unwrap();
    (coordinator, rollback.unwrap_or_default())
}

#[test]
fn a_rollback_whose_author_part_differs_from_the_revert_waits_for_a_person() {
    let pair = pair(false);
    let (coordinator, rollback) = up_to_the_rollback(&pair);
    let target = &pair.target;
    let row = target.change(&rollback).unwrap().unwrap();
    let copy = target.working_copy_path(&row.name);
    std::fs::create_dir_all(copy.join("definitions/tasks/extra")).unwrap();
    std::fs::write(
        copy.join("definitions/tasks/extra/task.toml"),
        definition_toml("x1", "Extra", "other", &[]),
    )
    .unwrap();
    block_on(
        target
            .git()
            .commit_if_dirty(&copy, "more", &crate::Identity::default()),
    )
    .unwrap();

    let mut events = target.landing_events();
    let mut landing = coordinator.land_rollback(start()).boxed_local().fuse();
    let reason = block_on(async {
        let reason = loop {
            futures::select! {
                result = landing => panic!("the rollback ended first: {result:?}"),
                next = events.next() => {
                    let (id, event) = next.unwrap();
                    if id == rollback && let LandingEvent::Waiting { reason, .. } = event {
                        break reason;
                    }
                }
            }
        };
        target.abandon_now(&rollback, start()).await.unwrap();
        assert_eq!(landing.await.unwrap(), LandingResult::Abandoned);
        reason
    });
    assert!(
        reason.contains("is not the revert of its landing"),
        "{reason}"
    );
    assert_eq!(coordinator.record().unwrap().phase, MovePhase::NeedsYou);
    // Nothing starts on the held tasks until a person resolves it.
    assert!(pair.source.hold("l1").unwrap().is_some());
    assert!(target.hold("l1").unwrap().is_some());
}

#[test]
fn a_rollback_conflict_leaves_the_claims_and_needs_you() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    refusing_source(source);
    let coordinator = confirm(source, target);
    let move_id = coordinator.move_id().to_string();
    block_on(coordinator.prepare(start())).unwrap();
    landed(block_on(coordinator.land_target(start())).unwrap());
    // A later landing in the target edits the line the move's landing added to.
    let later = new_copy(target, "calm-heron-0002", &person());
    let project_file = later.path.join("definitions/project.toml");
    let text = std::fs::read_to_string(&project_file).unwrap();
    std::fs::write(
        &project_file,
        text.replace("\"ledger\"", "\"ledger\", \"tax\""),
    )
    .unwrap();
    landed(
        block_on(land_as(
            target,
            &later.change_id,
            request("add tax", &person()),
            start(),
        ))
        .unwrap(),
    );

    assert!(matches!(
        run(&coordinator),
        MoveOutcome::NeedsYou { reason } if reason.contains("conflicts with a later landing")
    ));
    assert!(source.hold("l1").unwrap().is_some());
    assert!(target.hold("l1").unwrap().is_some());
    let error = apply(target, vec![set("l1", "branch", "work/x".into())])
        .unwrap_err()
        .to_string();
    assert!(error.contains("moving from Invoicing"), "{error}");
    let record = coordinator.record().unwrap();
    let note = format!("move {move_id} needs you");
    for (repo, change) in [
        (source, record.source_change.unwrap()),
        (target, record.target_change.unwrap()),
    ] {
        let listing = block_on(repo.change_listing()).unwrap();
        let listed = listing
            .changes
            .iter()
            .find(|listed| listed.row.change_id == change)
            .unwrap();
        assert_eq!(listed.move_note.as_deref(), Some(note.as_str()));
    }
}

#[test]
fn a_workflow_change_naming_the_move_that_edits_a_held_task_is_refused_and_not_approved() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let coordinator = confirm(source, target);
    let move_id = coordinator.move_id().to_string();
    let posing = Author::workflow("mover");
    let copy = new_copy(source, "dry-finch-0003", &posing);
    write_definition(&copy, "tasks/backfill/documents/notes.md", "rewritten\n");
    block_on(
        source
            .git()
            .commit_if_dirty(&copy.path, "edit", &crate::Identity::default()),
    )
    .unwrap();
    let mut meta = ChangeMeta::new(format!("part of move {move_id}"), posing);
    meta.body = format!("Bongos-Move: {move_id}\nBongos-Move-Rollback: {move_id}");
    // Sent by a peer at the host boundary, as another process would.
    let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let peer = crate::peer_identity(&ours, ClientKind::Cli).unwrap();
    let boundary = HostBoundary::new(source.clone());
    block_on(boundary.accept_now(
        RepoCommand::Land {
            change_id: copy.change_id.clone(),
            request: LandRequest {
                meta,
                mode: LandMode::Land,
            },
            person: None,
        },
        &peer,
        start(),
    ))
    .unwrap();
    let result = source.landing_result(&copy.change_id).unwrap();
    assert_eq!(result, None, "the change is still requested, refused");
    let stored = source.check_results(&copy.change_id).unwrap();
    let started = stored
        .iter()
        .find(|result| result.check == STARTED_UNCHANGED_CHECK)
        .expect("check 6 ran");
    let message = started.outcome.message().unwrap_or_default().to_string();
    assert!(message.contains("is held: moving to Billing"), "{message}");
    assert!(
        stored.iter().all(|result| result.check != APPROVAL_CHECK),
        "a refused change is never judged for approval"
    );
    assert!(
        source
            .read_state(|read| read.landings())
            .unwrap()
            .is_empty()
    );

    // A peer's state ops on a held task are refused like the router's.
    let error = boundary
        .apply(vec![status("l1", Status::Done)], person(), &peer)
        .unwrap_err()
        .to_string();
    assert!(error.contains("moving to Billing"), "{error}");
}

#[test]
fn the_moves_rows_and_claims_are_written_only_by_the_engine() {
    let pair = pair(false);
    let writer = pair.source.writer_or_error().unwrap();
    for namespace in [MOVES_NAMESPACE, MOVE_CLAIMS_NAMESPACE] {
        let error = writer
            .transact(|tx| tx.kv(namespace).put("l1", "forged".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("written only by the engine"), "{error}");
    }
    // A request naming a path instead of a task, as a peer could send it.
    let request: MoveRequest =
        serde_json::from_str(r#"{"task_id":"../../outside","parent_id":"g2"}"#).unwrap();
    let refusal = block_on(MoveCoordinator::preview(
        &pair.source,
        &pair.target,
        &request,
    ))
    .unwrap()
    .unwrap_err();
    assert!(refusal.reasons[0].contains("is not a task id"), "{refusal}");
}

#[test]
fn a_second_move_of_the_same_subtree_is_refused() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let first = confirm(source, target);
    let refusal = block_on(MoveCoordinator::confirm(
        source,
        target,
        &move_request(),
        mover(),
        None,
    ))
    .unwrap()
    .err()
    .expect("the second move is refused");
    assert_eq!(
        refusal.reasons,
        [
            format!(
                "task backfill is held: moving to Billing (move {})",
                first.move_id()
            ),
            format!(
                "child task child is held: moving to Billing (move {})",
                first.move_id()
            ),
        ]
    );
    // Moving a child alone is refused too.
    let child = MoveRequest {
        task_id: "l3".to_string(),
        parent_id: "g2".to_string(),
    };
    assert!(
        block_on(MoveCoordinator::preview(source, target, &child))
            .unwrap()
            .is_err()
    );
}

#[test]
fn a_crash_between_the_two_landings_resumes_to_completion() {
    let pair = pair(false);
    let Pair {
        _roots,
        source,
        target,
    } = pair;
    seed_state(&source);
    let coordinator = confirm(&source, &target);
    let move_id = coordinator.move_id().to_string();
    block_on(coordinator.prepare(start())).unwrap();
    landed(block_on(coordinator.land_target(start())).unwrap());
    drop(coordinator);

    // The process restarts: both projects open again and the coordinator is rebuilt.
    let hooks = StateApplyHooks::new();
    let source = reopen(source, &hooks);
    let target = reopen(target, &hooks);
    assert_eq!(
        source.moves_in_flight().unwrap(),
        std::slice::from_ref(&move_id)
    );
    let coordinator = MoveCoordinator::resume(&source, &target, &move_id).unwrap();
    assert_eq!(coordinator.record().unwrap().phase, MovePhase::TargetLanded);
    assert_eq!(run(&coordinator), MoveOutcome::Moved);
    assert!(!has_path(&source, "tasks/backfill/task.toml"));
    assert!(has_path(&target, "tasks/backfill/task.toml"));
    assert!(claims(&source).is_empty() && claims(&target).is_empty());
    assert!(target.read_state(|read| read.task("l1")).unwrap().is_some());
    // A coordinator for the wrong target is refused.
    assert!(MoveCoordinator::resume(&source, &source, &move_id).is_err());
}

#[test]
fn moving_back_restores_the_tasks() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    seed_state(source);
    let notes_blob = blob(source, "tasks/backfill/documents/notes.md");
    let child_blob = blob(source, "tasks/backfill/tasks/child/task.toml");
    let state = source.read_state(|read| read.task("l1")).unwrap();
    assert_eq!(run(&confirm(source, target)), MoveOutcome::Moved);

    let back = block_on(MoveCoordinator::confirm(
        target,
        source,
        &MoveRequest {
            task_id: "l1".to_string(),
            parent_id: "g1".to_string(),
        },
        mover(),
        None,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(run(&back), MoveOutcome::Moved);
    assert_eq!(
        blob(source, "tasks/backfill/documents/notes.md"),
        notes_blob
    );
    assert_eq!(
        blob(source, "tasks/backfill/tasks/child/task.toml"),
        child_blob
    );
    assert_eq!(source.read_state(|read| read.task("l1")).unwrap(), state);
    assert_eq!(
        std::fs::read(source.location().files.join("l3/chats/one.toml")).unwrap(),
        b"\x00\x01binary"
    );
    assert!(!has_path(target, "tasks/backfill/task.toml"));
}

#[test]
fn project_revert_of_one_side_is_refused_while_the_other_stands() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let coordinator = confirm(source, target);
    assert_eq!(run(&coordinator), MoveOutcome::Moved);
    let record = coordinator.record().unwrap();
    let error = block_on(source.revert(&record.source_change.unwrap(), &person(), start()))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the other side stands in Billing"),
        "{error}"
    );
    let error = block_on(target.revert(&record.target_change.unwrap(), &person(), start()))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the other side stands in Invoicing"),
        "{error}"
    );
}

#[test]
fn check_six_refuses_a_change_to_a_document_of_a_held_task() {
    let pair = pair(false);
    let (source, target) = (&pair.source, &pair.target);
    let _coordinator = confirm(source, target);
    let author = person();
    let copy = new_copy(source, "keen-otter-0004", &author);
    write_definition(&copy, "tasks/backfill/documents/new.md", "new\n");
    let result = block_on(land_as(
        source,
        &copy.change_id,
        request("doc", &author),
        start(),
    ))
    .unwrap();
    let LandingResult::Refused { reasons } = result else {
        panic!("expected a refusal, got {result:?}");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("is held: moving to Billing")),
        "{reasons:?}"
    );
}
