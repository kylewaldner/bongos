use bongos_checks_model::evidence::StartedBy;
use bongos_project_repo::{StateExporter, StateRead};
use tempfile::TempDir;
use util::rel_path::{RelPath, RelPathBuf};

use crate::evidence_store::{
    DONE_NAMESPACE, DoneRecord, LAST_NAMESPACE, LIVE_NAMESPACE, OWED_NAMESPACE, SEQ_NAMESPACE,
    done_record, evidence_prefix, next_generation, put_done, scan_records, store_record,
};
use crate::exporter::{ChecksExporter, EXPORT_NAMESPACE};
use crate::test_support::{record, writer};

fn file(name: &str, text: &str) -> (RelPathBuf, String) {
    (
        RelPath::unix(name).unwrap().to_rel_path_buf(),
        text.to_string(),
    )
}

fn table(text: &str) -> toml::Value {
    toml::Value::Table(toml::from_str(text).unwrap())
}

/// Everything the exporter keeps, as rows by namespace and key.
fn rows(read: &StateRead) -> Vec<(String, String, toml::Value)> {
    let mut all = Vec::new();
    for namespace in [
        crate::EVIDENCE_NAMESPACE,
        OWED_NAMESPACE,
        DONE_NAMESPACE,
        LAST_NAMESPACE,
        SEQ_NAMESPACE,
        LIVE_NAMESPACE,
    ] {
        for (key, value) in read.kv(namespace).scan("").unwrap() {
            all.push((namespace.to_string(), key, value));
        }
    }
    all
}

fn names(files: &[(RelPathBuf, String)]) -> Vec<String> {
    files
        .iter()
        .map(|(path, _)| path.as_unix_str().to_string())
        .collect()
}

#[test]
fn the_exporter_owns_the_checks_folder() {
    assert_eq!(ChecksExporter.namespace(), EXPORT_NAMESPACE);
    assert_eq!(EXPORT_NAMESPACE, "checks");
}

