use std::ops::Range;

use crate::rectangle::{Columns, Locate, locate};
use crate::rectangle_ops::{RectOp, RectPlan, RowText, insert, plan};

fn rows_of(text: &str) -> Vec<RowText> {
    let mut start = 0;
    text.split('\n')
        .map(|line| {
            let row = RowText {
                start,
                text: line.to_string(),
            };
            start += line.len() + 1;
            row
        })
        .collect()
}

fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut out = text.to_string();
    for (range, replacement) in edits.iter().rev() {
        out.replace_range(range.clone(), replacement);
    }
    out
}

fn cols(left: usize, right: usize) -> Columns {
    Columns { left, right }
}

fn run(op: RectOp, text: &str, left: usize, right: usize, tab: usize) -> (String, RectPlan) {
    let result = plan(op, &rows_of(text), cols(left, right), tab);
    (apply(text, &result.edits), result)
}

fn strings(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|item| item.to_string()).collect())
}

fn disjoint(result: &RectPlan) {
    for pair in result.edits.windows(2) {
        assert!(pair[0].0.end <= pair[1].0.start);
    }
}

#[test]
fn each_op_on_short_inside_and_spanning_rows() {
    let text = "ab\nabcd\nabcdefgh";

    let (after, result) = run(RectOp::Copy, text, 3, 6, 4);
    assert_eq!(after, text);
    assert!(result.edits.is_empty());
    assert_eq!(result.extracted, strings(&["   ", "d  ", "def"]));
    assert_eq!(result.point, 2);

    let (after, result) = run(RectOp::Delete, text, 3, 6, 4);
    assert_eq!(after, "ab\nabc\nabcgh");
    assert_eq!(result.extracted, strings(&["   ", "d  ", "def"]));
    assert_eq!(result.point, 2);
    disjoint(&result);

    let (after, result) = run(RectOp::Open, text, 3, 6, 4);
    assert_eq!(after, "ab    \nabc   d\nabc   defgh");
    assert_eq!(result.extracted, None);
    assert_eq!(result.point, 3);
    disjoint(&result);

    let (after, result) = run(RectOp::Clear, text, 3, 6, 4);
    assert_eq!(after, "ab\nabc \nabc   gh");
    assert_eq!(result.extracted, None);
    assert_eq!(result.point, 2);

    let (after, result) = run(RectOp::String("XY".to_string()), text, 3, 6, 4);
    assert_eq!(after, "ab XY\nabcXY\nabcXYgh");
    assert_eq!(result.extracted, None);
    assert_eq!(result.point, 3);

    let (after, result) = run(RectOp::Keep, text, 3, 6, 4);
    assert_eq!(after, "\nd\ndef");
    assert_eq!(result.point, 0);
}

#[test]
fn edge_inside_a_tab_splits_it_into_spaces() {
    // The tab covers columns 1 to 3.
    let (after, result) = run(RectOp::Delete, "a\tb", 2, 3, 4);
    assert_eq!(after, "a  b");
    assert_eq!(result.extracted, strings(&[" "]));
    // Left edge only.
    let (after, result) = run(RectOp::Delete, "a\tbcd", 2, 5, 4);
    assert_eq!(after, "a cd");
    assert_eq!(result.extracted, strings(&["  b"]));
    // Right edge only.
    let (after, result) = run(RectOp::Delete, "a\tb", 0, 2, 4);
    assert_eq!(after, "  b");
    assert_eq!(result.extracted, strings(&["a "]));
    let (after, _) = run(RectOp::Clear, "a\tb", 2, 3, 4);
    assert_eq!(after, "a   b");
    let (after, result) = run(RectOp::Open, "a\tb", 2, 4, 4);
    assert_eq!(after, "a     b");
    disjoint(&result);
}

#[test]
fn wide_character_is_kept_whole() {
    // The wide character covers columns 1 and 2.
    let (after, result) = run(RectOp::Delete, "a中b", 2, 4, 4);
    assert_eq!(after, "a中");
    assert_eq!(result.extracted, strings(&["b "]));
    let (after, result) = run(RectOp::Delete, "a中b", 0, 2, 4);
    assert_eq!(after, "b");
    assert_eq!(result.extracted, strings(&["a中"]));
    let (after, result) = run(RectOp::Delete, "a中b", 1, 2, 4);
    assert_eq!(after, "ab");
    assert_eq!(result.extracted, strings(&["中"]));
    let (after, _) = run(RectOp::Keep, "a中b", 3, 4, 4);
    assert_eq!(after, "b");
    let (after, _) = run(RectOp::Clear, "a中b", 0, 2, 4);
    assert_eq!(after, "   b");
}

#[test]
fn zero_width_per_op() {
    let text = "ab\nabcd";
    let (_, copy) = run(RectOp::Copy, text, 3, 3, 4);
    assert_eq!(copy.extracted, strings(&["", ""]));
    for op in [RectOp::Delete, RectOp::Open, RectOp::Clear] {
        let (after, result) = run(op, text, 3, 3, 4);
        assert_eq!(after, text);
        assert!(result.edits.is_empty());
    }
    let (after, _) = run(RectOp::String("X".to_string()), text, 3, 3, 4);
    assert_eq!(after, "ab X\nabcXd");
}

