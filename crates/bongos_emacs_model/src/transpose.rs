use std::ops::Range;

use crate::text::{Caret, Plan, TextSource, line_end, line_start, map_offset, slice};

/// One transposition: the range it rewrites, the new text, and where the
/// caret lands inside the new text.
struct Rewrite {
    range: Range<usize>,
    new: String,
    caret: usize,
}

/// Builds the plan for a set of rewrites. A rewrite that overlaps an earlier
/// one is dropped and its caret merges into the earlier one; carets without a
/// rewrite stay at their point, mapped into the new text. Every caret ends up
/// without a region.
fn rewrite_plan<T: TextSource + ?Sized>(
    text: &T,
    mut rewrites: Vec<Rewrite>,
    idle: Vec<usize>,
) -> Plan {
    rewrites.sort_by_key(|r| (r.range.start, r.range.end));
    let mut plan = Plan::default();
    let mut delta = 0isize;
    let mut last_end = 0;
    let mut landed: Vec<usize> = Vec::new();
    for rewrite in rewrites {
        if rewrite.range.start < last_end {
            continue;
        }
        if slice(text, &rewrite.range) != rewrite.new {
            plan.edits
                .push((rewrite.range.clone(), rewrite.new.clone()));
        }
        let start = (rewrite.range.start as isize + delta) as usize;
        landed.push(start + rewrite.caret);
        delta += rewrite.new.len() as isize - rewrite.range.len() as isize;
        last_end = rewrite.range.end;
    }
    landed.extend(idle.into_iter().map(|head| map_offset(&plan.edits, head)));
    landed.sort_unstable();
    landed.dedup();
    plan.carets = landed.into_iter().map(Caret::point).collect();
    plan
}

/// Moves `spans[0]` past the others (`first_to_last`) or the last span before
/// the others, leaving the text between the spans where it is. The caret lands
/// after the moved span.
fn rotate<T: TextSource + ?Sized>(
    text: &T,
    spans: &[Range<usize>],
    first_to_last: bool,
) -> Rewrite {
    let pieces: Vec<String> = spans.iter().map(|s| slice(text, s)).collect();
    let gaps: Vec<String> = spans
        .windows(2)
        .map(|w| slice(text, &(w[0].end..w[1].start)))
        .collect();
    let last = pieces.len() - 1;
    let order: Vec<usize> = if first_to_last {
        (1..=last).chain([0]).collect()
    } else {
        [last].into_iter().chain(0..last).collect()
    };
    let mut new = String::new();
    for (k, &piece) in order.iter().enumerate() {
        new.push_str(&pieces[piece]);
        if let Some(gap) = gaps.get(k) {
            new.push_str(gap);
        }
    }
    let caret = if first_to_last {
        new.len()
    } else {
        pieces[last].len()
    };
    Rewrite {
        range: spans[0].start..spans[last].end,
        new,
        caret,
    }
}

/// transpose-chars: the char before point moves forward N chars (`None` is 1)
/// and point ends after it; a negative count moves it backward. A plain
/// transposition at the end of a line swaps the two chars before point instead
/// (nothing happens when that would move a newline). Counts stop at the text
/// ends; 0 and a point at the start do nothing. A region is cleared.
pub fn transpose_chars<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let n = count.unwrap_or(1);
    let mut rewrites = Vec::new();
    let mut idle = Vec::new();
    for caret in carets {
        match transpose_chars_at(text, caret.head, n) {
            Some(rewrite) => rewrites.push(rewrite),
            None => idle.push(caret.head),
        }
    }
    rewrite_plan(text, rewrites, idle)
}

fn transpose_chars_at<T: TextSource + ?Sized>(text: &T, head: usize, n: i32) -> Option<Rewrite> {
    if n == 0 {
        return None;
    }
    let mut point = head;
    let mut before = text.reversed_chars_at(point).next()?;
    let at_eol = matches!(text.chars_at(point).next(), None | Some('\n'));
    if n == 1 && at_eol {
        if before == '\n' {
            return None;
        }
        point -= before.len_utf8();
        before = text.reversed_chars_at(point).next()?;
        if before == '\n' {
            return None;
        }
    }
    let width = before.len_utf8();
    if n > 0 {
        let following: String = text.chars_at(point).take(n as usize).collect();
        if following.is_empty() {
            return None;
        }
        let range = point - width..point + following.len();
        Some(Rewrite {
            new: format!("{following}{before}"),
            caret: range.len(),
            range,
        })
    } else {
        let mut preceding: Vec<char> = text
            .reversed_chars_at(point - width)
            .take(n.unsigned_abs() as usize)
            .collect();
        if preceding.is_empty() {
            return None;
        }
        preceding.reverse();
        let preceding: String = preceding.into_iter().collect();
        let range = point - width - preceding.len()..point;
        Some(Rewrite {
            new: format!("{before}{preceding}"),
            caret: width,
            range,
        })
    }
}

