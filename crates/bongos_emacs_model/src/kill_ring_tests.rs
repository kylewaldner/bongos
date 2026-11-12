use std::path::PathBuf;

use crate::kill_ring::*;

fn plain(text: &str) -> Entry {
    Entry::plain(text.to_string())
}

fn whole(text: &str) -> Entry {
    Entry::from_pieces(
        &[text.to_string()],
        vec![Piece {
            len: 0,
            is_entire_line: true,
            first_line_indent: 0,
            origin: None,
        }],
    )
}

fn multi(texts: &[&str]) -> Entry {
    let texts: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
    let pieces = texts
        .iter()
        .map(|_| Piece {
            len: 0,
            is_entire_line: false,
            first_line_indent: 0,
            origin: None,
        })
        .collect();
    Entry::from_pieces(&texts, pieces)
}

fn ring_of(texts: &[&str]) -> Ring {
    let mut ring = Ring::default();
    for text in texts {
        ring.push(plain(text));
    }
    ring
}

fn texts(ring: &Ring) -> Vec<String> {
    (0..ring.len())
        .map(|ix| ring.get(ix).map(|e| e.text.clone()).unwrap_or_default())
        .collect()
}

#[test]
fn push_puts_the_newest_first() {
    let ring = ring_of(&["a", "b", "c"]);
    assert_eq!(texts(&ring), ["c", "b", "a"]);
    assert_eq!(ring.newest().map(|e| e.text.as_str()), Some("c"));
}

#[test]
fn the_sixty_first_push_drops_the_oldest() {
    let mut ring = Ring::default();
    for n in 0..=KILL_RING_MAX {
        ring.push(plain(&n.to_string()));
    }
    assert_eq!(ring.len(), KILL_RING_MAX);
    assert_eq!(ring.get(0).map(|e| e.text.as_str()), Some("60"));
    assert_eq!(ring.get(59).map(|e| e.text.as_str()), Some("1"));
}

#[test]
fn join_on_an_empty_ring_pushes() {
    let mut ring = Ring::default();
    ring.join_newest(plain("a"), JoinSide::Append);
    assert_eq!(texts(&ring), ["a"]);
}

#[test]
fn append_and_prepend_join_plain_entries() {
    let mut ring = ring_of(&["old", "bar"]);
    ring.join_newest(plain("baz"), JoinSide::Append);
    assert_eq!(texts(&ring), ["barbaz", "old"]);
    ring.join_newest(plain("foo"), JoinSide::Prepend);
    assert_eq!(texts(&ring), ["foobarbaz", "old"]);
    let newest = ring.newest().unwrap();
    assert_eq!(newest.pieces.len(), 1);
    assert_eq!(newest.pieces[0].len, 9);
    assert!(!newest.pieces[0].is_entire_line);
}

#[test]
fn joining_whole_lines_stays_a_whole_line() {
    let mut ring = Ring::default();
    ring.push(whole("a\n"));
    ring.join_newest(whole("b\n"), JoinSide::Append);
    let newest = ring.newest().unwrap();
    assert_eq!(newest.text, "a\nb\n");
    assert!(newest.pieces[0].is_entire_line);
    ring.join_newest(plain("c"), JoinSide::Append);
    let newest = ring.newest().unwrap();
    assert_eq!(newest.text, "a\nb\nc");
    assert!(!newest.pieces[0].is_entire_line);
}

#[test]
fn the_earlier_part_gives_the_indent_and_the_origin_is_dropped() {
    let mut first = plain("a");
    first.pieces[0].first_line_indent = 4;
    first.pieces[0].origin = Some(PieceOrigin {
        path: PathBuf::from("x.rs"),
        lines: 1..=2,
    });
    let mut second = plain("b");
    second.pieces[0].first_line_indent = 8;
    let mut ring = Ring::default();
    ring.push(first.clone());
    ring.join_newest(second.clone(), JoinSide::Append);
    assert_eq!(ring.newest().unwrap().pieces[0].first_line_indent, 4);
    assert!(ring.newest().unwrap().pieces[0].origin.is_none());
    let mut ring = Ring::default();
    ring.push(first);
    ring.join_newest(second, JoinSide::Prepend);
    assert_eq!(ring.newest().unwrap().pieces[0].first_line_indent, 8);
}

#[test]
fn three_piece_entries_join_piecewise() {
    let mut ring = Ring::default();
    ring.push(multi(&["a", "b", "c"]));
    ring.join_newest(multi(&["1", "2", "3"]), JoinSide::Append);
    let newest = ring.newest().unwrap();
    assert_eq!(newest.text, "a1\nb2\nc3");
    assert_eq!(newest.piece_texts(), ["a1", "b2", "c3"]);
    ring.join_newest(multi(&["x", "y", "z"]), JoinSide::Prepend);
    assert_eq!(ring.newest().unwrap().piece_texts(), ["xa1", "yb2", "zc3"]);
}

