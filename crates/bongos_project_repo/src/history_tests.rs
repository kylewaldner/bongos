use std::sync::Arc;

use bongos_project_repo_model::Author;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::changes::ChangeState;
use crate::check_fakes::{FakeAmender, replace_in};
use crate::host_boundary::HostBoundary;
use crate::peer::{ClientKind, PeerIdentity};
use crate::sink::{PersonClaim, RepoCommand, RepoReply};
use crate::test_support::{
    add_task, bare_fixture_project, fixture_project, keyboard, land_as, landed, main_commit,
    new_copy, person, refs, request, run_git, start, write_definition,
};
use crate::{
    AuthorFilter, EndedState, HistoryFilter, LandingEvent, MAX_HISTORY_DAYS, ProjectRepo, StateOp,
    WorkingCopy,
};

fn land_at(repo: &ProjectRepo, copy: &WorkingCopy, author: &Author, at: DateTime<Utc>) -> String {
    landed(
        block_on(land_as(
            repo,
            &copy.change_id,
            request("land it", author),
            at,
        ))
        .unwrap(),
    )
}

/// Lands a change by `author` at `at` that adds the task `slug` (id `id`).
fn land_task(
    repo: &ProjectRepo,
    name: &str,
    slug: &str,
    id: &str,
    author: &Author,
    at: DateTime<Utc>,
) -> (WorkingCopy, String) {
    let copy = new_copy(repo, name, author);
    add_task(&copy, slug, id);
    let commit = land_at(repo, &copy, author, at);
    (copy, commit)
}

fn names(entries: &[crate::HistoryEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.name.as_str()).collect()
}

fn read(repo: &ProjectRepo, path: &str) -> String {
    std::fs::read_to_string(repo.location().repository.join(path)).unwrap()
}

#[test]
fn the_raw_diff_is_git_diff_and_the_task_aware_diff_names_tasks_and_moves() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    // `report` moves under `backfill`.
    std::fs::create_dir_all(copy.path.join("definitions/tasks/backfill/tasks")).unwrap();
    std::fs::rename(
        copy.path.join("definitions/tasks/report"),
        copy.path.join("definitions/tasks/backfill/tasks/report"),
    )
    .unwrap();
    land_at(&repo, &copy, &agent, start());

    let raw = block_on(repo.raw_diff(&copy.change_id)).unwrap();
    assert!(raw.starts_with("diff --git "), "{raw}");
    assert!(raw.contains("+++ b/definitions/tasks/alpha/task.toml"));
    assert!(raw.contains("+id = \"a1\""));

    let task = block_on(repo.task_diff(&copy.change_id)).unwrap();
    let lines = task.lines();
    assert!(lines.contains(&"task a1 added".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"task l2 moved: tasks/report -> tasks/backfill/tasks/report".to_string()),
        "{lines:?}"
    );
    assert_eq!(task.tasks(), vec!["a1".to_string(), "l2".to_string()]);
    // The raw diff is not the task-aware one.
    assert!(!raw.contains("task a1 added"));
}

#[test]
fn the_task_aware_diff_of_an_open_change_lists_its_staged_state_edits() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    block_on(
        repo.git()
            .commit_all(&copy.path, "add alpha", &crate::Identity::default()),
    )
    .unwrap();
    let ops = vec![
        StateOp::CreatePrompts {
            task: "a1".parse().unwrap(),
        },
        StateOp::MoveFiles {
            from: "l1/chat.txt".into(),
            to: "a1/chat.txt".into(),
        },
        StateOp::Delete {
            task: "l2".parse().unwrap(),
        },
    ];
    repo.writer_or_error()
        .unwrap()
        .transact(|tx| tx.stage(&copy.change_id, &ops))
        .unwrap();
    let lines = block_on(repo.task_diff(&copy.change_id)).unwrap().lines();
    assert!(lines.contains(&"task a1 added".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"prompts created for task a1".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"moves files l1/chat.txt -> a1/chat.txt".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"removes the state of task l2".to_string()),
        "{lines:?}"
    );
}