fn is_word_at<T: TextSource + ?Sized>(text: &T, offset: usize) -> bool {
    text.chars_at(offset)
        .next()
        .is_some_and(|c| text.is_word_char(c, offset))
}

/// The word that ends at or after `from`, skipping non-word chars first. A
/// `from` inside a word yields the rest of that word.
fn word_after<T: TextSource + ?Sized>(text: &T, from: usize) -> Option<Range<usize>> {
    let mut start = from;
    for c in text.chars_at(from) {
        if text.is_word_char(c, start) {
            break;
        }
        start += c.len_utf8();
    }
    let mut end = start;
    for c in text.chars_at(start) {
        if !text.is_word_char(c, end) {
            break;
        }
        end += c.len_utf8();
    }
    (end > start).then_some(start..end)
}

/// The word that starts before `from`, skipping non-word chars first. A `from`
/// inside a word yields the part before it.
fn word_before<T: TextSource + ?Sized>(text: &T, from: usize) -> Option<Range<usize>> {
    let mut end = from;
    for c in text.reversed_chars_at(from) {
        if is_word_at(text, end - c.len_utf8()) {
            break;
        }
        end -= c.len_utf8();
    }
    let mut start = end;
    for c in text.reversed_chars_at(end) {
        if !is_word_at(text, start - c.len_utf8()) {
            break;
        }
        start -= c.len_utf8();
    }
    (end > start).then_some(start..end)
}

/// The word before point, or the whole word point is inside.
fn word_at_or_before<T: TextSource + ?Sized>(text: &T, point: usize) -> Option<Range<usize>> {
    let word = word_before(text, point)?;
    if word.end < point {
        return Some(word);
    }
    let mut end = word.end;
    for c in text.chars_at(end) {
        if !text.is_word_char(c, end) {
            break;
        }
        end += c.len_utf8();
    }
    Some(word.start..end)
}

/// transpose-words: the word before point (or the word point is inside) moves
/// forward N words (`None` is 1), the text between words staying put; point
/// ends after the moved word. A negative count moves it backward. A region
/// swaps with the next word (the previous word for a negative count) and the
/// count is otherwise ignored. 0 and a missing word do nothing.
pub fn transpose_words<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let n = count.unwrap_or(1);
    let mut rewrites = Vec::new();
    let mut idle = Vec::new();
    for caret in carets {
        match transpose_words_at(text, caret, n) {
            Some(rewrite) => rewrites.push(rewrite),
            None => idle.push(caret.head),
        }
    }
    rewrite_plan(text, rewrites, idle)
}

/// transpose-words with the direction reversed (M-T): the word before point
/// moves backward N words; a region swaps with the previous word.
pub fn transpose_words_reverse<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    transpose_words(text, carets, Some(count.unwrap_or(1).saturating_neg()))
}

fn transpose_words_at<T: TextSource + ?Sized>(text: &T, caret: &Caret, n: i32) -> Option<Rewrite> {
    if n == 0 {
        return None;
    }
    let head = caret.head;
    if caret.has_region() {
        let region = caret.region();
        if n > 0 {
            let word = word_after(text, region.end)?;
            return Some(rotate(text, &[region, word], true));
        }
        let word = word_before(text, region.start)?;
        return Some(rotate(text, &[word, region], false));
    }
    let moving = word_at_or_before(text, head)?;
    let mut spans = vec![moving];
    if n > 0 {
        for _ in 0..n {
            let Some(next) = word_after(text, spans.last()?.end) else {
                break;
            };
            spans.push(next);
        }
        if spans.len() < 2 {
            return None;
        }
        Some(rotate(text, &spans, true))
    } else {
        for _ in 0..n.unsigned_abs() {
            let Some(previous) = word_before(text, spans.last()?.start) else {
                break;
            };
            spans.push(previous);
        }
        if spans.len() < 2 {
            return None;
        }
        // spans[0] is the moving word; the others run backward from it.
        spans[1..].reverse();
        spans.rotate_left(1);
        Some(rotate(text, &spans, false))
    }
}

