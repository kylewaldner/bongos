use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work_model::TodoState;
use chrono::{DateTime, Duration, TimeZone as _, Utc};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::backup::{
    BackupOptions, DRIVE_FAILURE_SENTENCE, NOT_BACKED_UP_NO_REMOTE, RUN_RECORDS_LOCAL_SENTENCE,
    SnapshotOutcome, SnapshotReason, copy_run_records, retry_delay, shown_remote,
};
use crate::peer::{ClientKind, PeerIdentity};
use crate::snapshot_tests::{change_state, project, start, state_files, test_options, writer};
use crate::state_branch::{self, snapshot_time};
use crate::{HostBoundary, ProjectRepo, ProjectValueOverrides, RepoCommand, RepoReply};

const STATE_REF: &str = "refs/heads/bongos/state";

fn minutes(count: i64) -> Duration {
    Duration::minutes(count)
}

/// `repo` with `remote` as its backup remote, sharing its state store.
fn with_remote(repo: &ProjectRepo, remote: &str) -> ProjectRepo {
    let mut location = repo.location().clone();
    location.remote = Some(remote.to_string());
    let moved =
        ProjectRepo::new(location, repo.git().clone()).with_state(repo.state().unwrap().clone());
    moved.set_backup_options(repo.backup_options());
    moved.use_exporters(Vec::new());
    moved.use_value_overrides(Arc::new(ProjectValueOverrides::new()));
    moved
}

/// A bare repository at `root/name`, with a `pre-receive` hook of `hook` when given.
fn bare(root: &TempDir, name: &str, hook: Option<&str>) -> PathBuf {
    let path = root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    block_on(crate::test_support::git().run(&path, &["init", "-q", "--bare"])).unwrap();
    if let Some(hook) = hook {
        let file = path.join("hooks").join("pre-receive");
        std::fs::write(&file, format!("#!/bin/sh\n{hook}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    path
}

fn tip(repo: &ProjectRepo) -> Option<String> {
    block_on(state_branch::tip(
        repo.git(),
        &repo.location().repository,
        STATE_REF,
    ))
}

fn remote_tip(repo: &ProjectRepo, remote: &Path) -> Option<String> {
    block_on(repo.git().rev_parse(remote, STATE_REF)).ok()
}

fn last_at(repo: &ProjectRepo) -> Option<DateTime<Utc>> {
    repo.last_snapshot().unwrap().map(|last| last.at)
}

#[test]
fn the_timer_snapshots_every_interval_only_while_the_state_changes() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);

    // Nothing written yet: nothing to snapshot.
    block_on(repo.backup_tick(start()));
    assert_eq!(tip(&repo), None);

    change_state(&repo, TodoState::Done);
    block_on(repo.backup_tick(start()));
    assert_eq!(last_at(&repo), Some(start()));

    // Within the interval: no snapshot, however much changed.
    change_state(&repo, TodoState::Open);
    block_on(repo.backup_tick(start() + minutes(10)));
    assert_eq!(last_at(&repo), Some(start()));

    // The interval passed and the state changed: a snapshot.
    block_on(repo.backup_tick(start() + minutes(15)));
    assert_eq!(last_at(&repo), Some(start() + minutes(15)));
    let taken = tip(&repo);

    // The interval passed again but nothing changed (the backup's own records do not count).
    block_on(repo.backup_tick(start() + minutes(30)));
    block_on(repo.backup_tick(start() + minutes(45)));
    assert_eq!(last_at(&repo), Some(start() + minutes(15)));
    assert_eq!(tip(&repo), taken);
}

#[test]
fn the_interval_comes_from_a_registered_override_first() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let overrides = Arc::new(ProjectValueOverrides::new());
    overrides.register(crate::project_values_tests::fixed(
        "lane",
        crate::SNAPSHOT_INTERVAL_KEY,
        toml::Value::Integer(60),
    ));
    repo.use_value_overrides(overrides);
    assert_eq!(repo.snapshot_interval_minutes(), 60);

    change_state(&repo, TodoState::Done);
    block_on(repo.backup_tick(start()));
    change_state(&repo, TodoState::Open);
    block_on(repo.backup_tick(start() + minutes(30)));
    assert_eq!(last_at(&repo), Some(start()));
    block_on(repo.backup_tick(start() + minutes(60)));
    assert_eq!(last_at(&repo), Some(start() + minutes(60)));
}

