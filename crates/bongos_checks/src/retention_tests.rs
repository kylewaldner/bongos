//! The output retention sweep. Every time is an offset from an injected `now`.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bongos_checks_model::evidence::{EvidenceRecord, Output, StartedBy};
use bongos_checks_model::verdict::Verdict;
use chrono::{DateTime, SecondsFormat, Utc};
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::engine::ChecksEngine;
use crate::evidence_store::{DoneRecord, done_record, put_done, scan_records, store_record};
use crate::pools_tests::FakeClock;
use crate::retention::{SWEEP_INTERVAL, sweep_output};
use crate::settings::TaskChecksSettings;
use crate::task_source::TaskSource;
use crate::task_tree::RunWorktrees;
use crate::test_support::{fixture_project, record, writer};

fn settings(output_dir: Option<PathBuf>) -> TaskChecksSettings {
    TaskChecksSettings {
        enabled: true,
        output_dir,
        output_retention_days: 30,
        output_limit_mb: 1,
        result_file_limit_kb: 64,
        default_budget_minutes: None,
        no_verdict_reruns: 2,
        no_verdict_rerun_minutes: 5,
        pool_wait_limit_hours: 4,
        kill_grace_seconds: 2,
        parallel_parts_max: 4,
        scratch_dir: None,
        tree_key_debounce_ms: 2000,
        live_write_ms: 1000,
        confine: false,
    }
}

/// An arbitrary fixed "now"; nothing compares it with the calendar.
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn days_before(time: DateTime<Utc>, days: i64) -> String {
    text(time - chrono::Duration::days(days))
}

/// A finished record of `task` that ended `age_days` before `at`, with an output folder holding a
/// log, under the default output folder `<files>/<task>/checks/<id>`.
fn aged(files: &Path, task: &str, id: &str, at: DateTime<Utc>, age_days: i64) -> EvidenceRecord {
    let folder = files.join(task).join("checks").join(id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("part-1.log"), "output\n").unwrap();
    let mut record = record(task, "tree1", "tests", id);
    record.ended = Some(days_before(at, age_days));
    record.output = Output::Path(folder.to_string_lossy().into_owned());
    record
}

fn store(writer: &bongos_project_repo::StateWriter, records: &[&EvidenceRecord]) {
    writer
        .transact(|tx| {
            for record in records {
                store_record(tx, record)?;
            }
            Ok(())
        })
        .unwrap();
}

fn stored(writer: &bongos_project_repo::StateWriter, id: &str) -> EvidenceRecord {
    writer
        .read(|read| scan_records(read, ""))
        .unwrap()
        .into_iter()
        .find(|record| record.id == id)
        .unwrap()
}

fn folder_of(record: &EvidenceRecord) -> PathBuf {
    match &record.output {
        Output::Path(path) => PathBuf::from(path),
        Output::Deleted => panic!("no folder"),
    }
}

#[test]
fn retention_deletes_old_output_and_marks_the_record_but_keeps_newer_output() {
    let root = TempDir::new().unwrap();
    let files = root.path().join("files");
    let writer = writer(&root);
    let old = aged(&files, "t1", "old", now(), 31);
    let edge = aged(&files, "t1", "edge", now(), 30);
    let new = aged(&files, "t1", "new", now(), 29);
    let mut unfinished = aged(&files, "t1", "running", now(), 90);
    unfinished.ended = None;
    store(&writer, &[&old, &edge, &new, &unfinished]);
    let done = DoneRecord {
        tree_key: "tree1".to_string(),
        generation: 1,
        at: days_before(now(), 31),
        evidence: vec!["old".to_string()],
    };
    writer.transact(|tx| put_done(tx, "t1", &done)).unwrap();

    let marked = sweep_output(&writer, &settings(None), &files, &text(now())).unwrap();
    assert_eq!(marked, 2);

    for record in [&old, &edge] {
        assert!(!folder_of(record).exists());
        let kept = stored(&writer, &record.id);
        assert_eq!(kept.output, Output::Deleted);
        // Only the output field changed.
        let mut expected = record.clone();
        expected.output = Output::Deleted;
        assert_eq!(kept, expected);
        assert_eq!(kept.verdict, Verdict::Pass);
    }
    for record in [&new, &unfinished] {
        assert!(folder_of(record).join("part-1.log").exists());
        assert_eq!(stored(&writer, &record.id), *record);
    }
    assert_eq!(
        writer.read(|read| done_record(read, "t1")).unwrap(),
        Some(done)
    );
}

