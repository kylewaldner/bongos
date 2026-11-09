use std::ops::Range;

use chrono::NaiveDateTime;

use crate::index::BlockInfo;
use crate::marker::{MarkerLine, MarkerSyntax};

/// One edit in whole rows: replace `rows` (rows of the current text; an empty range at `row`
/// inserts before `row`, and `line_count` appends) with `lines`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEdit {
    pub rows: Range<u32>,
    pub lines: Vec<String>,
}

impl PlanEdit {
    /// The text after the edit, for tests and callers without an editor.
    pub fn apply(&self, text: &str) -> String {
        let mut rows: Vec<&str> = text.split('\n').collect();
        let start = (self.rows.start as usize).min(rows.len());
        let end = (self.rows.end as usize).clamp(start, rows.len());
        rows.splice(start..end, self.lines.iter().map(String::as_str));
        rows.join("\n")
    }
}

/// A fresh marker row inserted above `row`, created `now`, pending.
pub fn insert_marker_above(row: u32, now: NaiveDateTime, syntax: &MarkerSyntax) -> PlanEdit {
    PlanEdit {
        rows: row..row,
        lines: vec![syntax.render(&MarkerLine::pending(now))],
    }
}

/// Stamps `block` as sent `now`: its marker row is replaced with the same created stamp and the new
/// sent stamp (a re-send replaces the old one); a block with no marker gets a marker row inserted
/// above it with both stamps `now`. Body rows are never touched.
pub fn mark_sent(block: &BlockInfo, now: NaiveDateTime, syntax: &MarkerSyntax) -> PlanEdit {
    match (block.marker_row, block.marker) {
        (Some(row), Some(marker)) => PlanEdit {
            rows: row..row + 1,
            lines: vec![syntax.render(&MarkerLine {
                created: marker.created,
                sent: Some(now),
            })],
        },
        _ => PlanEdit {
            rows: block.body.start..block.body.start,
            lines: vec![syntax.render(&MarkerLine {
                created: now,
                sent: Some(now),
            })],
        },
    }
}

/// A new pending block right after `block`: a marker row and an empty body row, inserted where
/// the block's body ends.
pub fn new_block_after(block: &BlockInfo, now: NaiveDateTime, syntax: &MarkerSyntax) -> PlanEdit {
    PlanEdit {
        rows: block.body.end..block.body.end,
        lines: vec![syntax.render(&MarkerLine::pending(now)), String::new()],
    }
}