#[test]
fn a_job_end_snapshot_keeps_the_minimum_gap_and_backup_now_does_not() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);

    let first = block_on(repo.snapshot_now(SnapshotReason::JobEnd, start())).unwrap();
    assert!(matches!(first, SnapshotOutcome::Taken { .. }), "{first:?}");
    // Unchanged state: nothing to take, even at a job's end.
    assert_eq!(
        block_on(repo.snapshot_now(SnapshotReason::JobEnd, start() + minutes(5))).unwrap(),
        SnapshotOutcome::NothingChanged
    );

    change_state(&repo, TodoState::Open);
    assert_eq!(
        block_on(repo.snapshot_now(SnapshotReason::JobEnd, start() + Duration::seconds(20)))
            .unwrap(),
        SnapshotOutcome::TooSoon { seconds_left: 40 }
    );
    let now = start() + Duration::seconds(21);
    let manual = block_on(repo.backup_now(now)).unwrap();
    assert!(
        matches!(manual, SnapshotOutcome::Taken { .. }),
        "{manual:?}"
    );
    assert_eq!(last_at(&repo), Some(now));
    // A manual snapshot of unchanged state commits nothing new.
    let again = block_on(repo.backup_now(now + Duration::seconds(1))).unwrap();
    assert!(
        matches!(again, SnapshotOutcome::Unchanged { .. }),
        "{again:?}"
    );
}

#[test]
fn the_last_snapshot_is_recorded_in_meta_without_changing_the_generation() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    let generation = writer(&repo).generation().unwrap();

    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    let last = repo.last_snapshot().unwrap().unwrap();
    assert_eq!(last.generation, generation);
    assert_eq!(last.at, start());
    assert_eq!(Some(last.commit.clone()), tip(&repo));
    assert_eq!(writer(&repo).generation().unwrap(), generation);
    let message = block_on(
        repo.git()
            .message_of(&repo.location().repository, &last.commit),
    )
    .unwrap();
    assert_eq!(snapshot_time(&message), Some(start()));
}

#[test]
fn without_a_remote_the_status_says_not_backed_up() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    assert!(!block_on(repo.push_now(start())).unwrap());
    let status = block_on(repo.backup_status(start())).unwrap();
    assert_eq!(status.remote, NOT_BACKED_UP_NO_REMOTE);
    assert_eq!(status.unpushed_snapshots, 1);
    assert_eq!(status.interval_minutes, 15);
    assert_eq!(status.interval_sentence, DRIVE_FAILURE_SENTENCE);
    let text = status.render();
    assert!(text.contains(NOT_BACKED_UP_NO_REMOTE), "{text}");
    assert!(
        text.contains(
            "Snapshot interval: 15 minutes. If this drive fails, you lose up to this much run state and everything not pushed."
        ),
        "{text}"
    );
    assert!(text.contains(RUN_RECORDS_LOCAL_SENTENCE), "{text}");
    assert!(text.contains("Orphan state: none"), "{text}");
}

#[test]
fn a_push_sends_only_the_state_branch() {
    let root = TempDir::new().unwrap();
    let remote = bare(&root, "remote.git", None);
    let repo = with_remote(&project(&root), remote.to_str().unwrap());
    change_state(&repo, TodoState::Done);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    assert!(block_on(repo.push_if_due(start())).unwrap());

    assert_eq!(remote_tip(&repo, &remote), tip(&repo));
    assert!(block_on(repo.git().rev_parse(&remote, "refs/heads/main")).is_err());
    let status = block_on(repo.backup_status(start())).unwrap();
    assert_eq!(status.unpushed_snapshots, 0);
    assert!(status.last_push.unwrap().pushed);
    // Nothing new: nothing to push.
    assert!(!block_on(repo.push_if_due(start() + minutes(1))).unwrap());
}

