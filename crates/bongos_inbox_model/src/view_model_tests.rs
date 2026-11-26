use super::*;
use crate::format::{Item, Unreadable};
use crate::maildir::Dir;
use chrono::{Duration, TimeZone as _};
use std::sync::Arc;
use std::time::SystemTime;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn known(action: &str) -> bool {
    matches!(action, "accept" | "reject" | "open" | "dismiss")
}

fn item(kind: &str, source: &str, group: Option<&str>, minutes_ago: i64, actions: &[&str]) -> Item {
    Item {
        version: 1,
        kind: kind.into(),
        created: now() - Duration::minutes(minutes_ago),
        source: source.into(),
        group: group.map(String::from),
        target: None,
        summary: format!("{kind} {minutes_ago}"),
        actions: actions.iter().map(|a| a.to_string()).collect(),
        extra: Vec::new(),
        body: String::new(),
        body_loaded: true,
    }
}

fn entry(dir: Dir, unique: &str, flags: &str, inode: u64) -> Entry {
    let name = if dir == Dir::New {
        unique.to_string()
    } else {
        format!("{unique}:2,{flags}")
    };
    Entry::for_test(dir, &name, inode, SystemTime::UNIX_EPOCH)
}

fn pair(unique: &str, dir: Dir, flags: &str, item: Item) -> (Entry, ParseResult) {
    (entry(dir, unique, flags, 1), Arc::new(Ok(item)))
}

fn rows_of(snapshot: &Snapshot, filter: &Filter) -> Vec<Row> {
    rows(snapshot, filter, &known)
}

#[test]
fn thirty_grouped_items_make_one_group_row() {
    let snapshot: Vec<_> = (0..30)
        .map(|n| {
            let flags = if n < 10 { "S" } else { "" };
            let dir = if n < 5 { Dir::Cur } else { Dir::New };
            let flags = if dir == Dir::New { "" } else { flags };
            pair(
                &format!("u{n:02}"),
                dir,
                flags,
                item("decision", "agent/a", Some("batch"), n, &["accept"]),
            )
        })
        .collect();
    let rows = rows_of(&snapshot, &Filter::All);
    assert_eq!(rows.len(), 1);
    let Row::Group(group) = &rows[0] else {
        panic!("expected group")
    };
    assert_eq!(group.count, 30);
    assert_eq!(group.unread, 25);
    assert_eq!(group.members.len(), 30);
    assert_eq!(group.summary, "decision 0");
    assert_eq!(group.label(), "agent/a: 30 items (25 unread)");
    assert_eq!(unread_count(&snapshot), 25);
}

#[test]
fn single_member_is_a_plain_row() {
    let snapshot = vec![pair(
        "a",
        Dir::New,
        "",
        item("note", "s", Some("g"), 1, &[]),
    )];
    assert!(matches!(rows_of(&snapshot, &Filter::All)[0], Row::Item(_)));
}

#[test]
fn newest_first_with_groups_at_newest_member() {
    let snapshot = vec![
        pair("a", Dir::New, "", item("note", "s1", None, 50, &[])),
        pair("b", Dir::New, "", item("note", "s2", Some("g"), 10, &[])),
        pair("c", Dir::New, "", item("note", "s3", None, 20, &[])),
        pair("d", Dir::New, "", item("note", "s2", Some("g"), 90, &[])),
    ];
    let rows = rows_of(&snapshot, &Filter::All);
    let order: Vec<_> = rows
        .iter()
        .map(|row| match row {
            Row::Item(i) => i.unique.clone(),
            Row::Group(g) => g.key.clone(),
        })
        .collect();
    assert_eq!(order, ["g", "c", "a"]);
    assert!(rows.windows(2).all(|w| w[0].created() >= w[1].created()));
}

#[test]
fn ties_break_by_unique() {
    let snapshot = vec![
        pair("b", Dir::New, "", item("note", "s1", None, 5, &[])),
        pair("a", Dir::New, "", item("note", "s2", None, 5, &[])),
    ];
    let rows = rows_of(&snapshot, &Filter::All);
    let Row::Item(first) = &rows[0] else { panic!() };
    assert_eq!(first.unique, "a");
}