#[test]
fn mismatched_piece_counts_join_into_one_piece() {
    let mut ring = Ring::default();
    ring.push(multi(&["a", "b", "c"]));
    ring.join_newest(multi(&["1", "2"]), JoinSide::Append);
    let newest = ring.newest().unwrap();
    assert_eq!(newest.text, "a\nb\nc1\n2");
    assert_eq!(newest.pieces.len(), 1);
    assert_eq!(newest.pieces[0].len, newest.text.len());
    let mut ring = Ring::default();
    ring.push(plain("a"));
    ring.join_newest(plain("b"), JoinSide::Append);
    assert_eq!(ring.newest().unwrap().pieces.len(), 1);
}

#[test]
fn secret_propagates_through_a_join() {
    let mut secret = plain("pw");
    secret.secret = true;
    let mut ring = Ring::default();
    ring.push(secret.clone());
    ring.join_newest(plain("x"), JoinSide::Append);
    assert!(ring.newest().unwrap().secret);
    let mut ring = Ring::default();
    ring.push(plain("x"));
    ring.join_newest(secret, JoinSide::Prepend);
    assert!(ring.newest().unwrap().secret);
    let mut ring = Ring::default();
    ring.push(plain("x"));
    ring.join_newest(plain("y"), JoinSide::Append);
    assert!(!ring.newest().unwrap().secret);
}

#[test]
fn from_pieces_round_trips_through_piece_texts() {
    let entry = multi(&["one", "two\nlines", "three"]);
    assert_eq!(entry.text, "one\ntwo\nlines\nthree");
    assert_eq!(
        entry.pieces.iter().map(|p| p.len).collect::<Vec<_>>(),
        [3, 9, 5]
    );
    assert_eq!(entry.piece_texts(), ["one", "two\nlines", "three"]);
    let lines = Entry::from_pieces(
        &["a\n".to_string(), "b".to_string()],
        vec![
            Piece {
                len: 0,
                is_entire_line: true,
                first_line_indent: 0,
                origin: None,
            },
            Piece {
                len: 0,
                is_entire_line: false,
                first_line_indent: 0,
                origin: None,
            },
        ],
    );
    assert_eq!(lines.text, "a\nb");
    assert_eq!(lines.piece_texts(), ["a\n", "b"]);
}

#[test]
fn from_pieces_with_mismatched_counts_is_one_plain_piece() {
    let entry = Entry::from_pieces(&["a".to_string(), "b".to_string()], Vec::new());
    assert_eq!(entry.text, "a\nb");
    assert_eq!(entry.pieces.len(), 1);
}

#[test]
fn yank_index_counts_from_the_newest() {
    let ring = ring_of(&["a", "b", "c"]);
    assert_eq!(ring.yank_index(None), Some(0));
    assert_eq!(ring.yank_index(Some(1)), Some(0));
    assert_eq!(ring.yank_index(Some(2)), Some(1));
    assert_eq!(ring.yank_index(Some(3)), Some(2));
    assert_eq!(ring.yank_index(Some(0)), Some(2));
    assert_eq!(ring.yank_index(Some(-1)), Some(1));
    assert_eq!(ring.yank_index(Some(-2)), Some(0));
    assert_eq!(ring.yank_index(Some(4)), Some(0));
    assert_eq!(Ring::default().yank_index(None), None);
    assert_eq!(Ring::default().yank_index(Some(3)), None);
}

#[test]
fn pop_index_moves_older_and_wraps() {
    let ring = ring_of(&["a", "b", "c"]);
    assert_eq!(ring.pop_index(0, None), Some(1));
    assert_eq!(ring.pop_index(2, None), Some(0));
    assert_eq!(ring.pop_index(0, Some(2)), Some(2));
    assert_eq!(ring.pop_index(1, Some(-1)), Some(0));
    assert_eq!(ring.pop_index(0, Some(-1)), Some(2));
    assert_eq!(ring.pop_index(0, Some(0)), Some(0));
    assert_eq!(Ring::default().pop_index(0, None), None);
}

#[test]
fn picker_view_lists_newest_first_and_hides_older_duplicates() {
    let ring = ring_of(&["a", "b", "a", "c"]);
    let rows = ring.picker_view("");
    assert_eq!(
        rows.iter()
            .map(|r| (r.index, r.label.as_str()))
            .collect::<Vec<_>>(),
        [(0, "c"), (1, "a"), (2, "b")]
    );
}

#[test]
fn picker_view_filters_by_case_insensitive_substring() {
    let ring = ring_of(&["Hello World", "other", "say HELLO"]);
    let rows = ring.picker_view("hello");
    assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 2]);
    assert!(ring.picker_view("zzz").is_empty());
}

#[test]
fn picker_labels_show_newlines_and_cut_long_text() {
    let ring = ring_of(&["a\nb\nc"]);
    assert_eq!(ring.picker_view("")[0].label, "a⏎b⏎c");
    let long = "x".repeat(250);
    let label = ring_of(&[long.as_str()]).picker_view("")[0].label.clone();
    assert_eq!(label.chars().count(), 201);
    assert!(label.ends_with('…'));
    let exact = "y".repeat(200);
    assert_eq!(ring_of(&[exact.as_str()]).picker_view("")[0].label, exact);
}