#[test]
fn a_refusing_remote_keeps_the_snapshot_local_and_retries_with_doubling_pauses() {
    let root = TempDir::new().unwrap();
    for remote in [
        root.path()
            .join("missing.git")
            .to_string_lossy()
            .into_owned(),
        format!("file://{}", root.path().join("gone.git").display()),
        bare(&root, "refusing.git", Some("echo refused >&2; exit 1"))
            .to_string_lossy()
            .into_owned(),
    ] {
        let project_root = TempDir::new().unwrap();
        let repo = with_remote(&project(&project_root), &remote);
        change_state(&repo, TodoState::Done);
        block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
        let local = tip(&repo);

        assert!(!block_on(repo.push_if_due(start())).unwrap(), "{remote}");
        assert_eq!(tip(&repo), local);
        let status = block_on(repo.backup_status(start())).unwrap();
        let push = status.last_push.unwrap();
        assert!(!push.pushed && !push.message.is_empty());
        assert_eq!(status.next_push_at, Some(start() + minutes(5)));
        assert_eq!(status.unpushed_snapshots, 1);

        // During the pause no push is tried.
        assert!(!block_on(repo.push_if_due(start() + minutes(4))).unwrap());
        assert_eq!(
            block_on(repo.backup_status(start()))
                .unwrap()
                .last_push
                .unwrap()
                .at,
            start()
        );

        // Work goes on: the state changes and the next snapshot is taken while pushes fail.
        change_state(&repo, TodoState::Open);
        let next =
            block_on(repo.snapshot_now(SnapshotReason::JobEnd, start() + minutes(5))).unwrap();
        assert!(matches!(next, SnapshotOutcome::Taken { .. }), "{next:?}");

        let mut now = start() + minutes(5);
        let mut pauses = Vec::new();
        for _ in 0..6 {
            block_on(repo.push_if_due(now)).unwrap();
            let next = block_on(repo.backup_status(now))
                .unwrap()
                .next_push_at
                .unwrap();
            pauses.push((next - now).num_minutes());
            now = next;
        }
        assert_eq!(pauses, [10, 20, 40, 60, 60, 60]);
    }
}

#[test]
fn retry_pauses_double_from_the_minimum_to_the_maximum() {
    let pauses: Vec<i64> = (1..=8)
        .map(|failures| retry_delay(failures, 5, 60).num_minutes())
        .collect();
    assert_eq!(pauses, [5, 10, 20, 40, 60, 60, 60, 60]);
    assert_eq!(retry_delay(u32::MAX, 5, 60).num_minutes(), 60);
    assert_eq!(retry_delay(1, 0, 0).num_minutes(), 1);
    assert_eq!(retry_delay(3, 10, 5).num_minutes(), 10);
}

#[test]
fn a_hanging_remote_is_stopped_and_recorded() {
    let root = TempDir::new().unwrap();
    let remote = bare(&root, "hanging.git", Some("sleep 30"));
    let repo = with_remote(&project(&root), remote.to_str().unwrap());
    // The push's time is up at once.
    repo.set_push_timeout(Arc::new(|| Box::pin(futures::future::ready(()))));
    change_state(&repo, TodoState::Done);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    assert!(!block_on(repo.push_now(start())).unwrap());

    let push = block_on(repo.backup_status(start()))
        .unwrap()
        .last_push
        .unwrap();
    assert!(push.message.contains("timed out"), "{}", push.message);
    assert_eq!(remote_tip(&repo, &remote), None);
}