/// transpose-lines: the line above point moves past N lines (`None` is 1),
/// starting with point's line, so it swaps with the current line; a negative
/// count moves it up past N lines instead. The caret lands at the end of the
/// moved line; a point on the first line does nothing. A region is cleared.
pub fn transpose_lines<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    let n = count.unwrap_or(1);
    let mut rewrites = Vec::new();
    let mut idle = Vec::new();
    for caret in carets {
        match transpose_lines_at(text, caret.head, n) {
            Some(rewrite) => rewrites.push(rewrite),
            None => idle.push(caret.head),
        }
    }
    rewrite_plan(text, rewrites, idle)
}

fn transpose_lines_at<T: TextSource + ?Sized>(text: &T, head: usize, n: i32) -> Option<Rewrite> {
    let current = line_start(text, head);
    if n == 0 || current == 0 {
        return None;
    }
    let above_end = current - 1;
    let above_start = line_start(text, above_end);
    if n > 0 {
        let mut spans = vec![above_start..above_end];
        let mut pos = current;
        for _ in 0..n {
            let end = line_end(text, pos);
            spans.push(pos..end);
            if end >= text.len() {
                break;
            }
            pos = end + 1;
        }
        Some(rotate(text, &spans, true))
    } else {
        let mut spans = vec![above_start..above_end];
        let mut start = above_start;
        for _ in 0..n.unsigned_abs() {
            if start == 0 {
                break;
            }
            let end = start - 1;
            start = line_start(text, end);
            spans.push(start..end);
        }
        if spans.len() < 2 {
            return None;
        }
        spans[1..].reverse();
        spans.rotate_left(1);
        Some(rotate(text, &spans, false))
    }
}

/// The word containing `offset`, else the next word, else the previous one.
fn word_unit<T: TextSource + ?Sized>(text: &T, offset: usize) -> Option<Range<usize>> {
    if is_word_at(text, offset) {
        let mut start = offset;
        for c in text.reversed_chars_at(offset) {
            if !is_word_at(text, start - c.len_utf8()) {
                break;
            }
            start -= c.len_utf8();
        }
        let mut end = offset;
        for c in text.chars_at(offset) {
            if !text.is_word_char(c, end) {
                break;
            }
            end += c.len_utf8();
        }
        return Some(start..end);
    }
    word_after(text, offset).or_else(|| word_before(text, offset))
}

/// Swaps two units (`point_unit` holds point, `mark_unit` the mark) in one rewrite. The caller's
/// closure gets whether point's unit comes first, the new text and its range, and returns where
/// the caret lands inside the new text.
fn swap_units<T: TextSource + ?Sized>(
    text: &T,
    point_unit: Range<usize>,
    mark_unit: Range<usize>,
    caret_after: impl FnOnce(bool, &str, &Range<usize>) -> usize,
) -> Plan {
    if point_unit.start < mark_unit.end && mark_unit.start < point_unit.end {
        return Plan::default();
    }
    let point_first = point_unit.start < mark_unit.start;
    let (lo, hi) = if point_first {
        (point_unit, mark_unit)
    } else {
        (mark_unit, point_unit)
    };
    let new = format!(
        "{}{}{}",
        slice(text, &hi),
        slice(text, &(lo.end..hi.start)),
        slice(text, &lo)
    );
    let range = lo.start..hi.end;
    let caret = caret_after(point_first, &new, &range);
    let rewrite = Rewrite { range, new, caret };
    rewrite_plan(text, vec![rewrite], Vec::new())
}

/// transpose-words with a mark: the word at point and the word at the mark swap (the word at an
/// offset is the word containing it, else the next word, else the previous one). Point ends
/// after the word now at its site. The same word, or no second word, is an empty plan.
pub fn transpose_words_with_mark<T: TextSource + ?Sized>(
    text: &T,
    point: usize,
    mark: usize,
) -> Plan {
    let (Some(point_unit), Some(mark_unit)) = (word_unit(text, point), word_unit(text, mark))
    else {
        return Plan::default();
    };
    let mark_len = mark_unit.len();
    swap_units(text, point_unit, mark_unit, |point_first, new, _| {
        if point_first {
            // The old mark word now sits first.
            mark_len
        } else {
            new.len()
        }
    })
}

/// transpose-lines with a mark: the line at point and the line at the mark swap, without their
/// newlines. Point ends at the start of the line after point's line (the end of the text when
/// that is the last line). The same line is an empty plan.
pub fn transpose_lines_with_mark<T: TextSource + ?Sized>(
    text: &T,
    point: usize,
    mark: usize,
) -> Plan {
    let unit = |offset: usize| line_start(text, offset)..line_end(text, offset);
    let total = text.len();
    let mark_len = unit(mark).len();
    swap_units(text, unit(point), unit(mark), |point_first, new, range| {
        if point_first {
            mark_len + 1
        } else if range.end < total {
            new.len() + 1
        } else {
            new.len()
        }
    })
}
