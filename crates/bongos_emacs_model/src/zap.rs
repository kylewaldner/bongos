use crate::text::{Caret, Place, Plan, Target, TextSource, build_plan};

/// Finds the `n`th occurrence of `ch` (n >= 1) at or after `from`
/// (`forward`) or ending at or before `from`. Returns its byte range.
fn find<T: TextSource + ?Sized>(
    text: &T,
    from: usize,
    ch: char,
    n: u32,
    forward: bool,
) -> Option<(usize, usize)> {
    let mut seen = 0;
    let mut pos = from;
    if forward {
        for c in text.chars_at(from) {
            if c == ch {
                seen += 1;
                if seen == n {
                    return Some((pos, pos + c.len_utf8()));
                }
            }
            pos += c.len_utf8();
        }
    } else {
        for c in text.reversed_chars_at(from) {
            let start = pos - c.len_utf8();
            if c == ch {
                seen += 1;
                if seen == n {
                    return Some((start, pos));
                }
            }
            pos = start;
        }
    }
    None
}

/// Shared by the four forms. `n` is the signed occurrence count (negative
/// searches backward); `inclusive` kills the matched character too. A region
/// is ignored and cleared. A caret with no such occurrence is left as a point;
/// when no caret finds one, the plan is empty (nothing changes, nothing is
/// cleared).
fn zap<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    n: i32,
    ch: char,
    inclusive: bool,
) -> Plan {
    if n == 0 {
        return Plan::default();
    }
    let forward = n > 0;
    let mut found = false;
    let targets = carets
        .iter()
        .map(|caret| {
            let point = caret.head;
            // The up-to forms start one character past point, as Emacs does, so the
            // target right next to point is skipped and repeating the command advances.
            let from = if inclusive {
                Some(point)
            } else if forward {
                text.chars_at(point).next().map(|c| point + c.len_utf8())
            } else {
                text.reversed_chars_at(point).next().map(|c| point - c.len_utf8())
            };
            let found_at = from.and_then(|from| find(text, from, ch, n.unsigned_abs(), forward));
            let range = match found_at {
                Some((start, end)) => {
                    found = true;
                    match (forward, inclusive) {
                        (true, true) => point..end,
                        (true, false) => point..start,
                        (false, true) => start..point,
                        (false, false) => end..point,
                    }
                }
                None => point..point,
            };
            Target {
                range,
                backward: !forward,
                place: Place::Start,
            }
        })
        .collect();
    if !found {
        return Plan::default();
    }
    build_plan(text, targets, true, &|_, _| String::new())
}

/// zap-to-char: kills from point through the Nth occurrence of `ch` after it
/// (`None` is 1). A negative count kills backward through the Nth occurrence
/// before point; 0 does nothing.
pub fn zap_to_char<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    ch: char,
) -> Plan {
    zap(text, carets, count.unwrap_or(1), ch, true)
}

/// zap-up-to-char: like [`zap_to_char`] but stops before the occurrence, and the search
/// starts one character past point (as in Emacs), so a target right after point is skipped.
pub fn zap_up_to_char<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    ch: char,
) -> Plan {
    zap(text, carets, count.unwrap_or(1), ch, false)
}

/// zap-back-to-char: [`zap_to_char`] with the direction reversed.
pub fn zap_back_to_char<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    ch: char,
) -> Plan {
    zap(text, carets, count.unwrap_or(1).saturating_neg(), ch, true)
}

/// zap-back-up-to-char: [`zap_up_to_char`] with the direction reversed.
pub fn zap_back_up_to_char<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    ch: char,
) -> Plan {
    zap(text, carets, count.unwrap_or(1).saturating_neg(), ch, false)
}