#[test]
fn a_remote_that_reads_as_an_option_is_never_passed_to_git() {
    let root = TempDir::new().unwrap();
    let marker = root.path().join("ran");
    for remote in [
        format!("--upload-pack=touch {}", marker.display()),
        format!("-oProxyCommand=touch {}", marker.display()),
        format!("ext::sh -c touch% {}", marker.display()),
        "origin\nmain".to_string(),
    ] {
        let project_root = TempDir::new().unwrap();
        let repo = with_remote(&project(&project_root), &remote);
        change_state(&repo, TodoState::Done);
        block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
        assert!(!block_on(repo.push_now(start())).unwrap());
        let push = block_on(repo.backup_status(start()))
            .unwrap()
            .last_push
            .unwrap();
        assert!(push.message.contains("not pushing"), "{}", push.message);
    }
    assert!(!marker.exists());
}

#[test]
fn credentials_in_a_remote_url_are_not_shown() {
    assert_eq!(
        shown_remote("https://kyle:tok_abcdefgh@example.com/state.git"),
        "https://<withheld: secret>@example.com/state.git"
    );
    assert_eq!(shown_remote("/backups/state.git"), "/backups/state.git");
    assert_eq!(shown_remote("origin"), "origin");
}

/// Snapshots of the state taken at each of `times`, one change before each.
fn snapshots_at(repo: &ProjectRepo, times: &[DateTime<Utc>]) {
    for (index, time) in times.iter().enumerate() {
        let state = if index % 2 == 0 {
            TodoState::Done
        } else {
            TodoState::Open
        };
        change_state(repo, state);
        writer(repo)
            .transact(|tx| tx.kv("probe").put("n", toml::Value::Integer(index as i64)))
            .unwrap();
        let outcome = block_on(repo.snapshot_now(SnapshotReason::Manual, *time)).unwrap();
        assert!(
            matches!(outcome, SnapshotOutcome::Taken { .. }),
            "{outcome:?}"
        );
    }
}

fn day(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, hour, 0, 0).unwrap()
}

#[test]
fn thinning_keeps_the_last_snapshot_of_each_old_day_with_the_same_trees() {
    let root = TempDir::new().unwrap();
    let remote = bare(&root, "remote.git", None);
    let repo = with_remote(&project(&root), remote.to_str().unwrap());
    let now = day(31, 12) + Duration::days(30);
    let times = [
        day(1, 8),
        day(1, 12),
        day(2, 9),
        day(2, 18),
        day(3, 10),
        now - Duration::days(2),
        now - Duration::hours(2),
        now - Duration::hours(1),
    ];
    snapshots_at(&repo, &times);
    let git = repo.git();
    let repository = &repo.location().repository;
    let before = block_on(state_branch::snapshots(git, repository, STATE_REF)).unwrap();
    assert_eq!(before.len(), 8);
    assert!(block_on(repo.push_now(now)).unwrap());
    let main_before = block_on(git.rev_parse(repository, "main")).unwrap();

    assert!(block_on(repo.thin_snapshots(now)).unwrap());

    let after = block_on(state_branch::snapshots(git, repository, STATE_REF)).unwrap();
    let kept_times: Vec<DateTime<Utc>> = after
        .iter()
        .map(|commit| snapshot_time(&commit.message).unwrap())
        .collect();
    assert_eq!(
        kept_times,
        [
            day(1, 12),
            day(2, 18),
            day(3, 10),
            now - Duration::days(2),
            now - Duration::hours(2),
            now - Duration::hours(1),
        ]
    );
    for kept in &after {
        let original = before
            .iter()
            .find(|commit| commit.message == kept.message)
            .unwrap();
        assert_eq!(kept.tree, original.tree);
    }
    assert_eq!(
        block_on(git.rev_parse(repository, "main")).unwrap(),
        main_before
    );
    // Thinning again changes nothing.
    assert!(!block_on(repo.thin_snapshots(now)).unwrap());

    // The remote still has the old history; the next push replaces it through the lease.
    let old_remote = remote_tip(&repo, &remote).unwrap();
    assert_ne!(Some(old_remote.clone()), tip(&repo));
    assert!(block_on(repo.push_if_due(now)).unwrap());
    assert_eq!(remote_tip(&repo, &remote), tip(&repo));
    assert!(block_on(git.rev_parse(&remote, "refs/heads/main")).is_err());
}