#[test]
fn retention_a_second_sweep_changes_nothing() {
    let root = TempDir::new().unwrap();
    let files = root.path().join("files");
    let writer = writer(&root);
    let old = aged(&files, "t1", "old", now(), 40);
    store(&writer, &[&old]);
    let at = text(now());
    assert_eq!(
        sweep_output(&writer, &settings(None), &files, &at).unwrap(),
        1
    );
    let before = writer.read(|read| scan_records(read, "")).unwrap();
    assert_eq!(
        sweep_output(&writer, &settings(None), &files, &at).unwrap(),
        0
    );
    assert_eq!(writer.read(|read| scan_records(read, "")).unwrap(), before);
}

#[test]
fn retention_never_deletes_outside_the_output_folder() {
    let root = TempDir::new().unwrap();
    let files = root.path().join("files");
    let writer = writer(&root);
    let precious = root.path().join("precious");
    std::fs::create_dir_all(&precious).unwrap();
    std::fs::write(precious.join("keep.txt"), "keep").unwrap();

    // A record whose output path points elsewhere.
    let mut elsewhere = aged(&files, "t1", "elsewhere", now(), 60);
    elsewhere.output = Output::Path(precious.to_string_lossy().into_owned());
    // A record whose id climbs out of the folder.
    let mut climbing = aged(&files, "t1", "../../../precious", now(), 60);
    climbing.output = Output::Path(
        files
            .join("t1")
            .join("checks")
            .join("../../../precious")
            .to_string_lossy()
            .into_owned(),
    );
    // An output folder that is a link to a folder elsewhere.
    let linked = aged(&files, "t1", "linked", now(), 60);
    std::fs::remove_dir_all(folder_of(&linked)).unwrap();
    std::os::unix::fs::symlink(&precious, folder_of(&linked)).unwrap();
    // A task output folder that is itself a link.
    let mut through = aged(&files, "t2", "x", now(), 60);
    std::fs::remove_dir_all(files.join("t2").join("checks")).unwrap();
    std::os::unix::fs::symlink(&precious, files.join("t2").join("checks")).unwrap();
    through.output = Output::Path(
        files
            .join("t2")
            .join("checks")
            .join("x")
            .to_string_lossy()
            .into_owned(),
    );
    // A sibling file next to the output folders stays too.
    let sibling = files.join("t1").join("notes.txt");
    std::fs::write(&sibling, "notes").unwrap();

    for record in [&elsewhere, &climbing, &linked, &through] {
        store(&writer, &[record]);
    }
    sweep_output(&writer, &settings(None), &files, &text(now())).unwrap();

    assert_eq!(
        std::fs::read_to_string(precious.join("keep.txt")).unwrap(),
        "keep"
    );
    assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "notes");
    // The link itself goes; what it pointed at stays.
    assert!(!folder_of(&linked).exists());
    assert_eq!(stored(&writer, "elsewhere").output, elsewhere.output);
    assert_eq!(stored(&writer, "x").output, through.output);
}