/// Four ended changes, newest first: `recent` (agent, a task, 1 day), `dropped` (agent,
/// abandoned, 5 days), `middle` (a person, 10 days) and `old` (agent, 40 days).
fn ended_changes(repo: &ProjectRepo) {
    let agent = Author::agent("chat 1");
    land_task(
        repo,
        "old",
        "alpha",
        "a1",
        &agent,
        start() - Duration::days(40),
    );
    land_task(
        repo,
        "middle",
        "beta",
        "b1",
        &person(),
        start() - Duration::days(10),
    );
    let dropped = new_copy(repo, "dropped", &agent);
    block_on(repo.abandon_now(&dropped.change_id, start() - Duration::days(5))).unwrap();
    land_task(
        repo,
        "recent",
        "gamma",
        "g2",
        &agent,
        start() - Duration::days(1),
    );
}

#[test]
fn history_lists_ended_changes_newest_first_and_filters_by_state_author_date_and_task() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    ended_changes(&repo);
    let history = |filter: HistoryFilter| block_on(repo.change_history(&filter, start())).unwrap();

    let all = history(HistoryFilter::default());
    assert_eq!(names(&all), ["recent", "dropped", "middle", "old"]);
    assert_eq!(all[0].subject, "land it");
    assert_eq!(all[1].subject, "dropped");
    assert_eq!(all[1].state, ChangeState::Abandoned);
    assert!(all[1].commit.is_none() && all[0].commit.is_some());

    let landed_only = HistoryFilter {
        state: EndedState::Landed,
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(landed_only)), ["recent", "middle", "old"]);
    let abandoned = HistoryFilter {
        state: EndedState::Abandoned,
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(abandoned)), ["dropped"]);

    let agents = HistoryFilter {
        author: Some(AuthorFilter::Agent),
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(agents)), ["recent", "dropped", "old"]);
    // A person's changes are read back as claimed persons and still match.
    let people = HistoryFilter {
        author: Some(AuthorFilter::Person),
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(people)), ["middle"]);

    let week = HistoryFilter {
        within_days: Some(7),
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(week)), ["recent", "dropped"]);
    let old = HistoryFilter {
        older_than_days: Some(30),
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(old)), ["old"]);
    let range = HistoryFilter {
        within_days: Some(20),
        older_than_days: Some(7),
        ..HistoryFilter::default()
    };
    assert_eq!(names(&history(range)), ["middle"]);
    // The same history read a month later ages every change.
    let later = block_on(repo.change_history(
        &HistoryFilter {
            within_days: Some(7),
            ..HistoryFilter::default()
        },
        start() + Duration::days(30),
    ))
    .unwrap();
    assert!(later.is_empty());

    let task = HistoryFilter {
        task: Some("b1".to_string()),
        ..HistoryFilter::default()
    };
    let by_task = history(task);
    assert_eq!(names(&by_task), ["middle"]);
    assert_eq!(by_task[0].tasks, vec!["b1".to_string()]);
    let unknown = HistoryFilter {
        task: Some("zz".to_string()),
        ..HistoryFilter::default()
    };
    assert!(history(unknown).is_empty());
}

#[test]
fn reverting_a_landed_change_lands_a_new_change_that_restores_the_files_and_keeps_history() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let agent = Author::agent("chat 1");
    let before = read(&repo, "definitions/tasks/report/task.toml");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    write_definition(
        &copy,
        "tasks/report/task.toml",
        &before.replace("Report", "Reports"),
    );
    let first = land_at(&repo, &copy, &agent, start());
    assert!(read(&repo, "definitions/tasks/report/task.toml").contains("Reports"));

    let author = person();
    let revert = block_on(repo.revert(&copy.change_id, &author, start())).unwrap();
    let row = repo.change(&revert.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Open);
    assert_eq!(row.name, format!("revert-{}", &first[..8]));
    // It is an ordinary change: nothing is in master until it lands.
    assert_eq!(main_commit(&repo), first);
    let meta = repo.revert_meta(&copy.change_id, &author).unwrap();
    assert_eq!(meta.subject, "Revert \"land it\"");
    let second = landed(
        block_on(land_as(
            &repo,
            &revert.change_id,
            crate::LandRequest {
                meta,
                mode: crate::LandMode::Land,
            },
            start(),
        ))
        .unwrap(),
    );

    assert_eq!(main_commit(&repo), second);
    assert_eq!(read(&repo, "definitions/tasks/report/task.toml"), before);
    assert!(
        !repo
            .location()
            .repository
            .join("definitions/tasks/alpha")
            .exists()
    );
    // The history is intact: the first landing is still an ancestor and both are listed.
    run_git(&repo, &["merge-base", "--is-ancestor", &first, "main"]);
    assert_eq!(run_git(&repo, &["rev-parse", &format!("{second}^")]), first);
    let history = block_on(repo.change_history(&HistoryFilter::default(), start())).unwrap();
    assert_eq!(history.len(), 2);
    assert!(
        history
            .iter()
            .any(|entry| entry.commit.as_deref() == Some(&first))
    );
    let revert_entry = history
        .iter()
        .find(|entry| entry.change_id == revert.change_id)
        .unwrap();
    assert_eq!(revert_entry.subject, "Revert \"land it\"");
    // Undoing it again is refused: it is undone.
    let error = block_on(repo.revert(&copy.change_id, &author, start())).unwrap_err();
    assert!(error.to_string().contains("already undone"), "{error:#}");
}