#[test]
fn a_lease_push_after_thinning_refuses_a_remote_that_moved() {
    let root = TempDir::new().unwrap();
    let remote = bare(&root, "remote.git", None);
    let repo = with_remote(&project(&root), remote.to_str().unwrap());
    let now = day(31, 12) + Duration::days(30);
    snapshots_at(&repo, &[day(1, 8), day(1, 12), now - Duration::hours(1)]);
    assert!(block_on(repo.push_now(now)).unwrap());
    assert!(block_on(repo.thin_snapshots(now)).unwrap());
    // Someone else moved the remote's branch meanwhile.
    let git = repo.git();
    let first = block_on(state_branch::snapshots(git, &remote, STATE_REF)).unwrap()[0]
        .commit
        .clone();
    block_on(git.run(&remote, &["update-ref", STATE_REF, &first])).unwrap();

    assert!(!block_on(repo.push_now(now)).unwrap());
    assert_eq!(remote_tip(&repo, &remote), Some(first));
}

#[test]
fn backups_stop_with_the_host() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    repo.stop_backups();

    assert_eq!(
        block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap(),
        SnapshotOutcome::Stopped
    );
    block_on(repo.backup_tick(start()));
    assert_eq!(tip(&repo), None);
}

/// A project whose `files/` holds run records of two tasks, a file outside `runs/`, and a link
/// out of the tree.
fn run_records(repo: &ProjectRepo, outside: &Path) -> PathBuf {
    let files = repo.location().files.clone();
    let runs = files.join("task-a").join("runs");
    std::fs::create_dir_all(runs.join("run-1")).unwrap();
    std::fs::write(runs.join("run-1").join("record.jsonl"), "one\n").unwrap();
    std::fs::write(runs.join("summary.txt"), "sum\n").unwrap();
    std::fs::create_dir_all(files.join("task-b").join("runs")).unwrap();
    std::fs::write(files.join("task-b").join("runs").join("b.jsonl"), "b\n").unwrap();
    std::fs::write(files.join("task-a").join("notes.txt"), "not a run record").unwrap();
    std::fs::write(outside.join("private.txt"), "outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside, runs.join("escape")).unwrap();
    files
}

fn copy_options(dir: &Path) -> BackupOptions {
    BackupOptions {
        run_record_copy_dir: Some(dir.to_path_buf()),
        ..test_options()
    }
}

#[test]
fn run_records_are_copied_when_new_or_changed_and_never_deleted() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let copies = TempDir::new().unwrap();
    let repo = project(&root);
    repo.set_backup_options(copy_options(copies.path()));
    let files = run_records(&repo, outside.path());

    assert!(repo.copy_run_records_if_due(start()).unwrap());
    let copied = copies.path();
    assert_eq!(
        std::fs::read_to_string(copied.join("task-a/runs/run-1/record.jsonl")).unwrap(),
        "one\n"
    );
    assert!(copied.join("task-a/runs/summary.txt").exists());
    assert!(copied.join("task-b/runs/b.jsonl").exists());
    assert!(!copied.join("task-a/notes.txt").exists());
    assert!(!copied.join("task-a/runs/escape").exists());
    let status = block_on(repo.backup_status(start())).unwrap();
    let last = status.run_record_copy.last.unwrap();
    assert_eq!(last.copied, 3);
    assert!(
        last.problems
            .iter()
            .any(|problem| problem.contains("symbolic links"))
    );

    // Within the interval nothing runs.
    std::fs::write(files.join("task-b/runs/new.jsonl"), "new\n").unwrap();
    assert!(!repo.copy_run_records_if_due(start() + minutes(30)).unwrap());
    assert!(!copied.join("task-b/runs/new.jsonl").exists());

    // Then only what is new or changed is copied, and a record gone from the source stays.
    std::fs::write(files.join("task-a/runs/run-1/record.jsonl"), "one\ntwo\n").unwrap();
    std::fs::remove_file(files.join("task-a/runs/summary.txt")).unwrap();
    assert!(repo.copy_run_records_if_due(start() + minutes(60)).unwrap());
    let status = block_on(repo.backup_status(start())).unwrap();
    assert_eq!(status.run_record_copy.last.unwrap().copied, 2);
    assert_eq!(
        std::fs::read_to_string(copied.join("task-a/runs/run-1/record.jsonl")).unwrap(),
        "one\ntwo\n"
    );
    assert!(copied.join("task-b/runs/new.jsonl").exists());
    assert!(copied.join("task-a/runs/summary.txt").exists());
}

