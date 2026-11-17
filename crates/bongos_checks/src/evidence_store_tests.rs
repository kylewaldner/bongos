use bongos_checks_model::evidence::StartedBy;
use bongos_checks_model::verdict::Verdict;
use tempfile::TempDir;

use crate::evidence_store::{
    DoneRecord, delete_done, delete_record, done_record, done_record_in, evidence_key,
    evidence_prefix, landing_key, landing_prefix, next_generation, put_done, record_key,
    scan_records, scan_records_in, store_record, task_prefix,
};
use crate::test_support::{record, writer};

#[test]
fn keys_hold_the_task_tree_check_and_record_id() {
    let task = record("t1", "tree1", "tests", "r1");
    assert_eq!(record_key(&task), "t1/tree1/tests/r1");
    assert_eq!(
        evidence_key("t1", "tree1", "tests", "r1"),
        "t1/tree1/tests/r1"
    );
    assert_eq!(evidence_prefix("t1", "tree1", "tests"), "t1/tree1/tests/");
    assert_eq!(task_prefix("t1"), "t1/");

    let mut landing = record("t1", "tree1", "tests", "r2");
    landing.started_by = StartedBy::Landing {
        tree: "tree9".to_string(),
    };
    assert_eq!(record_key(&landing), "landing/tree9/tests/r2");
    assert_eq!(
        landing_key("tree9", "tests", "r2"),
        "landing/tree9/tests/r2"
    );
    assert_eq!(landing_prefix("tree9", "tests"), "landing/tree9/tests/");
}

#[test]
fn checks_a_record_round_trips_through_a_transaction_and_a_prefix_scan() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let first = record("t1", "tree1", "tests", "r1");
    let mut unfinished = record("t1", "tree1", "tests", "r2");
    unfinished.ended = None;
    unfinished.duration_ms = None;
    unfinished.verdict = Verdict::Fail;
    unfinished.failing = vec!["a_test".to_string()];
    let other_check = record("t1", "tree1", "tests-slow", "r3");
    let other_tree = record("t1", "tree2", "tests", "r4");
    let other_task = record("t2", "tree1", "tests", "r5");
    let mut landing = record("t1", "tree1", "tests", "r6");
    landing.started_by = StartedBy::Landing {
        tree: "tree1".to_string(),
    };

    writer
        .transact(|tx| {
            for record in [
                &first,
                &unfinished,
                &other_check,
                &other_tree,
                &other_task,
                &landing,
            ] {
                store_record(tx, record)?;
            }
            // The same transaction sees what it wrote.
            let seen = scan_records_in(tx, &evidence_prefix("t1", "tree1", "tests"))?;
            assert_eq!(seen, vec![first.clone(), unfinished.clone()]);
            Ok(())
        })
        .unwrap();

    writer
        .read(|read| {
            // One prefix scan is "the records of check C on tree T of task X", ordered by key;
            // a check whose name only starts the same, another tree, another task and a landing
            // run's record are not in it.
            let found = scan_records(read, &evidence_prefix("t1", "tree1", "tests"))?;
            assert_eq!(found, vec![first.clone(), unfinished.clone()]);
            assert!(!found[1].is_finished());
            assert_eq!(found[1].failing, vec!["a_test".to_string()]);

            // Ordered by key: "tests-slow" sorts before "tests/".
            let of_task = scan_records(read, &task_prefix("t1"))?;
            assert_eq!(
                of_task
                    .iter()
                    .map(|record| record.id.as_str())
                    .collect::<Vec<_>>(),
                ["r3", "r1", "r2", "r4"]
            );
            let of_landing = scan_records(read, &landing_prefix("tree1", "tests"))?;
            assert_eq!(of_landing, vec![landing.clone()]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn checks_storing_again_replaces_the_record_and_delete_removes_it() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let mut running = record("t1", "tree1", "tests", "r1");
    running.ended = None;
    writer.transact(|tx| store_record(tx, &running)).unwrap();
    let finished = record("t1", "tree1", "tests", "r1");
    let key = writer.transact(|tx| store_record(tx, &finished)).unwrap();
    let prefix = evidence_prefix("t1", "tree1", "tests");
    assert_eq!(
        writer.read(|read| scan_records(read, &prefix)).unwrap(),
        vec![finished]
    );
    writer.transact(|tx| delete_record(tx, &key)).unwrap();
    assert!(
        writer
            .read(|read| scan_records(read, &prefix))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn checks_a_row_that_is_not_a_record_is_an_error_naming_its_key() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| {
            tx.kv(crate::EVIDENCE_NAMESPACE)
                .put("t1/tree1/tests/bad", toml::Value::Integer(3))
        })
        .unwrap();
    let error = writer
        .read(|read| scan_records(read, "t1/"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("t1/tree1/tests/bad"), "{error}");
}

#[test]
fn checks_done_records_are_put_read_and_deleted() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let done = DoneRecord {
        tree_key: "tree1".to_string(),
        generation: 4,
        at: "2026-03-01T10:00:05Z".to_string(),
        evidence: vec!["r1".to_string(), "r3".to_string()],
    };
    assert_eq!(writer.read(|read| done_record(read, "t1")).unwrap(), None);
    writer
        .transact(|tx| {
            put_done(tx, "t1", &done)?;
            assert_eq!(done_record_in(tx, "t1")?, Some(done.clone()));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        writer.read(|read| done_record(read, "t1")).unwrap(),
        Some(done)
    );
    writer.transact(|tx| delete_done(tx, "t1")).unwrap();
    assert_eq!(writer.read(|read| done_record(read, "t1")).unwrap(), None);
}

#[test]
fn checks_the_generation_counts_up_and_a_rolled_back_transaction_does_not_count() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    assert_eq!(writer.transact(next_generation).unwrap(), 1);
    assert_eq!(writer.transact(next_generation).unwrap(), 2);
    let failed: anyhow::Result<()> = writer.transact(|tx| {
        next_generation(tx)?;
        anyhow::bail!("stop")
    });
    assert!(failed.is_err());
    assert_eq!(writer.transact(next_generation).unwrap(), 3);
}
