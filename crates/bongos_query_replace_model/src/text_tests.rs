use crate::session::{Counts, End, Notice, Reason};
use crate::text::{HELP_ROWS, counter, decision_prompt, end_message, help, notice};

fn end(reason: Reason, replaced: usize, skipped: usize) -> End {
    End {
        reason,
        counts: Counts { replaced, skipped },
    }
}

#[test]
fn decision_prompt_names_both_strings() {
    assert_eq!(
        decision_prompt("fetchUser", "loadUser", false),
        "Query replacing fetchUser with loadUser: (y/n/!/./,/^/q/?)"
    );
}

#[test]
fn decision_prompt_says_regexp_in_regexp_mode() {
    assert_eq!(
        decision_prompt("(\\w+)Service", "UserRepository", true),
        "Query replacing regexp (\\w+)Service with UserRepository: (y/n/!/./,/^/q/?)"
    );
}

#[test]
fn decision_prompt_shows_newlines_as_marks() {
    assert_eq!(
        decision_prompt("a\nb", "c\n", false),
        "Query replacing a⏎b with c⏎: (y/n/!/./,/^/q/?)"
    );
}

#[test]
fn decision_prompt_with_empty_replacement() {
    assert_eq!(
        decision_prompt("foo", "", false),
        "Query replacing foo with : (y/n/!/./,/^/q/?)"
    );
}

#[test]
fn counter_known_capped_and_unknown() {
    assert_eq!(counter(4, Some(23), false), "4/23");
    assert_eq!(counter(4, Some(10000), true), "4/10000+");
    assert_eq!(counter(4, None, false), "");
    assert_eq!(counter(1, Some(1), false), "1/1");
}

#[test]
fn end_message_forms() {
    assert_eq!(
        end_message(&end(Reason::NoMatch, 0, 0)),
        "Nothing to replace"
    );
    assert_eq!(
        end_message(&end(Reason::NoMore, 1, 0)),
        "Replaced 1 occurrence"
    );
    assert_eq!(
        end_message(&end(Reason::BatchDone, 12, 0)),
        "Replaced 12 occurrences"
    );
    assert_eq!(
        end_message(&end(Reason::Exit, 12, 3)),
        "Replaced 12, skipped 3"
    );
    assert_eq!(
        end_message(&end(Reason::Exit, 0, 0)),
        "Replaced 0 occurrences"
    );
    assert_eq!(
        end_message(&end(Reason::NoMore, 0, 2)),
        "Replaced 0, skipped 2"
    );
}

#[test]
fn end_message_marks_an_interrupted_batch() {
    assert_eq!(
        end_message(&end(Reason::Interrupted, 12, 0)),
        "Replaced 12 occurrences (interrupted)"
    );
    assert_eq!(
        end_message(&end(Reason::Interrupted, 12, 3)),
        "Replaced 12, skipped 3 (interrupted)"
    );
    assert_eq!(
        end_message(&end(Reason::Aborted, 2, 0)),
        "Replaced 2 occurrences"
    );
}

#[test]
fn help_lists_the_key_table_in_order() {
    let lines = help();
    assert_eq!(lines.len(), HELP_ROWS.len());
    assert_eq!(
        lines[0],
        "y, SPC         Replace this match, advance to next"
    );
    assert_eq!(
        lines[12],
        "any other key  Ends the session and re-runs the key, as Emacs does"
    );
    let keys: Vec<&str> = HELP_ROWS.iter().map(|(keys, _)| *keys).collect();
    assert_eq!(
        keys,
        [
            "y, SPC",
            "n, DEL",
            "!",
            ".",
            ",",
            "^",
            "u",
            "e",
            "q, RET",
            "C-g",
            "C-l",
            "?, C-h",
            "any other key"
        ]
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Undo the last replacement, stay on that match; repeats"))
    );
}

#[test]
fn notices() {
    assert_eq!(notice(Notice::NoPrevious), "No previous replacement");
    assert_eq!(notice(Notice::NothingToUndo), "Nothing to undo");
}