#[test]
fn a_revert_is_refused_for_an_unknown_open_abandoned_or_conflicting_change() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let agent = Author::agent("chat 1");
    let open = new_copy(&repo, "open-one", &agent);
    let dropped = new_copy(&repo, "dropped-one", &agent);
    block_on(repo.abandon_now(&dropped.change_id, start())).unwrap();
    // `first` edits the report; `second` edits it again, so undoing `first` conflicts.
    let first = new_copy(&repo, "first-one", &agent);
    write_definition(
        &first,
        "tasks/report/task.toml",
        &crate::test_support::definition_toml("l2", "First", "other", &[("r1", "write")]),
    );
    land_at(&repo, &first, &agent, start());
    let second = new_copy(&repo, "second-one", &agent);
    write_definition(
        &second,
        "tasks/report/task.toml",
        &crate::test_support::definition_toml("l2", "Second", "other", &[("r1", "write")]),
    );
    land_at(&repo, &second, &agent, start());
    let refs_before = refs(&repo);

    for (id, reason) in [
        ("no-such-change", "there is no change"),
        (open.change_id.as_str(), "is not landed"),
        (dropped.change_id.as_str(), "is not landed"),
        (first.change_id.as_str(), "touch the same files"),
        ("../../etc/passwd", "is not a change id"),
        ("", "is not a change id"),
        ("a b", "is not a change id"),
        ("--output=/tmp/x", "is not a change id"),
    ] {
        let error = block_on(repo.revert(id, &agent, start())).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{id}: {error:#}");
    }
    assert_eq!(refs(&repo), refs_before);
    assert_eq!(
        repo.changes(&crate::ChangeFilter::default()).unwrap().len(),
        1
    );
}

/// A landed change amended by the check `edges`, after a commit whose subject names another check.
fn amended_change(repo: &ProjectRepo) -> (WorkingCopy, crate::AmendCommit, String) {
    repo.add_amending_check(Arc::new(FakeAmender::new(
        "edges",
        1,
        replace_in(
            "tasks/alpha/task.toml",
            "kind = \"other\"\n",
            "kind = \"other\"\nafter = [{ task = \"backfill\", unblock = \"merged\" }]\n",
        ),
    )));
    let agent = Author::agent("chat 1");
    let copy = new_copy(repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    // A person's commit that only looks like an amendment by another check.
    block_on(repo.git().commit_all(
        &copy.path,
        "amend: formatter edit",
        &crate::Identity::default(),
    ))
    .unwrap();
    let decoy = block_on(repo.git().rev_parse(&copy.path, "HEAD")).unwrap();
    land_at(repo, &copy, &agent, start());
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    (copy, row.amend_commits[0].clone(), decoy)
}

#[test]
fn an_amended_change_is_one_row_named_by_the_amend_commit_record_not_by_a_commit_subject() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let (copy, amend, decoy) = amended_change(&repo);

    let listing = block_on(repo.change_listing()).unwrap();
    let listed = listing
        .changes
        .iter()
        .find(|listed| listed.row.change_id == copy.change_id)
        .unwrap();
    let amended = listed.amended.as_ref().unwrap();
    assert_eq!(amended.checks, vec!["edges".to_string()]);
    assert_eq!(amended.files, 1);
    assert_eq!(amended.commit, amend.commit);
    assert!(listed.summary.contains("adds 1 task"), "{}", listed.summary);

    // `d` on that row: the diff of the amend commit alone.
    let diff = block_on(repo.amend_diff(&copy.change_id, &amend.commit)).unwrap();
    assert!(diff.contains("+after = [{ task = \"backfill\""), "{diff}");
    assert!(!diff.contains("+id = \"a1\""), "{diff}");
    // The commit that only names another check in its subject is not an amendment.
    assert_ne!(decoy, amend.commit);
    for commit in [decoy.as_str(), "HEAD", "--output=/tmp/x", ""] {
        assert!(
            block_on(repo.amend_diff(&copy.change_id, commit)).is_err(),
            "{commit}"
        );
    }
}

