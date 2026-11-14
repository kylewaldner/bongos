use bongos_project_repo_model::{Author, AuthorKind};
use chrono::{DateTime, Duration, Utc};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::test_support::{fixture_project, refs};
use crate::{
    AbandonRefused, ChangeFilter, ChangeState, LandingResult, ProjectRepo, generated_name,
};

/// The fixed date every time in these tests is an offset from.
fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn create(repo: &ProjectRepo, name: &str, now: DateTime<Utc>) -> crate::WorkingCopy {
    block_on(repo.create_working_copy(name, "work/", &Author::agent("tester"), now)).unwrap()
}

fn main_commit(repo: &ProjectRepo) -> String {
    block_on(
        repo.git()
            .rev_parse(&repo.location().repository, crate::MAIN_BRANCH),
    )
    .unwrap()
}

#[test]
fn two_edits_make_independent_working_copies_outside_the_repository() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let first = create(&repo, "amber-otter-0001", start());
    let second = create(&repo, "calm-heron-0002", start());

    assert_ne!(first.path, second.path);
    assert_ne!(first.change_id, second.change_id);
    assert_eq!(first.branch, "work/amber-otter-0001");
    for copy in [&first, &second] {
        assert!(copy.path.join("definitions/task.toml").is_file());
        assert!(!copy.path.starts_with(&repo.location().repository));
        assert!(copy.path.starts_with(&repo.location().working_copies));
    }
    // A file written in one working copy is in the other only after landing, never before.
    std::fs::write(first.path.join("definitions/note.md"), "only here\n").unwrap();
    assert!(!second.path.join("definitions/note.md").exists());
    assert!(
        !repo
            .location()
            .repository
            .join("definitions/note.md")
            .exists()
    );

    let rows = repo.changes(&ChangeFilter::default()).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.state == ChangeState::Open));
    assert_eq!(rows[0].branch, first.branch);
    assert!(
        block_on(repo.create_working_copy(
            "amber-otter-0001",
            "work/",
            &Author::agent("x"),
            start()
        ))
        .is_err()
    );
}

#[test]
fn abandon_deletes_the_branch_and_worktree_leaves_main_and_marks_the_row() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let before = main_commit(&repo);
    let copy = create(&repo, "amber-otter-0001", start());
    assert!(copy.path.is_dir());

    block_on(repo.abandon_now(&copy.change_id, start() + Duration::hours(1))).unwrap();

    assert!(!copy.path.exists());
    assert!(!block_on(
        repo.git()
            .branch_exists(&repo.location().repository, &copy.branch)
    ));
    assert_eq!(main_commit(&repo), before);
    assert!(!refs(&repo).contains("work/"));
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Abandoned);
    assert!(repo.changes(&ChangeFilter::default()).unwrap().is_empty());
    let abandoned = ChangeFilter {
        state: Some(ChangeState::Abandoned),
        ..ChangeFilter::default()
    };
    assert_eq!(repo.changes(&abandoned).unwrap().len(), 1);
    // Abandoning twice is not an error.
    block_on(repo.abandon_now(&copy.change_id, start())).unwrap();
}

#[test]
fn abandon_refuses_a_landed_change_and_an_unknown_one() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let copy = create(&repo, "amber-otter-0001", start());
    let mut row = repo.change(&copy.change_id).unwrap().unwrap();
    row.state = ChangeState::Landed;
    row.commit = Some("abc123".to_string());
    repo.state()
        .unwrap()
        .writer()
        .unwrap()
        .transact(|tx| tx.set_change(&row))
        .unwrap();

    match block_on(repo.abandon_now(&copy.change_id, start())) {
        Err(AbandonRefused::Landed { commit }) => assert_eq!(commit, "abc123"),
        other => panic!("expected Landed, got {other:?}"),
    }
    assert!(copy.path.is_dir());
    assert!(matches!(
        block_on(repo.abandon_now("nope", start())),
        Err(AbandonRefused::Unknown(_))
    ));
}

#[test]
fn landing_result_is_known_at_once_for_never_requested_and_abandoned_changes() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let copy = create(&repo, "amber-otter-0001", start());
    assert_eq!(
        repo.landing_result(&copy.change_id).unwrap(),
        Some(LandingResult::NotRequested)
    );
    block_on(repo.abandon_now(&copy.change_id, start())).unwrap();
    assert_eq!(
        repo.landing_result(&copy.change_id).unwrap(),
        Some(LandingResult::Abandoned)
    );
    assert!(repo.landing_result("nope").is_err());
}

#[test]
fn changes_filter_by_author_and_state() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    create(&repo, "amber-otter-0001", start());
    block_on(repo.create_working_copy(
        "calm-heron-0002",
        "work/",
        &Author::healer("heal"),
        start(),
    ))
    .unwrap();
    let healer = ChangeFilter {
        author_kind: Some(AuthorKind::Healer),
        ..ChangeFilter::default()
    };
    let rows = repo.changes(&healer).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].author.name, "heal");
    let named = ChangeFilter {
        author_name: Some("tester".to_string()),
        ..ChangeFilter::default()
    };
    assert_eq!(repo.changes(&named).unwrap().len(), 1);
    let landed = ChangeFilter {
        state: Some(ChangeState::Landed),
        ..ChangeFilter::default()
    };
    assert!(repo.changes(&landed).unwrap().is_empty());
}

#[test]
fn a_change_idle_for_the_stale_days_shows_stale_with_an_injected_clock() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let copy = create(&repo, "amber-otter-0001", start());
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert!(!row.is_stale(start() + Duration::days(13), 14));
    assert!(row.is_stale(start() + Duration::days(14), 14));
    assert!(row.is_stale(start() + Duration::days(15), 14));
    let mut landed = row;
    landed.state = ChangeState::Landed;
    assert!(!landed.is_stale(start() + Duration::days(40), 14));
}

#[test]
fn protecting_master_makes_files_read_only_and_leaves_git_alone() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.protect_master_files().unwrap();
    let task = repo.location().repository.join("definitions/task.toml");
    assert!(std::fs::metadata(&task).unwrap().permissions().readonly());
    assert!(std::fs::write(&task, "changed").is_err() || running_as_root());
    // A working copy made afterwards is writable.
    let copy = create(&repo, "amber-otter-0001", start());
    std::fs::write(copy.path.join("definitions/task.toml"), "changed").unwrap();
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn generated_names_are_adjective_noun_and_four_hex_digits() {
    for entropy in [0, 1, 0xabcd_1234, u64::MAX] {
        let name = generated_name(entropy);
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 3, "{name}");
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert_ne!(generated_name(1), generated_name(2));
}
