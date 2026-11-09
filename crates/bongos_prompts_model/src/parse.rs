use std::ops::Range;

use crate::marker::{MarkerLine, MarkerSyntax};

/// A block: a marker row (or none, for text before the first marker) and the body rows up to the
/// next marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub marker_row: Option<u32>,
    pub marker: Option<MarkerLine>,
    /// Body rows, excluding the marker row. Empty when the marker is the last row or is followed
    /// by another marker.
    pub body: Range<u32>,
}

impl Block {
    /// Pending unless the marker carries a sent stamp.
    pub fn is_pending(&self) -> bool {
        !self.marker.is_some_and(|marker| marker.is_sent())
    }

    /// The text this block sends and copies, or `None` when the body is empty (blank).
    pub fn send_text(&self, rows: &[&str]) -> Option<String> {
        let body = rows.get(self.body.start as usize..self.body.end as usize)?;
        let text = send_text(body);
        (!text.is_empty()).then_some(text)
    }
}

/// Rows as an editor counts them: split on `\n`, so a trailing newline gives a final empty row and
/// an empty text is one empty row. A `\r` before the break stays on its row.
pub fn split_rows(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// A body's rows with leading and trailing blank (whitespace-only) rows removed, joined with `\n`.
/// Interior rows are untouched, except that the `\r` of a CRLF line break is dropped, so a file
/// with CRLF line breaks sends the same text as one with LF.
pub fn send_text(body: &[&str]) -> String {
    let rows: Vec<&str> = body
        .iter()
        .map(|row| row.strip_suffix('\r').unwrap_or(row))
        .collect();
    let blank = |row: &&str| row.trim().is_empty();
    let start = rows.iter().position(|row| !blank(row));
    let Some(start) = start else {
        return String::new();
    };
    let end = rows.iter().rposition(|row| !blank(row)).unwrap_or(start);
    rows[start..=end].join("\n")
}

/// Splits a file into blocks. Text before the first marker is block 0 with no marker, so an empty
/// file is one pending block; a file that starts with a marker has no such block.
pub fn parse(text: &str, syntax: &MarkerSyntax) -> Vec<Block> {
    let rows = split_rows(text);
    let mut blocks: Vec<Block> = Vec::new();
    for (row, line) in rows.iter().enumerate() {
        let row = row as u32;
        if let Some(marker) = syntax.parse(line) {
            if let Some(last) = blocks.last_mut() {
                last.body.end = row;
            }
            blocks.push(Block {
                marker_row: Some(row),
                marker: Some(marker),
                body: row + 1..row + 1,
            });
        } else if blocks.is_empty() {
            blocks.push(Block {
                marker_row: None,
                marker: None,
                body: 0..0,
            });
        }
    }
    let total = rows.len() as u32;
    if let Some(last) = blocks.last_mut() {
        last.body.end = total;
    }
    blocks
}