#[test]
fn a_change_with_no_amend_commits_has_no_amended_by_row() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let (_, _) = land_task(&repo, "plain", "alpha", "a1", &Author::agent("c"), start());
    let listing = block_on(repo.change_listing()).unwrap();
    assert_eq!(listing.changes.len(), 1);
    assert!(listing.changes[0].amended.is_none());
    assert_eq!(listing.changes[0].subject.as_deref(), Some("land it"));
    assert!(listing.split.is_none() && listing.outside.is_none());
}

fn window() -> PeerIdentity {
    PeerIdentity::for_test(4242, ClientKind::Window, true)
}

/// `command` as it crosses the socket.
fn over_the_wire(command: RepoCommand) -> RepoCommand {
    serde_json::from_value(serde_json::to_value(command).unwrap()).unwrap()
}

fn send(
    repo: &ProjectRepo,
    command: serde_json::Value,
    peer: &PeerIdentity,
) -> anyhow::Result<RepoReply> {
    let command: RepoCommand = serde_json::from_value(command)?;
    block_on(HostBoundary::new(repo.clone()).accept_now(command, peer, start()))
}

#[test]
fn a_history_command_from_a_peer_is_checked_before_it_is_used() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    ended_changes(&repo);

    // The wire form of a good request answers like the call.
    let reply = send(
        &repo,
        serde_json::json!({"command": "show-history", "filter": {"state": "landed", "author": "agent"}}),
        &window(),
    )
    .unwrap();
    let RepoReply::History { entries } = reply else {
        panic!("expected a history");
    };
    assert_eq!(names(&entries), ["recent", "old"]);
    let again = over_the_wire(RepoCommand::ShowHistory {
        filter: HistoryFilter::default(),
    });
    assert!(matches!(
        block_on(HostBoundary::new(repo.clone()).accept_now(again, &window(), start())),
        Ok(RepoReply::History { .. })
    ));

    // What serde alone refuses.
    for filter in [
        serde_json::json!({"author": "root"}),
        serde_json::json!({"state": "deleted"}),
        serde_json::json!({"within_days": -1}),
        serde_json::json!({"within_days": "7"}),
        serde_json::json!({"task": 5}),
        serde_json::json!({"since": "2020-01-01"}),
    ] {
        let command = serde_json::json!({"command": "show-history", "filter": filter});
        assert!(send(&repo, command, &window()).is_err(), "{filter}");
    }
    // What the host refuses after serde accepted it.
    let long = "a".repeat(200);
    for filter in [
        serde_json::json!({"task": "../../etc/passwd"}),
        serde_json::json!({"task": "a/b"}),
        serde_json::json!({"task": "--output=x"}),
        serde_json::json!({"task": ""}),
        serde_json::json!({"task": long}),
        serde_json::json!({"within_days": MAX_HISTORY_DAYS + 1}),
        serde_json::json!({"older_than_days": 4_000_000_000_u32}),
        serde_json::json!({"within_days": 3, "older_than_days": 9}),
    ] {
        let command = serde_json::json!({"command": "show-history", "filter": filter});
        assert!(send(&repo, command, &window()).is_err(), "{filter}");
    }
}

/// Sends a revert of `change_id` by `author` with a person's claim from `peer`.
fn sent_revert(
    repo: &ProjectRepo,
    change_id: &str,
    author: &Author,
    peer: &PeerIdentity,
) -> anyhow::Result<String> {
    let command = over_the_wire(RepoCommand::Revert {
        change_id: change_id.to_string(),
        author: author.clone(),
        person: Some(PersonClaim {
            name: keyboard().author.name,
        }),
    });
    match block_on(HostBoundary::new(repo.clone()).accept_now(command, peer, start()))? {
        RepoReply::Started { change_id } => Ok(change_id),
        other => panic!("expected a started revert, got {other:?}"),
    }
}

