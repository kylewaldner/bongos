use crate::text::{Caret, Place, Plan, Target, TextSource, build_plan};
use crate::words::forward_word_target;

#[derive(Clone, Copy)]
enum Mode {
    Upcase,
    Downcase,
    Capitalize,
}

fn convert<T: TextSource + ?Sized>(text: &T, old: &str, start: usize, mode: Mode) -> String {
    match mode {
        Mode::Upcase => old.to_uppercase(),
        Mode::Downcase => old.to_lowercase(),
        Mode::Capitalize => {
            let mut out = String::with_capacity(old.len());
            let mut in_word = false;
            for (i, c) in old.char_indices() {
                if text.is_word_char(c, start + i) {
                    if in_word {
                        out.extend(c.to_lowercase());
                    } else {
                        out.extend(c.to_uppercase());
                    }
                    in_word = true;
                } else {
                    out.push(c);
                    in_word = false;
                }
            }
            out
        }
    }
}

/// `previous`: the word forms act forward and move point; the previous forms
/// act before point and leave it. Negative counts flip the direction and
/// always leave point in place. A caret with a region converts the region and
/// keeps it selected.
fn case_command<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
    mode: Mode,
    previous: bool,
) -> Plan {
    let n = count.unwrap_or(1);
    let targets = carets
        .iter()
        .map(|caret| {
            if caret.has_region() {
                return Target {
                    range: caret.region(),
                    backward: false,
                    place: Place::Selection {
                        head_at_start: caret.head < caret.tail,
                    },
                };
            }
            let point = caret.head;
            let signed = if previous { n.saturating_neg() } else { n };
            let other = forward_word_target(text, point, Some(signed));
            Target {
                range: point.min(other)..point.max(other),
                backward: other < point,
                place: if signed > 0 && !previous {
                    Place::End
                } else {
                    Place::Keep(point)
                },
            }
        })
        .collect();
    build_plan(text, targets, false, &|old, start| {
        convert(text, old, start, mode)
    })
}

pub fn upcase_word<T: TextSource + ?Sized>(text: &T, carets: &[Caret], count: Option<i32>) -> Plan {
    case_command(text, carets, count, Mode::Upcase, false)
}

pub fn downcase_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    case_command(text, carets, count, Mode::Downcase, false)
}

pub fn capitalize_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    case_command(text, carets, count, Mode::Capitalize, false)
}

pub fn upcase_previous_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    case_command(text, carets, count, Mode::Upcase, true)
}

pub fn downcase_previous_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    case_command(text, carets, count, Mode::Downcase, true)
}

pub fn capitalize_previous_word<T: TextSource + ?Sized>(
    text: &T,
    carets: &[Caret],
    count: Option<i32>,
) -> Plan {
    case_command(text, carets, count, Mode::Capitalize, true)
}