#[test]
fn picker_preview_is_ten_lines_and_a_marker() {
    let ten: Vec<String> = (1..=10).map(|n| format!("l{n}")).collect();
    let ring = ring_of(&[ten.join("\n").as_str()]);
    assert_eq!(ring.picker_view("")[0].preview, ten.join("\n"));
    let fifteen: Vec<String> = (1..=15).map(|n| format!("l{n}")).collect();
    let preview = ring_of(&[fifteen.join("\n").as_str()]).picker_view("")[0]
        .preview
        .clone();
    assert_eq!(preview, format!("{}\n… 5 more lines", ten.join("\n")));
}

#[test]
fn secret_rows_are_concealed() {
    let mut ring = Ring::default();
    let mut secret = plain("hunter22");
    secret.secret = true;
    ring.push(secret);
    let rows = ring.picker_view("");
    assert_eq!(rows[0].label, "●●●●●●●● (concealed, 8 characters)");
    assert_eq!(rows[0].preview, rows[0].label);
    assert!(rows[0].secret);
    assert!(ring.picker_view("hunter").is_empty());
}

#[test]
fn json_round_trips_text_and_pieces() {
    let mut ring = Ring::default();
    ring.push(multi(&["a", "b\nc"]));
    ring.push(whole("line\n"));
    let back = Ring::from_json(&ring.to_json());
    assert_eq!(texts(&back), texts(&ring));
    assert_eq!(back.get(1).unwrap().piece_texts(), ["a", "b\nc"]);
    assert!(back.get(0).unwrap().pieces[0].is_entire_line);
}

#[test]
fn json_leaves_out_secret_oversized_entries_and_origins() {
    let mut ring = Ring::default();
    let mut origin = plain("kept");
    origin.pieces[0].origin = Some(PieceOrigin {
        path: PathBuf::from("/secret/path.rs"),
        lines: 3..=4,
    });
    ring.push(origin);
    let mut secret = plain("hunter22");
    secret.secret = true;
    ring.push(secret);
    ring.push(plain(&"z".repeat(PERSIST_ENTRY_MAX_BYTES + 1)));
    ring.push(plain(&"z".repeat(PERSIST_ENTRY_MAX_BYTES)));
    let json = ring.to_json();
    assert!(!json.contains("hunter22"));
    assert!(!json.contains("origin"));
    assert!(!json.contains("path.rs"));
    let back = Ring::from_json(&json);
    assert_eq!(back.len(), 2);
    assert_eq!(back.get(1).unwrap().text, "kept");
    assert!(back.get(1).unwrap().pieces[0].origin.is_none());
}

#[test]
fn json_has_the_documented_shape() {
    let json = ring_of(&["a"]).to_json();
    assert_eq!(
        json,
        r#"{"version":1,"entries":[{"text":"a","pieces":[{"len":1,"is_entire_line":false,"first_line_indent":0}]}]}"#
    );
}

#[test]
fn malformed_or_unknown_version_json_is_an_empty_ring() {
    assert!(Ring::from_json("").is_empty());
    assert!(Ring::from_json("not json").is_empty());
    assert!(Ring::from_json("[]").is_empty());
    assert!(Ring::from_json(r#"{"version":1,"entries":[{"pieces":[]}]}"#).is_empty());
    assert!(Ring::from_json(r#"{"version":2,"entries":[{"text":"a","pieces":[]}]}"#).is_empty());
    assert!(Ring::from_json(r#"{"entries":[]}"#).is_empty());
}

#[test]
fn json_lens_that_do_not_fit_are_repaired_to_one_plain_piece() {
    let bad = [
        r#"{"version":1,"entries":[{"text":"abc","pieces":[{"len":99,"is_entire_line":true,"first_line_indent":2}]}]}"#,
        r#"{"version":1,"entries":[{"text":"abc","pieces":[]}]}"#,
        r#"{"version":1,"entries":[{"text":"éa","pieces":[{"len":1,"is_entire_line":false,"first_line_indent":0},{"len":2,"is_entire_line":false,"first_line_indent":0}]}]}"#,
        r#"{"version":1,"entries":[{"text":"abc","pieces":[{"len":1,"is_entire_line":false,"first_line_indent":0}]}]}"#,
    ];
    for json in bad {
        let ring = Ring::from_json(json);
        assert_eq!(ring.len(), 1, "{json}");
        let entry = ring.get(0).unwrap();
        assert_eq!(entry.pieces.len(), 1, "{json}");
        assert_eq!(entry.pieces[0].len, entry.text.len(), "{json}");
        assert!(!entry.pieces[0].is_entire_line, "{json}");
    }
}

#[test]
fn json_caps_the_ring_at_sixty_entries() {
    let entries: Vec<String> = (0..80)
        .map(|n| format!(r#"{{"text":"{n}","pieces":[]}}"#))
        .collect();
    let json = format!(r#"{{"version":1,"entries":[{}]}}"#, entries.join(","));
    let ring = Ring::from_json(&json);
    assert_eq!(ring.len(), KILL_RING_MAX);
    assert_eq!(ring.get(0).unwrap().text, "0");
}