#[test]
fn unknown_kind_and_action_resolve_to_open_dismiss() {
    let snapshot = vec![pair(
        "a",
        Dir::New,
        "",
        item("mystery", "s", None, 1, &["frobnicate"]),
    )];
    let Row::Item(row) = &rows_of(&snapshot, &Filter::All)[0] else {
        panic!()
    };
    assert_eq!(row.actions, ["open", "dismiss"]);
    assert_eq!(resolve_actions(&[], &known), ["open", "dismiss"]);
    let listed = ["reject".to_string(), "nope".to_string(), "open".to_string()];
    assert_eq!(
        resolve_actions(&listed, &known),
        ["reject", "open", "dismiss"]
    );
    assert_eq!(primary_action(&listed, &known), Some("reject".to_string()));
    assert_eq!(primary_action(&["nope".to_string()], &known), None);
}

#[test]
fn unreadable_rows_sort_in_and_never_hide_others() {
    let bad: ParseResult = Arc::new(Err(Unreadable {
        reason: "bad".into(),
        line: Some(2),
    }));
    let mut bad_entry = entry(Dir::Cur, "bad1", "", 7);
    bad_entry.mtime = SystemTime::from(now() - Duration::minutes(15));
    let snapshot = vec![
        pair("a", Dir::New, "", item("note", "s", Some("g"), 10, &[])),
        pair("b", Dir::New, "", item("note", "s", Some("g"), 30, &[])),
        (bad_entry, bad.clone()),
        (entry(Dir::Cur, "bad2", "S", 8), bad),
    ];
    let rows = rows_of(&snapshot, &Filter::All);
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0], Row::Group(_)));
    let Row::Item(unreadable) = &rows[1] else {
        panic!()
    };
    assert_eq!(unreadable.kind, "unreadable");
    assert_eq!(unreadable.summary, "bad1:2,");
    assert_eq!(unreadable.actions, ["open", "dismiss"]);
    assert_eq!(unreadable.created, now() - Duration::minutes(15));
    assert_eq!(unread_count(&snapshot), 3);
}

#[test]
fn filter_by_kind_and_source() {
    let snapshot = vec![
        pair("a", Dir::New, "", item("decision", "s1", None, 1, &[])),
        pair("b", Dir::New, "", item("note", "s1", None, 2, &[])),
        pair("c", Dir::New, "", item("note", "s2", None, 3, &[])),
    ];
    assert_eq!(rows_of(&snapshot, &Filter::Kind("note".into())).len(), 2);
    // Both s1 items share a group key, so the source filter yields one group.
    let by_source = rows_of(&snapshot, &Filter::Source("s1".into()));
    assert_eq!(by_source.len(), 1);
    assert!(matches!(&by_source[0], Row::Group(g) if g.count == 2));
    assert_eq!(rows_of(&snapshot, &Filter::Source("zz".into())).len(), 0);
    let labels: Vec<_> = filter_candidates(&snapshot)
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(
        labels,
        [
            "all",
            "kind: decision",
            "kind: note",
            "source: s1",
            "source: s2"
        ]
    );
}

#[test]
fn age_text_units() {
    let ago = |d: Duration| age_text(now() - d, now());
    assert_eq!(ago(Duration::seconds(59)), "now");
    assert_eq!(ago(Duration::minutes(3)), "3m");
    assert_eq!(ago(Duration::minutes(119)), "1h");
    assert_eq!(ago(Duration::hours(2)), "2h");
    assert_eq!(ago(Duration::days(4)), "4d");
    assert_eq!(ago(Duration::minutes(-5)), "now");
}

#[test]
fn unread_count_counts_new_and_cur_without_seen() {
    let snapshot = vec![
        pair("a", Dir::New, "", item("note", "s", None, 1, &[])),
        pair("b", Dir::Cur, "", item("note", "s", None, 2, &[])),
        pair("c", Dir::Cur, "S", item("note", "s", None, 3, &[])),
        pair("d", Dir::Cur, "FS", item("note", "s", None, 4, &[])),
    ];
    assert_eq!(unread_count(&snapshot), 2);
}