#[test]
fn without_a_copy_folder_run_records_stay_on_this_machine() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let repo = project(&root);
    run_records(&repo, outside.path());

    assert!(!repo.copy_run_records_if_due(start()).unwrap());
    let status = block_on(repo.backup_status(start())).unwrap();
    assert_eq!(status.run_record_copy.dir, None);
    assert_eq!(
        status.run_record_copy.sentence.as_deref(),
        Some(RUN_RECORDS_LOCAL_SENTENCE)
    );
    assert_eq!(status.run_record_copy.last, None);
}

#[test]
fn a_copy_folder_that_cannot_hold_the_copy_is_reported() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let repo = project(&root);
    let files = run_records(&repo, outside.path());
    for (destination, expected) in [
        (
            files.join("task-a").join("runs"),
            "inside the project's files",
        ),
        (files.clone(), "inside the project's files"),
        (root.path().join("missing"), "does not exist"),
        (PathBuf::from("relative/copies"), "not an absolute path"),
        (outside.path().join("..").join("x"), "`..`"),
        (outside.path().join("private.txt"), "not a folder"),
    ] {
        let (copied, problems) = copy_run_records(&files, &destination);
        assert_eq!(copied, 0, "{}", destination.display());
        assert!(
            problems.iter().any(|problem| problem.contains(expected)),
            "{}: {problems:?}",
            destination.display()
        );
    }
    // Recorded in the status, and nothing panics.
    repo.set_backup_options(copy_options(&root.path().join("missing")));
    assert!(repo.copy_run_records_if_due(start()).unwrap());
    let last = block_on(repo.backup_status(start()))
        .unwrap()
        .run_record_copy
        .last
        .unwrap();
    assert!(last.problems[0].contains("does not exist"));
}

