//! Tests of the live rows of check runs: the throttle per task, the last write winning, the
//! exporter skipping them and the clear when a store is opened. Time is a fixed starting point
//! plus offsets; nothing reads the clock or the calendar.

use std::time::{Duration, Instant};

use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{StateApplyHooks, StateExporter as _, StateStore};
use tempfile::TempDir;

use crate::evidence_store::{LIVE_NAMESPACE, store_record};
use crate::exporter::ChecksExporter;
use crate::live::{Flush, LiveRow, LiveRows, live_rows, part_key};
use crate::test_support::{location, record, writer};

const SECOND: Duration = Duration::from_millis(1000);

fn running(part: u32) -> LiveRow {
    LiveRow::Running {
        part,
        of: 4,
        kept: 0,
        since: format!("2026-03-01T10:00:0{part}Z"),
    }
}

#[test]
fn live_rows_are_written_at_most_once_per_task_per_interval_and_the_last_state_wins() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let mut rows = LiveRows::default();
    let start = Instant::now();

    // The first state of a task is written at once.
    assert_eq!(
        rows.update(&writer, "t1", "tests", running(1), start, SECOND),
        Flush::Now
    );
    rows.flush("t1", start).unwrap();
    // Within the interval the next state waits for the rest of it, and later states join it.
    let soon = start + Duration::from_millis(300);
    assert_eq!(
        rows.update(&writer, "t1", "tests", running(2), soon, SECOND),
        Flush::After(Duration::from_millis(700))
    );
    assert_eq!(
        rows.update(&writer, "t1", "tests", running(3), soon, SECOND),
        Flush::Scheduled
    );
    assert_eq!(
        rows.update(&writer, "t1", "lint", running(1), soon, SECOND),
        Flush::Scheduled
    );
    // The same state again changes nothing.
    assert_eq!(
        rows.update(&writer, "t1", "lint", running(1), soon, SECOND),
        Flush::Unchanged
    );
    // Another task has its own interval.
    assert_eq!(
        rows.update(&writer, "t2", "tests", running(1), soon, SECOND),
        Flush::Now
    );
    rows.flush("t2", soon).unwrap();
    // Still the first state of t1 in the store.
    let stored = writer.read(|read| live_rows(read)).unwrap();
    assert_eq!(stored[&part_key("t1", "tests")], running(1));

    // When the interval has passed, one transaction carries the newest state of each row.
    rows.flush("t1", start + SECOND).unwrap();
    let stored = writer.read(|read| live_rows(read)).unwrap();
    assert_eq!(stored[&part_key("t1", "tests")], running(3));
    assert_eq!(stored[&part_key("t1", "lint")], running(1));
    assert_eq!(stored[&part_key("t2", "tests")], running(1));

    let t1: Vec<Instant> = rows
        .writes
        .iter()
        .filter(|write| write.task_id == "t1")
        .map(|write| write.at)
        .collect();
    assert_eq!(t1, [start, start + SECOND]);
    assert_eq!(rows.writes.len(), 3);
}

#[test]
fn live_rows_are_never_exported() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| store_record(tx, &record("t1", "tree1", "tests", "r1")))
        .unwrap();
    let mut rows = LiveRows::default();
    let now = Instant::now();
    rows.update(&writer, "t1", "tests", running(2), now, SECOND);
    rows.flush("t1", now).unwrap();
    assert_eq!(writer.read(|read| live_rows(read)).unwrap().len(), 1);

    let files = writer.read(|read| ChecksExporter.export(read)).unwrap();

    assert!(!files.is_empty(), "the evidence is exported");
    for (path, text) in &files {
        assert!(!text.contains("running"), "{}: {text}", path.as_unix_str());
        assert!(!text.contains("part/"), "{}: {text}", path.as_unix_str());
    }
}

#[test]
fn the_live_namespace_is_empty_after_reopening_the_store() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let first = std::sync::Arc::new(
        StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap(),
    );
    let mut rows = LiveRows::default();
    let now = Instant::now();
    rows.update(
        &first,
        "t1",
        "tests",
        LiveRow::Ended {
            verdict: Verdict::Pass,
            since: "2026-03-01T10:00:00Z".to_string(),
        },
        now,
        SECOND,
    );
    rows.flush("t1", now).unwrap();
    first
        .transact(|tx| {
            tx.kv(LIVE_NAMESPACE)
                .put("hold/t1", toml::Value::String("checking".to_string()))
        })
        .unwrap();
    drop(rows);
    drop(first);

    // The process that takes the writer next clears the namespace before it writes a row.
    let reopened = std::sync::Arc::new(
        StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap(),
    );
    let stale = reopened
        .read(|read| read.kv(LIVE_NAMESPACE).scan(""))
        .unwrap();
    assert_eq!(stale.len(), 2, "rows from before survive in the file");
    let mut rows = LiveRows::default();
    rows.adopt(&reopened);

    let left = reopened
        .read(|read| read.kv(LIVE_NAMESPACE).scan(""))
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
    // Only once per store: a row written after the first use stays.
    rows.update(&reopened, "t1", "tests", running(1), now, SECOND);
    rows.flush("t1", now).unwrap();
    rows.adopt(&reopened);
    assert_eq!(reopened.read(|read| live_rows(read)).unwrap().len(), 1);
}

#[test]
fn a_live_row_keeps_when_its_state_began() {
    let rows = LiveRows::default();
    let first = running(1);
    let mut later = running(1);
    if let LiveRow::Running { since, .. } = &mut later {
        *since = "2026-03-01T10:09:00Z".to_string();
    }
    assert_eq!(rows.next_row("t1", "tests", later.clone()), later);

    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let mut rows = LiveRows::default();
    rows.update(
        &writer,
        "t1",
        "tests",
        first.clone(),
        Instant::now(),
        SECOND,
    );
    assert_eq!(rows.next_row("t1", "tests", later), first);
    assert_eq!(first.since(), "2026-03-01T10:00:01Z");
}
