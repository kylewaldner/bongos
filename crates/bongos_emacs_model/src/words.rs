use crate::text::{Caret, Place, Plan, Target, TextSource, build_plan};

fn step_forward<T: TextSource + ?Sized>(text: &T, from: usize) -> usize {
    let mut pos = from;
    let mut in_word = false;
    for c in text.chars_at(from) {
        if text.is_word_char(c, pos) {
            in_word = true;
        } else if in_word {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

fn step_backward<T: TextSource + ?Sized>(text: &T, from: usize) -> usize {
    let mut pos = from;
    let mut in_word = false;
    for c in text.reversed_chars_at(from) {
        let start = pos - c.len_utf8();
        if text.is_word_char(c, start) {
            in_word = true;
        } else if in_word {
            break;
        }
        pos = start;
    }
    pos
}

/// Offset after moving `count` words (`None` is 1) from `offset`. Negative
/// counts move backward, 0 stays; the buffer ends stop large counts.
pub fn forward_word_target<T: TextSource + ?Sized>(
    text: &T,
    offset: usize,
    count: Option<i32>,
) -> usize {
    let n = count.unwrap_or(1);
    let forward = n >= 0;
    let mut pos = offset;
    for _ in 0..n.unsigned_abs() {
        let next = if forward {
            step_forward(text, pos)
        } else {
            step_backward(text, pos)
        };
        if next == pos {
            break;
        }
        pos = next;
    }
    pos
}

/// Like [`forward_word_target`] with the direction reversed.
pub fn backward_word_target<T: TextSource + ?Sized>(
    text: &T,
    offset: usize,
    count: Option<i32>,
) -> usize {
    forward_word_target(text, offset, Some(count.unwrap_or(1).saturating_neg()))
}

fn move_carets<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    select: bool,
    backward: bool,
) -> Vec<Caret> {
    let mut moved: Vec<Caret> = carets
        .iter()
        .map(|caret| {
            let head = if backward {
                backward_word_target(text, caret.head, count)
            } else {
                forward_word_target(text, caret.head, count)
            };
            Caret::new(head, if select { caret.tail } else { head })
        })
        .collect();
    moved.sort_by_key(|c| (c.head, c.tail));
    moved.dedup();
    moved
}

/// forward-word for every caret. `select` extends the region (keeps the tail);
/// otherwise the caret collapses at the target. Carets that land together merge.
pub fn forward_word_carets<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    select: bool,
) -> Vec<Caret> {
    move_carets(text, carets, count, select, false)
}

/// backward-word for every caret; see [`forward_word_carets`].
pub fn backward_word_carets<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    select: bool,
) -> Vec<Caret> {
    move_carets(text, carets, count, select, true)
}

fn kill<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    backward: bool,
) -> Plan {
    let n = count.unwrap_or(1);
    let n = if backward { n.saturating_neg() } else { n };
    let targets = carets
        .iter()
        .map(|caret| {
            if caret.has_region() {
                // A region is killed whole; the count is ignored.
                return Target {
                    range: caret.region(),
                    backward: false,
                    place: Place::Start,
                };
            }
            let target = forward_word_target(text, caret.head, Some(n));
            Target {
                range: caret.head.min(target)..caret.head.max(target),
                backward: target < caret.head,
                place: Place::Start,
            }
        })
        .collect();
    build_plan(text, targets, true, &|_, _| String::new())
}

/// kill-word: kills to the end of the Nth word after each caret (negative N
/// kills backward, 0 kills nothing). A caret with a region kills the region.
pub fn kill_word<T: TextSource + ?Sized>(text: &T, carets: &[Caret], count: Option<i32>) -> Plan {
    kill(text, carets, count, false)
}

/// backward-kill-word: kill-word with the direction reversed.
pub fn backward_kill_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    kill(text, carets, count, true)
}
