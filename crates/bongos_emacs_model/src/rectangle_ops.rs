//! Row plans for the rectangle commands. Each function takes the buffer rows a
//! rectangle touches and returns edits in buffer offsets (sorted, disjoint, in
//! pre-edit coordinates), the rows a copy or kill extracts, and the point
//! after the edits. A tab cut by an edge becomes spaces so the cells on each
//! side of the cut keep their width, and a wide character is never split: it
//! belongs to a rectangle when its first cell is inside it.

use std::ops::Range;

use crate::rectangle::{Columns, Locate, cells, locate};

/// One buffer line without its newline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowText {
    /// Buffer offset of the row's first byte.
    pub start: usize,
    pub text: String,
}

/// A rectangle operation. `Delete` also serves for kill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RectOp {
    Copy,
    Delete,
    Open,
    Clear,
    String(String),
    Keep,
}

/// The result of planning a rectangle operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RectPlan {
    /// Replacements in buffer offsets, sorted and disjoint.
    pub edits: Vec<(Range<usize>, String)>,
    /// The rectangle's rows, filled by `Copy` and `Delete` only.
    pub extracted: Option<Vec<String>>,
    /// Buffer offset of the point after the edits. For `Copy` this is the
    /// top-left corner, which callers normally ignore in favor of their own
    /// point. It is 0 when there are no rows.
    pub point: usize,
}

/// One character of a row, or one column of a tab cut by an edge.
struct Unit {
    start: usize,
    width: usize,
    text: String,
}

/// A row cut into the parts left of, inside and right of a column range.
#[derive(Default)]
struct Split {
    before: String,
    inside: String,
    after: String,
    inside_width: usize,
    total: usize,
}

fn spaces(count: usize) -> String {
    " ".repeat(count)
}

fn units(line: &str, edges: &[usize], tab: usize) -> Vec<Unit> {
    let all = cells(line, tab);
    let mut out = Vec::with_capacity(all.len());
    for (index, cell) in all.iter().enumerate() {
        let end = all.get(index + 1).map_or(line.len(), |next| next.byte);
        let Some(text) = line.get(cell.byte..end) else {
            continue;
        };
        let cell_end = cell.start_col + cell.width;
        let cut = text == "\t"
            && edges
                .iter()
                .any(|&edge| cell.start_col < edge && edge < cell_end);
        if cut {
            for offset in 0..cell.width {
                out.push(Unit {
                    start: cell.start_col + offset,
                    width: 1,
                    text: " ".to_string(),
                });
            }
        } else {
            out.push(Unit {
                start: cell.start_col,
                width: cell.width,
                text: text.to_string(),
            });
        }
    }
    out
}

/// Splits units into before (0), inside (1) and after (2) by `side`; a
/// zero-width character stays with the character before it.
fn split(units: &[Unit], side: impl Fn(&Unit) -> u8) -> Split {
    let mut parts = Split::default();
    let mut previous = 0;
    for unit in units {
        let which = if unit.width == 0 {
            previous
        } else {
            side(unit)
        };
        previous = which;
        match which {
            0 => parts.before.push_str(&unit.text),
            1 => {
                parts.inside.push_str(&unit.text);
                parts.inside_width += unit.width;
            }
            _ => parts.after.push_str(&unit.text),
        }
        parts.total = parts.total.max(unit.start + unit.width);
    }
    parts
}

/// The smallest single replacement turning `old` into `new`, or none.
fn row_edit(start: usize, old: &str, new: &str) -> Option<(Range<usize>, String)> {
    if old == new {
        return None;
    }
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let old_rest = old.get(prefix..).unwrap_or("");
    let new_rest = new.get(prefix..).unwrap_or("");
    let suffix: usize = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let old_end = old_rest.len().saturating_sub(suffix);
    let new_end = new_rest.len().saturating_sub(suffix);
    let replacement = new_rest.get(..new_end).unwrap_or("").to_string();
    Some((start + prefix..start + prefix + old_end, replacement))
}

