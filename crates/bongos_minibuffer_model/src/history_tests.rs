use crate::{HISTORY_CAPACITY, HistoryRing};

#[test]
fn newest_entry_is_first() {
    let mut ring = HistoryRing::new();
    ring.push("one");
    ring.push("two");
    ring.push("three");
    assert_eq!(ring.entries(), ["three", "two", "one"]);
    assert_eq!(ring.get(0), Some("three"));
    assert_eq!(ring.get(2), Some("one"));
    assert_eq!(ring.get(3), None);
    assert_eq!(ring.len(), 3);
}

#[test]
fn a_repeat_moves_to_the_front() {
    let mut ring = HistoryRing::new();
    ring.push("one");
    ring.push("two");
    ring.push("three");
    ring.push("one");
    assert_eq!(ring.entries(), ["one", "three", "two"]);
    ring.push("one");
    assert_eq!(ring.entries(), ["one", "three", "two"]);
}

#[test]
fn empty_entries_are_ignored() {
    let mut ring = HistoryRing::new();
    ring.push("");
    assert!(ring.is_empty());
}

#[test]
fn the_ring_is_capped_at_100_entries() {
    assert_eq!(HISTORY_CAPACITY, 100);
    let mut ring = HistoryRing::new();
    for n in 0..250 {
        ring.push(format!("entry {n}"));
    }
    assert_eq!(ring.len(), 100);
    assert_eq!(ring.get(0), Some("entry 249"));
    assert_eq!(ring.get(99), Some("entry 150"));
    // A repeat at the limit does not drop anything.
    ring.push("entry 150");
    assert_eq!(ring.len(), 100);
    assert_eq!(ring.get(0), Some("entry 150"));
    assert_eq!(ring.get(99), Some("entry 151"));
}

#[test]
fn json_round_trips_newest_first() {
    let mut ring = HistoryRing::new();
    ring.push("old");
    ring.push("new \"quoted\"");
    let json = ring.to_json();
    assert_eq!(json, r#"["new \"quoted\"","old"]"#);
    assert_eq!(HistoryRing::from_json(&json), ring);
    assert_eq!(serde_json::to_string(&ring).unwrap(), json);
}

#[test]
fn stored_json_is_normalized_and_bad_json_is_empty() {
    let mut stored = vec![
        "a".to_string(),
        "b".to_string(),
        "a".to_string(),
        String::new(),
    ];
    stored.extend((0..200).map(|n| format!("e{n}")));
    let json = serde_json::to_string(&stored).unwrap();
    let ring = HistoryRing::from_json(&json);
    assert_eq!(ring.len(), 100);
    assert_eq!(&ring.entries()[..3], ["a", "b", "e0"]);

    assert!(HistoryRing::from_json("").is_empty());
    assert!(HistoryRing::from_json("not json").is_empty());
    assert!(HistoryRing::from_json(r#"{"a": 1}"#).is_empty());
    assert!(HistoryRing::from_json("[1, 2]").is_empty());
}