#[cfg(unix)]
#[test]
fn an_unwritable_copy_folder_is_reported_without_a_panic() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let copies = TempDir::new().unwrap();
    let repo = project(&root);
    let files = run_records(&repo, outside.path());
    std::fs::set_permissions(copies.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let (copied, problems) = copy_run_records(&files, copies.path());

    std::fs::set_permissions(copies.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // A process that may write anywhere copies; any other reports each file it could not.
    if copied == 0 {
        assert!(
            problems.iter().any(|problem| problem.contains("copying")),
            "{problems:?}"
        );
    }
}

fn window() -> PeerIdentity {
    PeerIdentity::for_test(4242, ClientKind::Window, true)
}

fn cli() -> PeerIdentity {
    PeerIdentity::for_test(5151, ClientKind::Cli, false)
}

fn over_the_wire(command: RepoCommand) -> RepoCommand {
    serde_json::from_value(serde_json::to_value(command).unwrap()).unwrap()
}

fn send(
    repo: &ProjectRepo,
    command: serde_json::Value,
    peer: &PeerIdentity,
    now: DateTime<Utc>,
) -> anyhow::Result<RepoReply> {
    let command: RepoCommand = serde_json::from_value(command)?;
    block_on(HostBoundary::new(repo.clone()).accept_now(command, peer, now))
}

fn outcome(reply: RepoReply) -> SnapshotOutcome {
    match reply {
        RepoReply::Snapshot { outcome } => outcome,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[test]
fn snapshot_and_backup_commands_cross_the_host_boundary() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    for command in [
        RepoCommand::SnapshotNow {
            reason: SnapshotReason::JobEnd,
        },
        RepoCommand::BackupNow,
        RepoCommand::BackupStatus,
    ] {
        assert_eq!(over_the_wire(command.clone()), command);
    }

    let taken = outcome(
        send(
            &repo,
            serde_json::json!({"command": "snapshot-now", "reason": "job-end"}),
            &cli(),
            start(),
        )
        .unwrap(),
    );
    assert!(matches!(taken, SnapshotOutcome::Taken { .. }), "{taken:?}");

    // A peer that is not a verified window cannot skip the gap, with either command.
    change_state(&repo, TodoState::Open);
    let soon = start() + Duration::seconds(10);
    for command in [
        serde_json::json!({"command": "snapshot-now", "reason": "manual"}),
        serde_json::json!({"command": "backup-now"}),
    ] {
        assert!(
            matches!(
                outcome(send(&repo, command.clone(), &cli(), soon).unwrap()),
                SnapshotOutcome::TooSoon { .. }
            ),
            "{command}"
        );
    }
    // A verified window's backup now is taken at once.
    let now = outcome(
        send(
            &repo,
            serde_json::json!({"command": "backup-now"}),
            &window(),
            soon,
        )
        .unwrap(),
    );
    assert!(matches!(now, SnapshotOutcome::Taken { .. }), "{now:?}");

    // The status is the host's, and survives the wire whole.
    let reply = send(
        &repo,
        serde_json::json!({"command": "backup-status"}),
        &cli(),
        soon,
    )
    .unwrap();
    let wired: RepoReply = serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
    assert_eq!(wired, reply);
    let RepoReply::BackupStatus { status } = reply else {
        panic!("expected a status");
    };
    assert_eq!(*status, block_on(repo.backup_status(soon)).unwrap());
    assert_eq!(status.last_snapshot.unwrap().at, soon);
}

#[test]
fn malformed_backup_commands_are_refused_before_they_reach_the_repository() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    let huge = "m".repeat(4 * 1024 * 1024);
    for command in [
        serde_json::json!({"command": "snapshot-now"}),
        serde_json::json!({"command": "snapshot-now", "reason": "../../etc/passwd"}),
        serde_json::json!({"command": "snapshot-now", "reason": "Manual"}),
        serde_json::json!({"command": "snapshot-now", "reason": huge}),
        serde_json::json!({"command": "snapshot-now", "reason": 3}),
        serde_json::json!({"command": "snapshot-now", "reason": ["manual"]}),
        serde_json::json!({"command": "snapshot-now", "reason": {"path": "/tmp/x"}}),
        serde_json::json!({"command": "backup-now!"}),
        serde_json::json!({"command": huge}),
        serde_json::json!("backup-now"),
        serde_json::json!(null),
    ] {
        let shown: String = command.to_string().chars().take(80).collect();
        assert!(send(&repo, command, &window(), start()).is_err(), "{shown}");
    }
    assert_eq!(tip(&repo), None);
    // The replies a host sends parse as nothing else.
    for reply in [
        serde_json::json!({"snapshot": {"outcome": {"outcome": "taken"}}}),
        serde_json::json!({"backup-status": {"status": {"remote": 5}}}),
    ] {
        assert!(serde_json::from_value::<RepoReply>(reply).is_err());
    }
}

#[test]
fn the_status_lists_orphan_state_and_unpushed_snapshots() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let main = crate::TreeRef::Commit(crate::CommitId("main".to_string()));
    let definitions = block_on(crate::load_definition_tree(
        repo.git(),
        &repo.location().repository,
        &main,
    ))
    .unwrap();
    writer(&repo).set_definitions(Arc::new(definitions));
    writer(&repo)
        .transact(|tx| {
            tx.set_task(
                "gone-task",
                crate::test_support::task_with_todos(&[("x", TodoState::Done)]),
            )
        })
        .unwrap();
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
    change_state(&repo, TodoState::Open);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start() + minutes(1))).unwrap();

    let status = block_on(repo.backup_status(start())).unwrap();
    assert_eq!(status.orphans, ["gone-task"]);
    assert_eq!(status.unpushed_snapshots, 2);
    assert!(status.render().contains("gone-task"));
    assert!(state_files(&repo).contains_key("tasks/gone-task.toml"));
}
