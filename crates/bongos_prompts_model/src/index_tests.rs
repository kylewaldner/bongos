use std::ops::Range;

use crate::{MarkerSyntax, PromptIndex, RowEdit, merge_row_edits, split_rows};

const M1: &str = "=== prompt 2026-11-05 13:40 ===";
const M2: &str = "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===";

fn build(text: &str) -> PromptIndex {
    PromptIndex::build(MarkerSyntax::default(), &split_rows(text))
}

fn rows_of(index: &PromptIndex) -> Vec<(u32, bool)> {
    index
        .markers()
        .iter()
        .map(|m| (m.row, m.marker.is_sent()))
        .collect()
}

#[test]
fn build_finds_markers_and_blocks() {
    let text = format!("pre\n{M1}\na\nb\n{M2}\nc");
    let index = build(&text);
    assert_eq!(rows_of(&index), vec![(1, false), (4, true)]);
    assert_eq!(index.line_count(), 6);
    assert_eq!(index.block_count(), 3);
    assert_eq!(index.stats().lines_parsed, 6);
    assert_eq!(index.block_at(0).marker_row, None);
    assert_eq!(index.block_at(3).marker_row, Some(1));
    assert_eq!(index.block_at(99).marker_row, Some(4));
    assert_eq!(index.body_rows(1), Some(2..4));
    assert_eq!(index.position(3), (2, 3));
    assert_eq!(index.position(0), (1, 3));
}

#[test]
fn queries_without_preamble() {
    let index = build(&format!("{M1}\na\n{M2}\nb\n{M1}\n"));
    assert_eq!(index.block_count(), 3);
    assert_eq!(index.block_index_at(0), 0);
    assert_eq!(index.position(3), (2, 3));
    assert_eq!(index.previous(3, 1).unwrap().index, 0);
    assert_eq!(index.previous(3, 9).unwrap().index, 0);
    assert!(index.previous(0, 1).is_none());
    assert_eq!(index.next(0, 1).unwrap().index, 1);
    assert_eq!(index.next(0, 9).unwrap().index, 2);
    assert!(index.next(4, 1).is_none());
    assert!(index.next(0, 0).is_none());
}

#[test]
fn next_pending_skips_sent_and_wraps_once() {
    let index = build(&format!("{M1}\na\n{M2}\nb\n{M2}\nc\n{M1}\nd"));
    let (block, wrapped) = index.next_pending(1).unwrap();
    assert_eq!((block.index, wrapped), (3, false));
    let (block, wrapped) = index.next_pending(7).unwrap();
    assert_eq!((block.index, wrapped), (0, true));
    let only_sent = build(&format!("{M2}\na\n{M2}\nb"));
    assert!(only_sent.next_pending(0).is_none());
    let only_here = build(&format!("{M2}\na\n{M1}\nb"));
    assert!(only_here.next_pending(3).is_none());
}

#[test]
fn edit_insert_delete_join_split() {
    let mut index = build(&format!("a\n{M1}\nb\n{M2}\nc"));

    // insert a marker line before row 2
    let change = index.edit(2..2, &[M1]);
    assert_eq!(change.added.len(), 1);
    assert!(change.removed.is_empty() && change.changed);
    assert_eq!(rows_of(&index), vec![(1, false), (2, false), (4, true)]);
    assert_eq!(index.line_count(), 6);

    // delete that marker row
    let change = index.edit(2..3, &[]);
    assert_eq!(change.removed.len(), 1);
    assert_eq!(rows_of(&index), vec![(1, false), (3, true)]);

    // join: replace rows 2..4 (body, marker) with one body row
    let change = index.edit(2..4, &["bc"]);
    assert_eq!(change.removed.len(), 1);
    assert_eq!(rows_of(&index), vec![(1, false)]);
    assert_eq!(index.line_count(), 4);

    // split: one row becomes a body row and a marker
    index.edit(2..3, &["b", M2]);
    assert_eq!(rows_of(&index), vec![(1, false), (3, true)]);
    assert_eq!(index.line_count(), 5);
}

#[test]
fn unchanged_marker_keeps_its_id() {
    let mut index = build(&format!("{M1}\na"));
    let id = index.markers()[0].id;
    let change = index.edit(0..1, &[M1]);
    assert!(!change.changed);
    assert_eq!(index.markers()[0].id, id);
    let change = index.edit(0..1, &[M2]);
    assert_eq!((change.removed, change.added.len()), (vec![id], 1));
}

