use std::ops::Range;

use crate::text::{Caret, Plan, TextSource, line_end, line_start, map_offset, slice};

/// The lines a region touches, from the start of the first to the end of the
/// last, without its newline. A region that ends right after a newline does
/// not touch the line that follows. A caret without a region touches its line.
pub(crate) fn touched_lines<T: TextSource + ?Sized>(
    text: &T,
    region: &Range<usize>,
) -> Range<usize> {
    let start = line_start(text, region.start);
    let ends_after_newline =
        region.end > region.start && text.reversed_chars_at(region.end).next() == Some('\n');
    let last = if ends_after_newline {
        region.end - 1
    } else {
        region.end
    };
    start..line_end(text, last)
}

/// The whole-line span of a caret's region (or its line without one): from the
/// start of the first touched line to the end of the last, newline excluded.
pub fn whole_line_span<T: TextSource + ?Sized>(text: &T, caret: &Caret) -> Range<usize> {
    touched_lines(text, &caret.region())
}

/// Text to insert at `at`, and where the caret lands inside it.
struct Insert {
    at: usize,
    text: String,
    caret: usize,
}

/// A plan of pure insertions. Inserts at one offset collapse to the first;
/// `others` are carets that insert nothing and keep their region.
fn insert_plan(mut inserts: Vec<Insert>, others: &[Caret]) -> Plan {
    inserts.sort_by_key(|i| i.at);
    inserts.dedup_by_key(|i| i.at);
    let mut plan = Plan::default();
    let mut carets = Vec::new();
    let mut delta = 0usize;
    for insert in inserts {
        carets.push(Caret::point(insert.at + delta + insert.caret));
        delta += insert.text.len();
        plan.edits.push((insert.at..insert.at, insert.text));
    }
    carets.extend(others.iter().map(|c| Caret {
        head: map_offset(&plan.edits, c.head),
        tail: map_offset(&plan.edits, c.tail),
    }));
    carets.sort_by_key(|c| (c.head, c.tail));
    carets.dedup();
    plan.carets = carets;
    plan
}

/// duplicate-line. A region is copied once, character for character, right
/// after itself, and the caret lands after the copy with no region. Without a
/// region, N copies (`None` is 1) of the line go below it; the caret moves to
/// the first copy at the same column. 0 or a negative count does nothing.
pub fn duplicate_line<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let n = count.unwrap_or(1);
    let mut inserts = Vec::new();
    let mut others = Vec::new();
    for caret in carets {
        if caret.has_region() {
            let copy = slice(text, &caret.region());
            inserts.push(Insert {
                at: caret.region().end,
                caret: copy.len(),
                text: copy,
            });
        } else if n > 0 {
            let start = line_start(text, caret.head);
            let end = line_end(text, caret.head);
            let copy = format!("\n{}", slice(text, &(start..end)));
            inserts.push(Insert {
                at: end,
                text: copy.repeat(n as usize),
                caret: 1 + caret.head - start,
            });
        } else {
            others.push(Caret::point(caret.head));
        }
    }
    insert_plan(inserts, &others)
}

fn is_blank<T: TextSource + ?Sized>(text: &T, line: &Range<usize>) -> bool {
    text.chars_at(line.start)
        .take_while(|&c| c != '\n')
        .all(char::is_whitespace)
}

/// copy-from-above: inserts at point the text of the nearest non-blank line
/// above, starting at point's column (counted in chars): N chars (`None` is to
/// the end of that line). A caret with a region is left alone. Nothing happens
/// with no such line, a column past its end, or a count of 0 or less.
pub fn copy_from_above<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let mut inserts = Vec::new();
    let mut others = Vec::new();
    for caret in carets {
        let copy = if caret.has_region() || count.is_some_and(|n| n <= 0) {
            None
        } else {
            text_above(text, caret.head, count)
        };
        match copy {
            Some(copy) if !copy.is_empty() => inserts.push(Insert {
                at: caret.head,
                caret: copy.len(),
                text: copy,
            }),
            _ if caret.has_region() => others.push(*caret),
            _ => others.push(Caret::point(caret.head)),
        }
    }
    insert_plan(inserts, &others)
}

fn text_above<T: TextSource + ?Sized>(
    text: &T,
    point: usize,
    count: Option<i32>,
) -> Option<String> {
    let start = line_start(text, point);
    let column = slice(text, &(start..point)).chars().count();
    let mut line_begin = start;
    let line = loop {
        if line_begin == 0 {
            return None;
        }
        let end = line_begin - 1;
        line_begin = line_start(text, end);
        let line = line_begin..end;
        if !is_blank(text, &line) {
            break line;
        }
    };
    let rest = text
        .chars_at(line.start)
        .take_while(|&c| c != '\n')
        .skip(column);
    Some(match count {
        Some(n) => rest.take(n as usize).collect(),
        None => rest.collect(),
    })
}