#[test]
fn insert_inside_a_tab_and_a_wide_character() {
    let text = "a\tb\nxy";
    let entry = vec!["Q".to_string(), "R".to_string()];
    let result = insert(&rows_of(text), 0, 2, &entry, text.len(), 4);
    assert_eq!(apply(text, &result.edits), "a Q  b\nxyR");
    assert_eq!(result.extracted, None);
    assert_eq!(result.point, 10);

    let text = "a中b";
    let entry = vec!["Q".to_string()];
    let result = insert(&rows_of(text), 0, 2, &entry, text.len(), 4);
    assert_eq!(apply(text, &result.edits), "aQ中b");
    assert_eq!(result.point, 2);
}

#[test]
fn insert_pads_to_the_widest_row_only_where_text_follows() {
    let text = "abcdef\nab";
    let entry = vec!["X".to_string(), "YYY".to_string()];
    let result = insert(&rows_of(text), 0, 2, &entry, text.len(), 4);
    assert_eq!(apply(text, &result.edits), "abX  cdef\nabYYY");
    assert_eq!(result.point, 15);
    disjoint(&result);

    let text = "a";
    let entry = vec!["Z".to_string()];
    let result = insert(&rows_of(text), 0, 3, &entry, text.len(), 4);
    assert_eq!(apply(text, &result.edits), "a  Z");
}

#[test]
fn insert_past_the_last_line_appends_lines() {
    let text = "ab";
    let entry = vec!["X".to_string(), "Y".to_string(), "Z".to_string()];
    let result = insert(&rows_of(text), 0, 1, &entry, text.len(), 4);
    assert_eq!(apply(text, &result.edits), "aXb\n Y\n Z");
    assert_eq!(result.point, 9);
    disjoint(&result);

    let result = insert(&rows_of(text), 1, 2, &entry[..1], text.len(), 4);
    assert_eq!(apply(text, &result.edits), "ab\n  X");
}

#[test]
fn nothing_to_plan() {
    let entry = vec![String::new(), String::new()];
    let text = "abc\ndef";
    let result = insert(&rows_of(text), 0, 1, &entry, text.len(), 4);
    assert!(result.edits.is_empty());
    assert_eq!(result.point, 1);
    let result = insert(&rows_of(text), 0, 1, &[], text.len(), 4);
    assert!(result.edits.is_empty());
    let result = insert(&[], 0, 3, &entry, 0, 4);
    assert!(result.edits.is_empty());
    assert_eq!(result.point, 0);

    for op in [
        RectOp::Copy,
        RectOp::Delete,
        RectOp::Open,
        RectOp::Clear,
        RectOp::String("X".to_string()),
        RectOp::Keep,
    ] {
        let result = plan(op, &[], cols(1, 3), 4);
        assert!(result.edits.is_empty());
    }
    let (after, result) = run(RectOp::Delete, "", 0, 3, 4);
    assert_eq!(after, "");
    assert!(result.edits.is_empty());
    assert_eq!(result.extracted, strings(&["   "]));
}

#[test]
fn delete_then_insert_restores_spanning_rows() {
    let alphabet = ['a', 'b', 'c', ' ', '\t', '中', 'z'];
    let mut state: u64 = 12345;
    let mut next = move |bound: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % bound
    };
    let mut checked = 0;
    for _ in 0..400 {
        let tab = 1 + next(8);
        let left = next(6);
        let right = left + next(6);
        let row_count = 1 + next(4);
        let lines: Vec<String> = (0..row_count)
            .map(|_| {
                (0..next(14))
                    .map(|_| alphabet[next(alphabet.len())])
                    .collect()
            })
            .collect();
        let text = lines.join("\n");
        let rows = rows_of(&text);
        let spans = rows.iter().all(|row| {
            let reaches = matches!(
                locate(&row.text, right, tab),
                Locate::At(_) | Locate::InsideTab { .. } | Locate::InsideWide { .. }
            );
            reaches
                && matches!(locate(&row.text, left, tab), Locate::At(_))
                && matches!(locate(&row.text, right, tab), Locate::At(_))
        });
        if !spans {
            continue;
        }
        checked += 1;
        let deleted = plan(RectOp::Delete, &rows, cols(left, right), tab);
        let shrunk = apply(&text, &deleted.edits);
        let entry = deleted.extracted.clone().unwrap_or_default();
        let restored = insert(&rows_of(&shrunk), 0, left, &entry, shrunk.len(), tab);
        assert_eq!(
            apply(&shrunk, &restored.edits),
            text,
            "text {text:?} cols {left}..{right} tab {tab}"
        );
    }
    assert!(checked > 20, "only {checked} cases were checked");
}
