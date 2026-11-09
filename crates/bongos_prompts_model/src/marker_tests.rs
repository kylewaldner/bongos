use chrono::{NaiveDate, NaiveDateTime};

use crate::{MarkerLine, MarkerSyntax};

pub(crate) fn stamp(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, day)
        .unwrap()
        .and_hms_opt(hour, minute, 0)
        .unwrap()
}

#[test]
fn default_syntax_round_trips() {
    let syntax = MarkerSyntax::default();
    let pending = MarkerLine::pending(stamp(5, 13, 40));
    let sent = MarkerLine {
        created: stamp(5, 13, 40),
        sent: Some(stamp(5, 14, 2)),
    };
    assert_eq!(syntax.render(&pending), "=== prompt 2026-11-05 13:40 ===");
    assert_eq!(
        syntax.render(&sent),
        "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ==="
    );
    assert_eq!(syntax.parse(&syntax.render(&pending)), Some(pending));
    assert_eq!(syntax.parse(&syntax.render(&sent)), Some(sent));
    assert_eq!(
        syntax.parse("=== prompt 2026-11-05 13:40 ===  \r"),
        Some(pending)
    );
}

#[test]
fn configured_syntax_round_trips_and_default_is_still_read() {
    let syntax = MarkerSyntax::new("<<< ask", ">>>");
    let sent = MarkerLine {
        created: stamp(5, 9, 5),
        sent: Some(stamp(6, 23, 59)),
    };
    let line = syntax.render(&sent);
    assert_eq!(line, "<<< ask 2026-11-05 09:05 | sent 2026-11-06 23:59 >>>");
    assert_eq!(syntax.parse(&line), Some(sent));
    assert_eq!(
        syntax.parse("=== prompt 2026-11-05 09:05 ==="),
        Some(MarkerLine::pending(stamp(5, 9, 5)))
    );
    assert_eq!(MarkerSyntax::default().parse(&line), None);
}

#[test]
fn near_misses_are_body() {
    let syntax = MarkerSyntax::default();
    for line in [
        "===",
        "---",
        "# prompt",
        "=== prompt ===",
        "=== prompt 2026-11-05 13:40",
        "=== prompt 2026-11-05 13:40 ====",
        "=== prompt 2026-13-05 13:40 ===",
        "=== prompt 2026-11-31 13:40 ===",
        "=== prompt 2026-11-05 25:40 ===",
        "=== prompt 2026-11-05 13:60 ===",
        "=== prompt 2026-1-05 13:40 ===",
        "=== prompt 2026-11-05 13:40:10 ===",
        "=== prompt 2026-11-05 13:40 extra ===",
        "=== prompt 2026-11-05 13:40 | sent ===",
        "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:0 ===",
        "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 | sent 2026-11-05 14:03 ===",
        " === prompt 2026-11-05 13:40 ===",
        "===  prompt 2026-11-05 13:40 ===",
        "=== prompt +026-11-05 13:40 ===",
        "",
    ] {
        assert_eq!(syntax.parse(line), None, "{line:?}");
    }
}

#[test]
fn empty_strings_fall_back_to_the_default() {
    assert_eq!(MarkerSyntax::new("", " "), MarkerSyntax::default());
}