/// Plans `op` over the rectangle `cols` of `rows` (consecutive buffer rows,
/// top first).
pub fn plan(op: RectOp, rows: &[RowText], cols: Columns, tab: usize) -> RectPlan {
    let width = cols.width();
    let mut result = RectPlan::default();
    let mut extracted = Vec::new();
    let extracts = matches!(op, RectOp::Copy | RectOp::Delete);
    for (index, row) in rows.iter().enumerate() {
        let all = units(&row.text, &[cols.left, cols.right], tab);
        let parts = split(&all, |unit| {
            if unit.start < cols.left {
                0
            } else if unit.start < cols.right {
                1
            } else {
                2
            }
        });
        let short = parts.total < cols.left;
        let pad = if short { cols.left - parts.total } else { 0 };
        let mut lead = 0;
        let new = match &op {
            RectOp::Copy => None,
            RectOp::Delete => Some(format!("{}{}", parts.before, parts.after)),
            RectOp::Open if width == 0 => None,
            RectOp::Open => {
                lead = pad;
                Some(format!(
                    "{}{}{}{}{}",
                    parts.before,
                    spaces(pad),
                    spaces(width),
                    parts.inside,
                    parts.after
                ))
            }
            RectOp::Clear => Some(format!(
                "{}{}{}",
                parts.before,
                spaces(parts.inside_width),
                parts.after
            )),
            RectOp::String(text) => {
                lead = pad;
                Some(format!(
                    "{}{}{}{}",
                    parts.before,
                    spaces(pad),
                    text,
                    parts.after
                ))
            }
            RectOp::Keep if short => Some(String::new()),
            RectOp::Keep => Some(parts.inside.clone()),
        };
        if index == 0 {
            result.point = row.start
                + if matches!(op, RectOp::Keep) {
                    0
                } else {
                    parts.before.len() + lead
                };
        }
        if extracts {
            extracted.push(format!(
                "{}{}",
                parts.inside,
                spaces(width.saturating_sub(parts.inside_width))
            ));
        }
        if let Some(new) = new {
            result.edits.extend(row_edit(row.start, &row.text, &new));
        }
    }
    if extracts {
        result.extracted = Some(extracted);
    }
    result
}

/// Display width of `text` laid out from column `start`.
fn width_from(text: &str, start: usize, tab: usize) -> usize {
    let tab = tab.max(1);
    let mut col = start;
    for ch in text.chars() {
        col += if ch == '\t' {
            tab - col % tab
        } else {
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1)
        };
    }
    col - start
}

/// Byte offset of display column `col` in `text`: the start of a tab or wide
/// character containing it, or the end of the text when the column is past it.
fn byte_at(text: &str, col: usize, tab: usize) -> usize {
    match locate(text, col, tab) {
        Locate::At(byte) | Locate::InsideTab { byte, .. } | Locate::InsideWide { byte } => byte,
        Locate::PastEnd { .. } => text.len(),
    }
}

/// Plans a rectangle yank: row `i` of `entry` goes on buffer row
/// `at_row + i` at display column `at_col`. Rows past the last buffer row are
/// appended at `eof` as new lines.
pub fn insert(
    rows: &[RowText],
    at_row: usize,
    at_col: usize,
    entry: &[String],
    eof: usize,
    tab: usize,
) -> RectPlan {
    let mut result = RectPlan::default();
    if entry.iter().all(|row| row.is_empty()) {
        result.point = rows
            .get(at_row)
            .map_or(eof, |row| row.start + byte_at(&row.text, at_col, tab));
        return result;
    }
    let widest = entry
        .iter()
        .map(|row| width_from(row, at_col, tab))
        .max()
        .unwrap_or(0);
    let mut delta: isize = 0;
    // Post-edit start and text of the lowest row written so far.
    let mut bottom: Option<(usize, String)> = None;
    for (index, row_entry) in entry.iter().enumerate() {
        let Some(row) = rows.get(at_row + index) else {
            break;
        };
        let all = units(&row.text, &[at_col], tab);
        let parts = split(&all, |unit| {
            if unit.start + unit.width <= at_col {
                0
            } else {
                2
            }
        });
        let pad = at_col.saturating_sub(parts.total);
        let placed = if parts.after.is_empty() {
            row_entry.clone()
        } else {
            let own = width_from(row_entry, at_col, tab);
            format!("{}{}", row_entry, spaces(widest.saturating_sub(own)))
        };
        let new = format!("{}{}{}{}", parts.before, spaces(pad), placed, parts.after);
        let post_start = row.start.saturating_add_signed(delta);
        if let Some(edit) = row_edit(row.start, &row.text, &new) {
            result.edits.push(edit);
        }
        delta += new.len() as isize - row.text.len() as isize;
        bottom = Some((post_start, new));
    }
    let first_missing = rows.len();
    let last_row = at_row + entry.len();
    if first_missing < last_row {
        let mut appended = String::new();
        let mut last_start = 0;
        let mut last_line = String::new();
        for line in first_missing..last_row {
            appended.push('\n');
            last_start = appended.len();
            last_line = match line.checked_sub(at_row).and_then(|i| entry.get(i)) {
                Some(row_entry) => format!("{}{}", spaces(at_col), row_entry),
                None => String::new(),
            };
            appended.push_str(&last_line);
        }
        bottom = Some((eof.saturating_add_signed(delta) + last_start, last_line));
        result.edits.push((eof..eof, appended));
    }
    if let Some((start, text)) = bottom {
        result.point = start + byte_at(&text, at_col + widest, tab);
    }
    result
}