#[test]
fn retention_uses_the_configured_output_dir_and_landing_folder() {
    let root = TempDir::new().unwrap();
    let files = root.path().join("files");
    let configured = root.path().join("configured");
    let writer = writer(&root);
    let settings = settings(Some(configured.clone()));

    let mut task = record("t1", "tree1", "tests", "r1");
    task.ended = Some(days_before(now(), 45));
    let task_folder = configured.join("t1").join("r1");
    task.output = Output::Path(task_folder.to_string_lossy().into_owned());
    let mut landing = record("t1", "tree1", "tests", "r2");
    landing.ended = Some(days_before(now(), 45));
    landing.started_by = StartedBy::Landing {
        tree: "tree9".to_string(),
    };
    let landing_folder = configured.join("landing").join("r2");
    landing.output = Output::Path(landing_folder.to_string_lossy().into_owned());
    for folder in [&task_folder, &landing_folder] {
        std::fs::create_dir_all(folder).unwrap();
        std::fs::write(folder.join("part-1.log"), "x").unwrap();
    }
    store(&writer, &[&task, &landing]);

    assert_eq!(
        sweep_output(&writer, &settings, &files, &text(now())).unwrap(),
        2
    );
    assert!(!task_folder.exists());
    assert!(!landing_folder.exists());
    assert!(configured.join("t1").exists());
}

struct NoTasks;

impl TaskSource for NoTasks {
    fn task(&self, _: &str, _: &gpui::App) -> Option<crate::task_source::TaskChecks> {
        None
    }
}

struct NoRuns;

impl RunWorktrees for NoRuns {
    fn active(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }

    fn last_ended(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }
}

#[gpui::test]
async fn retention_runs_at_start_and_again_every_24_hours_on_the_engine_clock(
    cx: &mut TestAppContext,
) {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let writer = repo.state().unwrap().writer().unwrap().clone();
    let files = repo.location().files.clone();
    let clock = FakeClock::manual();
    let at = Arc::new(Mutex::new(now()));
    let reading = at.clone();
    let engine = cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        TaskChecksSettings::register(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let checks = content.task_checks.get_or_insert_default();
                checks.enabled = Some(true);
                checks.output_retention_days = Some(30);
            });
        });
        ChecksEngine::install(
            ChecksEngine::new(
                Rc::new(NoTasks),
                Rc::new(NoRuns),
                clock.clone(),
                Arc::new(move || text(*reading.lock().unwrap())),
            ),
            cx,
        )
    });
    let old = aged(&files, "t1", "old", now(), 31);
    // One day short of the retention at start; past it a day later.
    let soon = aged(&files, "t1", "soon", now(), 29);
    store(&writer, &[&old, &soon]);

    engine.update(cx, |engine, cx| engine.follow_project(&repo, cx));
    cx.run_until_parked();
    assert_eq!(stored(&writer, "old").output, Output::Deleted);
    assert!(!folder_of(&old).exists());
    assert_eq!(stored(&writer, "soon").output, soon.output);
    assert_eq!(*clock.sleeps().last().unwrap(), SWEEP_INTERVAL);

    // The timer has not fired yet: nothing happens.
    clock.advance(SWEEP_INTERVAL - Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(stored(&writer, "soon").output, soon.output);

    *at.lock().unwrap() += chrono::Duration::days(2);
    clock.advance(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(stored(&writer, "soon").output, Output::Deleted);
    assert!(!folder_of(&soon).exists());
    assert_eq!(clock.sleeps().len(), 2, "the next sweep is scheduled");
}

#[gpui::test]
async fn retention_skips_while_task_checks_are_off(cx: &mut TestAppContext) {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let writer = repo.state().unwrap().writer().unwrap().clone();
    let files = repo.location().files.clone();
    let clock = FakeClock::manual();
    let engine = cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        TaskChecksSettings::register(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let checks = content.task_checks.get_or_insert_default();
                checks.enabled = Some(false);
                checks.output_retention_days = Some(30);
            });
        });
        ChecksEngine::install(
            ChecksEngine::new(
                Rc::new(NoTasks),
                Rc::new(NoRuns),
                clock.clone(),
                Arc::new(|| text(now())),
            ),
            cx,
        )
    });
    let old = aged(&files, "t1", "old", now(), 60);
    store(&writer, &[&old]);
    engine.update(cx, |engine, cx| engine.follow_project(&repo, cx));
    cx.run_until_parked();
    assert!(folder_of(&old).exists());
    assert_eq!(stored(&writer, "old").output, old.output);
}