fn until_landing_event(
    events: &mut UnboundedReceiver<(String, LandingEvent)>,
    change_id: &str,
    mut wanted: impl FnMut(&LandingEvent) -> bool,
) {
    block_on(async {
        loop {
            let (id, event) = events.next().await.expect("the landing events");
            if id == change_id && wanted(&event) {
                return;
            }
        }
    });
}

#[test]
fn a_revert_from_a_peer_names_a_landed_change_and_a_person_claim_approves_only_a_persons_revert() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let person = person();
    let (copy, _) = land_task(&repo, "alpha-one", "alpha", "a1", &person, start());
    let mut events = repo.landing_events();

    // An agent's revert with a verified window's claim approves nothing and waits.
    let agent = Author::agent("chat 1");
    let waiting = sent_revert(&repo, &copy.change_id, &agent, &window()).unwrap();
    assert!(repo.approvals(&waiting).unwrap().is_empty());
    until_landing_event(
        &mut events,
        &waiting,
        |event| matches!(event, LandingEvent::Waiting { reason, .. } if reason.starts_with("waiting for approval")),
    );
    assert!(repo.approvals(&waiting).unwrap().is_empty());
    assert!(
        repo.location()
            .repository
            .join("definitions/tasks/alpha")
            .exists()
    );
    block_on(repo.abandon_now(&waiting, start())).unwrap();

    // A person's revert from an unverified peer waits too.
    let cli = PeerIdentity::for_test(4242, ClientKind::Cli, true);
    let waiting = sent_revert(&repo, &copy.change_id, &person, &cli).unwrap();
    until_landing_event(&mut events, &waiting, |event| {
        matches!(event, LandingEvent::Waiting { .. })
    });
    assert!(repo.approvals(&waiting).unwrap().is_empty());
    block_on(repo.abandon_now(&waiting, start())).unwrap();

    // A person's revert from a verified window lands as the person's own change.
    let reverted = sent_revert(&repo, &copy.change_id, &person, &window()).unwrap();
    until_landing_event(&mut events, &reverted, |event| {
        matches!(event, LandingEvent::Landed { .. })
    });
    assert!(
        !repo
            .location()
            .repository
            .join("definitions/tasks/alpha")
            .exists()
    );
    assert_eq!(
        repo.change(&reverted).unwrap().unwrap().state,
        ChangeState::Landed
    );
}

#[test]
fn a_revert_from_a_peer_with_a_bad_change_id_or_a_change_that_did_not_land_changes_nothing() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let agent = Author::agent("chat 1");
    let open = new_copy(&repo, "open-one", &agent);
    let (landed_copy, _) = land_task(&repo, "alpha-one", "alpha", "a1", &agent, start());
    let before = (refs(&repo), main_commit(&repo));

    for id in [
        "../../etc/passwd",
        "..",
        "",
        "a b",
        "x;rm -rf",
        "--output=/tmp/x",
        "main",
        "HEAD",
        &"a".repeat(65),
        "no-such-change",
        open.change_id.as_str(),
    ] {
        assert!(sent_revert(&repo, id, &agent, &window()).is_err(), "{id}");
    }
    // Wrong shapes never reach the host.
    for command in [
        serde_json::json!({"command": "revert", "change_id": 7, "author": {"kind": "agent"}}),
        serde_json::json!({"command": "revert", "author": {"kind": "agent"}}),
        serde_json::json!({"command": "revert", "change_id": "x", "author": {"kind": "root"}}),
        serde_json::json!({"command": "revert", "change_id": "x", "author": {"kind": "agent"}, "person": {"name": 1}}),
    ] {
        assert!(
            send(&repo, command.clone(), &window()).is_err(),
            "{command}"
        );
    }
    assert_eq!((refs(&repo), main_commit(&repo)), before);
    // The landed change is untouched and still revertible afterwards.
    assert!(block_on(repo.revert(&landed_copy.change_id, &agent, start())).is_ok());
}