#[test]
fn checks_records_round_trip_through_the_exporter() {
    let root = TempDir::new().unwrap();
    let source = writer(&root);
    let mut landing = record("t1", "tree1", "tests", "r6");
    landing.started_by = StartedBy::Landing {
        tree: "tree1".to_string(),
    };
    let mut running = record("t1", "tree1", "tests", "r2");
    running.ended = None;
    let done = DoneRecord {
        tree_key: "tree1".to_string(),
        generation: 2,
        at: "2026-03-01T10:00:05Z".to_string(),
        evidence: vec!["r1".to_string()],
    };
    source
        .transact(|tx| {
            for record in [
                &record("t1", "tree1", "tests", "r1"),
                &running,
                &record("t1", "tree2", "lint", "r3"),
                &record("t2", "tree1", "tests", "r5"),
                &landing,
            ] {
                store_record(tx, record)?;
            }
            put_done(tx, "t1", &done)?;
            tx.kv(OWED_NAMESPACE).put(
                "t1",
                table("closing_from = [\"g1\"]\n[last_child]\nid = \"t9\"\ngeneration = 1\n"),
            )?;
            tx.kv(OWED_NAMESPACE)
                .put("t2", table("closing_from = []\n"))?;
            tx.kv(LAST_NAMESPACE).put(
                "g1",
                table("id = \"t1\"\ngeneration = 1\ntree_key = \"tree1\"\n"),
            )?;
            // Two increments, so the counter is not at its first value.
            next_generation(tx)?;
            next_generation(tx)?;
            // Live rows are never in a snapshot.
            tx.kv(LIVE_NAMESPACE)
                .put("part/t1/tests", table("state = \"running\"\n"))?;
            Ok(())
        })
        .unwrap();

    let exporter = ChecksExporter;
    let files = source.read(|read| exporter.export(read)).unwrap();
    assert_eq!(
        names(&files),
        [
            "checks/g1.toml",
            "checks/landing.toml",
            "checks/t1.toml",
            "checks/t2.toml",
            "checks/seq.toml"
        ]
    );
    let seq = files
        .iter()
        .find(|(path, _)| path.as_unix_str() == "checks/seq.toml")
        .unwrap();
    assert_eq!(seq.1.trim(), "seq = 2");
    let task_one = &files
        .iter()
        .find(|(path, _)| path.as_unix_str() == "checks/t1.toml")
        .unwrap()
        .1;
    assert!(!task_one.contains("part/"), "no live rows: {task_one}");

    // Restore into another store that holds stale rows of its own: they are replaced.
    let other_root = TempDir::new().unwrap();
    let target = writer(&other_root);
    target
        .transact(|tx| {
            store_record(tx, &record("stale", "tree0", "tests", "old"))?;
            tx.kv(DONE_NAMESPACE)
                .put("stale", table("tree_key = \"x\"\n"))?;
            tx.kv(LIVE_NAMESPACE)
                .put("hold/stale", table("reason = \"checking\"\n"))?;
            Ok(())
        })
        .unwrap();
    target.transact(|tx| exporter.import(tx, &files)).unwrap();

    let before: Vec<_> = source
        .read(|read| Ok(rows(read)))
        .unwrap()
        .into_iter()
        .filter(|(namespace, _, _)| namespace != LIVE_NAMESPACE)
        .collect();
    let after = target.read(|read| Ok(rows(read))).unwrap();
    let live: Vec<_> = after
        .iter()
        .filter(|(namespace, _, _)| namespace == LIVE_NAMESPACE)
        .collect();
    assert_eq!(live.len(), 1, "the importer leaves the live rows alone");
    let after: Vec<_> = after
        .into_iter()
        .filter(|(namespace, _, _)| namespace != LIVE_NAMESPACE)
        .collect();
    assert_eq!(after, before);

    // The records are readable as records, and the done record and the counter survived.
    target
        .read(|read| {
            let found = scan_records(read, &evidence_prefix("t1", "tree1", "tests"))?;
            assert_eq!(
                found.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["r1", "r2"]
            );
            assert!(!found[1].is_finished());
            assert_eq!(done_record(read, "t1")?, Some(done.clone()));
            Ok(())
        })
        .unwrap();
    assert_eq!(target.transact(next_generation).unwrap(), 3);
}

#[test]
fn checks_an_empty_store_exports_no_files_and_imports_to_empty() {
    let root = TempDir::new().unwrap();
    let source = writer(&root);
    assert!(
        source
            .read(|read| ChecksExporter.export(read))
            .unwrap()
            .is_empty()
    );

    let other_root = TempDir::new().unwrap();
    let target = writer(&other_root);
    target
        .transact(|tx| store_record(tx, &record("t1", "tree1", "tests", "r1")))
        .unwrap();
    target
        .transact(|tx| ChecksExporter.import(tx, &[]))
        .unwrap();
    assert!(target.read(|read| Ok(rows(read))).unwrap().is_empty());
}

#[test]
fn checks_a_malformed_snapshot_is_refused_and_leaves_the_state_alone() {
    let root = TempDir::new().unwrap();
    let target = writer(&root);
    target
        .transact(|tx| store_record(tx, &record("t1", "tree1", "tests", "r1")))
        .unwrap();
    let before = target.read(|read| Ok(rows(read))).unwrap();
    for bad in [
        file("checks/t1.toml", "not toml ["),
        file("checks/t1.toml", "mystery = 1\n"),
        file("checks/seq.toml", "seq = \"two\"\n"),
        file("checks/seq.toml", "other = 2\n"),
        file("elsewhere/t1.toml", ""),
    ] {
        let result = target.transact(|tx| ChecksExporter.import(tx, std::slice::from_ref(&bad)));
        assert!(result.is_err(), "{} was accepted", bad.0.as_unix_str());
        assert_eq!(target.read(|read| Ok(rows(read))).unwrap(), before);
    }
}
