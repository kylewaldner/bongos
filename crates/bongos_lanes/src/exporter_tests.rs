//! Tests of the `lanes` state exporter: owner and lane records round-trip through a snapshot.

use bongos_lanes_model::LanesState;
use bongos_project_repo::{StateExporter, StateRead};
use tempfile::TempDir;
use util::rel_path::{RelPath, RelPathBuf};

use crate::exporter::{EXPORT_NAMESPACE, LanesExporter, STATE_FILE};
use crate::store::{LANES_NAMESPACE, read_state, write_state};
use crate::test_support::{lanes_state, writer};

fn file(name: &str, text: &str) -> (RelPathBuf, String) {
    (
        RelPath::unix(name).unwrap().to_rel_path_buf(),
        text.to_string(),
    )
}

/// Every row of the lanes namespace, by key.
fn rows(read: &StateRead) -> Vec<(String, toml::Value)> {
    read.kv(LANES_NAMESPACE).scan("").unwrap()
}

#[test]
fn lanes_the_exporter_owns_the_lanes_file() {
    assert_eq!(LanesExporter.namespace(), EXPORT_NAMESPACE);
    assert_eq!(STATE_FILE, "lanes.toml");
}

#[test]
fn lanes_owner_and_lane_records_round_trip_through_the_exporter() {
    let root = TempDir::new().unwrap();
    let source = writer(&root);
    let state = lanes_state();
    source.transact(|tx| write_state(tx, &state)).unwrap();
    let records = source.read(|read| Ok(rows(read))).unwrap();
    // The dispatcher record is never exported: a restore never claims a live host.
    source
        .transact(|tx| {
            tx.kv(LANES_NAMESPACE)
                .put("dispatcher", toml::Value::String("host 1".to_string()))
        })
        .unwrap();

    let files = source.read(|read| LanesExporter.export(read)).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0.as_unix_str(), STATE_FILE);
    assert!(files[0].1.contains("[owners.feature]"), "{}", files[0].1);
    assert!(files[0].1.contains("[lanes.build-1"), "{}", files[0].1);
    assert!(!files[0].1.contains("host 1"), "{}", files[0].1);

    // Restore into another store that holds a stale owner and lane: they are replaced.
    let other_root = TempDir::new().unwrap();
    let target = writer(&other_root);
    let mut stale = LanesState::default();
    let stale_lane = bongos_lanes_model::LaneId::new("old", 3);
    stale.lanes.insert(
        stale_lane.clone(),
        bongos_lanes_model::LaneRecord::new(&stale_lane, bongos_lanes_model::LaneState::Halted),
    );
    target
        .transact(|tx| {
            write_state(tx, &stale)?;
            tx.kv(LANES_NAMESPACE)
                .put("dispatcher", toml::Value::String("a live host".to_string()))
        })
        .unwrap();
    target
        .transact(|tx| LanesExporter.import(tx, &files))
        .unwrap();

    assert_eq!(target.read(read_state).unwrap(), state);
    assert_eq!(
        target.read(|read| Ok(rows(read))).unwrap(),
        records,
        "the owner and lane records come back as they were, and no dispatcher record"
    );
}

#[test]
fn lanes_an_empty_state_exports_no_files_and_imports_to_empty() {
    let root = TempDir::new().unwrap();
    let source = writer(&root);
    assert!(
        source
            .read(|read| LanesExporter.export(read))
            .unwrap()
            .is_empty()
    );
    let other_root = TempDir::new().unwrap();
    let target = writer(&other_root);
    target
        .transact(|tx| write_state(tx, &lanes_state()))
        .unwrap();
    target.transact(|tx| LanesExporter.import(tx, &[])).unwrap();
    assert!(target.read(|read| Ok(rows(read))).unwrap().is_empty());
}

#[test]
fn lanes_a_malformed_snapshot_is_refused_and_leaves_the_state_alone() {
    let root = TempDir::new().unwrap();
    let target = writer(&root);
    target
        .transact(|tx| write_state(tx, &lanes_state()))
        .unwrap();
    let before = target.read(|read| Ok(rows(read))).unwrap();
    for bad in [
        file(STATE_FILE, "not toml ["),
        file(STATE_FILE, "mystery = 1\n"),
        file(STATE_FILE, "owners = 3\n"),
        file(STATE_FILE, "[owners.t1]\nstate = \"flying\"\n"),
        file(
            STATE_FILE,
            "[lanes.build-2]\nkind = \"build\"\nindex = 1\nstate = \"idle\"\n",
        ),
        file(
            STATE_FILE,
            "[lanes.nonsense]\nkind = \"build\"\nindex = 1\nstate = \"idle\"\n",
        ),
        file("elsewhere/state.toml", ""),
    ] {
        let result = target.transact(|tx| LanesExporter.import(tx, std::slice::from_ref(&bad)));
        assert!(
            result.is_err(),
            "{} was accepted: {}",
            bad.0.as_unix_str(),
            bad.1
        );
        assert_eq!(target.read(|read| Ok(rows(read))).unwrap(), before);
    }
}
