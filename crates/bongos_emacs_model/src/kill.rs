use std::ops::Range;

use crate::text::{
    Caret, KilledPiece, Place, Plan, Target, TextSource, build_plan, deletion_plan, line_end,
    line_start, slice,
};

fn is_horizontal(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Offset just past the newline that ends the line at `end` (the text length
/// on the last line).
fn past_newline<T: TextSource + ?Sized>(text: &T, end: usize) -> usize {
    (end + 1).min(text.len())
}

/// Offset just past the `n`th newline at or after `from`, or the text end.
fn past_nth_newline<T: TextSource + ?Sized>(text: &T, from: usize, n: u32) -> usize {
    let mut pos = from;
    for _ in 0..n {
        let end = line_end(text, pos);
        if end >= text.len() {
            return text.len();
        }
        pos = end + 1;
    }
    pos
}

/// Start of the line `n` lines above the line starting at `start`.
fn nth_line_start_above<T: TextSource + ?Sized>(text: &T, mut start: usize, n: u32) -> usize {
    for _ in 0..n {
        if start == 0 {
            break;
        }
        start = line_start(text, start - 1);
    }
    start
}

/// kill-line. A caret with a region kills the region (the count is ignored).
/// Otherwise, with no count, it kills to the line end, and through the newline
/// too when only spaces and tabs follow point or point is at the line start.
/// N > 0 kills from point through the Nth newline; 0 kills from the line start
/// to point; N < 0 kills back to the start of the line N lines above.
pub fn kill_line<T: TextSource + ?Sized>(text: &T, carets: &[Caret], count: Option<i32>) -> Plan {
    let targets = carets
        .iter()
        .map(|caret| {
            if caret.has_region() {
                return Target {
                    range: caret.region(),
                    backward: false,
                    place: Place::Start,
                };
            }
            let point = caret.head;
            let start = line_start(text, point);
            let end = line_end(text, point);
            let (range, backward) = match count {
                None => {
                    let rest_blank = text
                        .chars_at(point)
                        .take_while(|&c| c != '\n')
                        .all(is_horizontal);
                    if point == start || rest_blank {
                        (point..past_newline(text, end), false)
                    } else {
                        (point..end, false)
                    }
                }
                Some(0) => (start..point, true),
                Some(n) if n > 0 => (
                    point..past_nth_newline(text, point, n.unsigned_abs()),
                    false,
                ),
                Some(n) => (
                    nth_line_start_above(text, start, n.unsigned_abs())..point,
                    true,
                ),
            };
            Target {
                range,
                backward,
                place: Place::Start,
            }
        })
        .collect();
    build_plan(text, targets, true, &|_, _| String::new())
}

/// What one caret kills or copies in kill-region-or-line.
struct Item {
    range: Range<usize>,
    backward: bool,
    /// A last line without a newline was taken: the piece must end in one.
    needs_newline: bool,
    /// The range was widened to include the newline before the last line;
    /// the piece moves that newline to its end.
    widened_at: Option<usize>,
    /// Taken by a caret with no region, and merged only with such items.
    whole_line: bool,
}

fn whole_lines<T: TextSource + ?Sized>(text: &T, point: usize, n: i32) -> Item {
    let start = line_start(text, point);
    let len = text.len();
    let mut item = Item {
        range: point..point,
        backward: false,
        needs_newline: false,
        widened_at: None,
        whole_line: true,
    };
    if n > 0 {
        let end = past_nth_newline(text, start, n.unsigned_abs());
        item.range = start..end;
        if end == len && start < len && !ends_with_newline(text) {
            item.needs_newline = true;
            if start > 0 {
                item.range.start = start - 1;
                item.widened_at = Some(start - 1);
            }
        }
    } else if n < 0 {
        item.range = nth_line_start_above(text, start, n.unsigned_abs())..start;
        item.backward = true;
    }
    item
}

fn ends_with_newline<T: TextSource + ?Sized>(text: &T) -> bool {
    text.reversed_chars_at(text.len()).next() == Some('\n')
}

/// Whole-line-or-region ranges, merged where carets overlap, one per caret
/// group. Empty items stay so every caret keeps its place.
fn items<T: TextSource + ?Sized>(text: &T, carets: &[Caret], count: Option<i32>) -> Vec<Item> {
    let n = count.unwrap_or(1);
    let mut items: Vec<Item> = carets
        .iter()
        .map(|caret| {
            if caret.has_region() {
                Item {
                    range: caret.region(),
                    backward: false,
                    needs_newline: false,
                    widened_at: None,
                    whole_line: false,
                }
            } else if caret.is_active() {
                // An active empty region (mark just set) kills nothing, never the line.
                Item {
                    range: caret.head..caret.head,
                    backward: false,
                    needs_newline: false,
                    widened_at: None,
                    whole_line: false,
                }
            } else {
                whole_lines(text, caret.head, n)
            }
        })
        .collect();
    items.sort_by_key(|i| (i.range.start, i.range.end));
    let mut merged: Vec<Item> = Vec::with_capacity(items.len());
    for item in items {
        match merged.last_mut() {
            Some(last)
                if item.range.start < last.range.end || item.range.start == last.range.start =>
            {
                last.range.end = last.range.end.max(item.range.end);
                last.needs_newline |= item.needs_newline;
                last.widened_at = last.widened_at.or(item.widened_at);
                last.whole_line &= item.whole_line;
            }
            _ => merged.push(item),
        }
    }
    merged
}

fn piece<T: TextSource + ?Sized>(text: &T, item: &Item) -> KilledPiece {
    let mut text = slice(text, &item.range);
    let mut source_start = item.range.start;
    if item.needs_newline {
        if item.widened_at == Some(item.range.start) && text.starts_with('\n') {
            text.remove(0);
            source_start += 1;
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    KilledPiece {
        text,
        backward: item.backward,
        source_start: Some(source_start),
        whole_line: item.whole_line,
    }
}

/// kill-region-or-line: a caret with a region kills it (the count is
/// ignored); an active empty region kills nothing; otherwise it kills N whole lines (`None` is 1) from its line
/// start, newline included. On a last line without a newline it kills the line
/// and the newline before it, and the piece still ends in a newline. A
/// negative count kills the N lines above the caret's line; 0 kills nothing.
pub fn kill_region_or_line<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let items = items(text, carets, count);
    let killed = items
        .iter()
        .filter(|item| !item.range.is_empty())
        .map(|item| piece(text, item))
        .collect();
    let ranges = items.iter().map(|item| item.range.clone()).collect();
    let after: Vec<Caret> = items
        .iter()
        .map(|item| Caret::point(item.range.start))
        .collect();
    deletion_plan(ranges, &after, killed)
}

/// copy-region-or-line: the pieces kill-region-or-line would kill, with no
/// edit; the carets are returned as given.
pub fn copy_region_or_line<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let killed = items(text, carets, count)
        .iter()
        .filter(|item| !item.range.is_empty())
        .map(|item| piece(text, item))
        .collect();
    Plan {
        edits: Vec::new(),
        carets: carets.to_vec(),
        killed,
    }
}
