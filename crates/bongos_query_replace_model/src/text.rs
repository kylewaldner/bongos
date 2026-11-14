//! The texts a session shows: the decision prompt, the counter, the help
//! table, notices and the end message.

use crate::session::{End, Notice, Reason};

/// How a newline shows in the one-line prompt, as in isearch.
pub const NEWLINE_MARK: char = '⏎';

/// The key table, one `(keys, effect)` row per command.
pub const HELP_ROWS: &[(&str, &str)] = &[
    ("y, SPC", "Replace this match, advance to next"),
    ("n, DEL", "Skip, advance to next"),
    ("!", "Replace this and every remaining match without asking"),
    (".", "Replace this match, then end the session"),
    (
        ",",
        "Replace this match, show the result, wait; next key advances",
    ),
    (
        "^",
        "Go back to the previous replaced match and show it; repeats",
    ),
    (
        "u",
        "Undo the last replacement, stay on that match; repeats",
    ),
    (
        "e",
        "Edit the replacement text; RET resumes with the new text",
    ),
    ("q, RET", "End the session; point stays at current match"),
    ("C-g", "End the session like q; replacements made stay"),
    ("C-l", "Recenter the current match, session continues"),
    ("?, C-h", "Show the key list in the minibuffer"),
    (
        "any other key",
        "Ends the session and re-runs the key, as Emacs does",
    ),
];

fn one_line(text: &str) -> String {
    text.replace('\n', &NEWLINE_MARK.to_string())
}

/// `Query replacing fetchUser with loadUser: (y/n/!/./,/^/q/?)`, with
/// `regexp` after `replacing` in regexp mode. `replacement` is the text as it
/// will be inserted for the current match.
pub fn decision_prompt(from: &str, replacement: &str, regexp: bool) -> String {
    let lead = if regexp {
        "Query replacing regexp"
    } else {
        "Query replacing"
    };
    format!(
        "{lead} {} with {}: (y/n/!/./,/^/q/?)",
        one_line(from),
        one_line(replacement)
    )
}

/// `4/23` for the 1-based `index` of the current match; `4/10000+` when the
/// count stopped at the cap `total`; empty while the total is unknown.
pub fn counter(index: usize, total: Option<usize>, capped: bool) -> String {
    match total {
        None => String::new(),
        Some(total) if capped => format!("{index}/{total}+"),
        Some(total) => format!("{index}/{total}"),
    }
}

/// The help table as lines, keys left-aligned in one column.
pub fn help() -> Vec<String> {
    let width = HELP_ROWS
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);
    HELP_ROWS
        .iter()
        .map(|(keys, effect)| format!("{keys:<width$}  {effect}"))
        .collect()
}

pub fn notice(notice: Notice) -> &'static str {
    match notice {
        Notice::NoPrevious => "No previous replacement",
        Notice::NothingToUndo => "Nothing to undo",
    }
}

/// The echo-area message when a session ends: `Nothing to replace` when no
/// match was found, else `Replaced 12 occurrences` or `Replaced 12, skipped
/// 3`, with ` (interrupted)` after a `C-g` stopped a `!`.
pub fn end_message(end: &End) -> String {
    if end.reason == Reason::NoMatch {
        return "Nothing to replace".to_string();
    }
    let counts = end.counts;
    let mut message = if counts.skipped > 0 {
        format!("Replaced {}, skipped {}", counts.replaced, counts.skipped)
    } else if counts.replaced == 1 {
        "Replaced 1 occurrence".to_string()
    } else {
        format!("Replaced {} occurrences", counts.replaced)
    };
    if end.reason == Reason::Interrupted {
        message.push_str(" (interrupted)");
    }
    message
}