#[test]
fn queries_parse_nothing_and_one_line_edit_parses_one() {
    let mut text = String::new();
    for n in 0..5_000 {
        text.push_str(&format!("{M1}\nbody {n}\n"));
    }
    let mut index = build(&text);
    assert_eq!(index.block_count(), 5_000);
    let before = index.stats();
    for row in (0..10_000).step_by(37) {
        index.block_at(row);
        index.position(row);
        index.previous(row, 3);
        index.next(row, 3);
        index.next_pending(row);
        index.body_rows(index.block_index_at(row));
    }
    assert_eq!(index.stats(), before);
    index.edit(5_001..5_002, &["changed body"]);
    assert_eq!(index.stats().lines_parsed, before.lines_parsed + 1);
    assert_eq!(index.stats().rebuilds, before.rebuilds);
    index.rebuild(&split_rows(&text));
    assert_eq!(index.stats().rebuilds, before.rebuilds + 1);
}

struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn random_line(rng: &mut Rng) -> String {
    let pool = [
        "",
        "body",
        "  indented",
        M1,
        M2,
        "===",
        "=== prompt",
        "=== prompt 2026-11-05 13:40 ====",
        "=== prompt 2026-02-30 13:40 ===",
        "=== prompt 2026-11-05 13:41 ===",
    ];
    pool[rng.next(pool.len() as u64) as usize].to_string()
}

fn check_matches_rebuild(index: &PromptIndex, lines: &[String]) {
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let fresh = PromptIndex::build(MarkerSyntax::default(), &refs);
    let pairs = |i: &PromptIndex| {
        i.markers()
            .iter()
            .map(|m| (m.row, m.marker))
            .collect::<Vec<_>>()
    };
    assert_eq!(pairs(index), pairs(&fresh));
    assert_eq!(index.line_count(), fresh.line_count());
    assert_eq!(index.block_count(), fresh.block_count());
}

#[test]
fn random_edits_match_a_full_rebuild() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut lines: Vec<String> = (0..12).map(|_| random_line(&mut rng)).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut index = PromptIndex::build(MarkerSyntax::default(), &refs);
    let mut edits = 0;
    while edits < 1_000 {
        if rng.next(4) == 0 && lines.len() > 6 {
            // several disjoint edits, applied last to first
            let mut ranges: Vec<Range<u32>> = Vec::new();
            let mut at = 0u32;
            while (at as usize) < lines.len() && ranges.len() < 3 {
                let start = at + rng.next(3) as u32;
                if start as usize > lines.len() {
                    break;
                }
                let end = (start + rng.next(3) as u32).min(lines.len() as u32);
                ranges.push(start..end);
                at = end + 1;
            }
            for range in ranges.into_iter().rev() {
                let new: Vec<String> = (0..rng.next(3)).map(|_| random_line(&mut rng)).collect();
                let refs: Vec<&str> = new.iter().map(String::as_str).collect();
                index.edit(range.clone(), &refs);
                lines.splice(range.start as usize..range.end as usize, new);
                edits += 1;
            }
        } else {
            let start = rng.next(lines.len() as u64 + 1) as u32;
            let end = (start + rng.next(4) as u32).min(lines.len() as u32);
            let new: Vec<String> = (0..rng.next(4)).map(|_| random_line(&mut rng)).collect();
            let refs: Vec<&str> = new.iter().map(String::as_str).collect();
            index.edit(start..end, &refs);
            lines.splice(start as usize..end as usize, new);
            edits += 1;
        }
        if lines.is_empty() {
            let line = String::new();
            index.edit(0..0, &[""]);
            lines.push(line);
        }
        check_matches_rebuild(&index, &lines);
    }
}

#[test]
fn same_row_edits_merge_into_one() {
    let edit = |old: Range<u32>, new: Range<u32>| RowEdit {
        old_rows: old,
        new_rows: new,
    };
    let merged = merge_row_edits(&[edit(3..4, 3..4), edit(3..4, 3..4), edit(7..8, 7..9)]);
    assert_eq!(merged, vec![edit(3..4, 3..4), edit(7..8, 7..9)]);
    // a later edit overlapping the first one's rows, after the first grew by one row
    let merged = merge_row_edits(&[edit(3..5, 3..6), edit(5..6, 6..7), edit(4..5, 4..5)]);
    assert_eq!(merged.len(), 2);
    let merged = merge_row_edits(&[edit(2..2, 2..3), edit(2..3, 3..4)]);
    assert_eq!(merged, vec![edit(2..3, 2..4)]);
}
