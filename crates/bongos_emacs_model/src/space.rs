use std::ops::Range;

use crate::text::{
    Caret, Place, Plan, Target, TextSource, build_plan, deletion_plan, line_end, line_start,
};

fn is_horizontal(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn skip_back<T: TextSource + ?Sized>(text: &T, from: usize, skip: impl Fn(char) -> bool) -> usize {
    let mut pos = from;
    for c in text.reversed_chars_at(from) {
        if !skip(c) {
            break;
        }
        pos -= c.len_utf8();
    }
    pos
}

fn skip_forward<T: TextSource + ?Sized>(
    text: &T,
    from: usize,
    skip: impl Fn(char) -> bool,
) -> usize {
    let mut pos = from;
    for c in text.chars_at(from) {
        if !skip(c) {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// delete-horizontal-space: deletes the spaces and tabs around each caret's
/// point. Any count (even 0) deletes only the ones before point. A region is
/// cleared; the command acts at point. Nothing is killed.
pub fn delete_horizontal_space<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let targets = carets
        .iter()
        .map(|caret| {
            let start = skip_back(text, caret.head, is_horizontal);
            let end = if count.is_none() {
                skip_forward(text, caret.head, is_horizontal)
            } else {
                caret.head
            };
            Target {
                range: start..end,
                backward: false,
                place: Place::Start,
            }
        })
        .collect();
    build_plan(text, targets, false, &|_, _| String::new())
}

/// just-one-space: replaces the spaces and tabs around each caret's point with
/// N spaces (`None` is 1; 0 leaves none). A negative count leaves -N spaces and
/// also removes newlines around point. A region is cleared; point ends after
/// the spaces.
pub fn just_one_space<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let n = count.unwrap_or(1);
    let with_newlines = n < 0;
    let skip = move |c: char| is_horizontal(c) || (with_newlines && c == '\n');
    let spaces = " ".repeat(n.unsigned_abs() as usize);
    let targets = carets
        .iter()
        .map(|caret| Target {
            range: skip_back(text, caret.head, skip)..skip_forward(text, caret.head, skip),
            backward: false,
            place: Place::End,
        })
        .collect();
    build_plan(text, targets, false, &|_, _| spaces.clone())
}

fn is_blank_line<T: TextSource + ?Sized>(text: &T, line_start: usize) -> bool {
    text.chars_at(line_start)
        .take_while(|&c| c != '\n')
        .all(is_horizontal)
}

/// Start of the first line of the run of blank lines ending just before
/// `line_start` (`line_start` itself when the previous line is not blank).
fn blank_run_start<T: TextSource + ?Sized>(text: &T, mut start: usize) -> usize {
    while start > 0 {
        let previous = line_start(text, start - 1);
        if !is_blank_line(text, previous) {
            break;
        }
        start = previous;
    }
    start
}

/// Start of the first nonblank line at or after the line starting at `from`,
/// or the text length when only blank lines remain.
fn blank_run_end<T: TextSource + ?Sized>(text: &T, mut from: usize) -> usize {
    let len = text.len();
    while from < len {
        if !is_blank_line(text, from) {
            return from;
        }
        let end = line_end(text, from);
        if end >= len {
            return len;
        }
        from = end + 1;
    }
    len
}

/// delete-blank-lines at each caret's point; the count is ignored and a region
/// is cleared. On a lone blank line it deletes that line. On one of several
/// blank lines it deletes the others and keeps that line. On a nonblank line it
/// deletes the blank lines that follow. A blank run that reaches the end of the
/// text leaves no blank line behind.
pub fn delete_blank_lines<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    _count: Option<i32>,
) -> Plan {
    let len = text.len();
    let mut heads: Vec<usize> = carets.iter().map(|c| c.head).collect();
    heads.sort_unstable();
    heads.dedup();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut last_run: Option<(usize, usize)> = None;
    for &point in &heads {
        let start = line_start(text, point);
        let end = line_end(text, point);
        let next = if end < len { end + 1 } else { len };
        let following = blank_run_end(text, next);
        if !is_blank_line(text, start) {
            ranges.push(next..following);
            continue;
        }
        let preceding = blank_run_start(text, start);
        if last_run == Some((preceding, following)) {
            continue;
        }
        last_run = Some((preceding, following));
        if preceding == start && following == next {
            // The only blank line: delete it.
            ranges.push(start..next);
        } else if following == len && end < len {
            // The rest of the text is blank lines; none is worth keeping.
            ranges.push(preceding..len);
        } else {
            ranges.push(preceding..start);
            ranges.push(next..following);
        }
    }
    let kept: Vec<Caret> = carets.iter().map(|c| Caret::point(c.head)).collect();
    deletion_plan(ranges, &kept, Vec::new())
}
