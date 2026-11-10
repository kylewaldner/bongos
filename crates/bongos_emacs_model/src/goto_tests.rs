use crate::goto::{GotoQuery, parse};

fn position(line: usize, column: Option<usize>) -> GotoQuery {
    GotoQuery::Position { line, column }
}

#[test]
fn parses_a_line_and_a_column() {
    assert_eq!(parse("42"), position(42, None));
    assert_eq!(parse("42:7"), position(42, Some(7)));
    assert_eq!(parse(" 12 : 3 "), position(12, Some(3)));
    assert_eq!(parse("42:"), position(42, None));
    assert_eq!(parse("0"), position(0, None));
    assert_eq!(parse("0:0"), position(0, Some(0)));
}

#[test]
fn blank_input_is_empty() {
    assert_eq!(parse(""), GotoQuery::Empty);
    assert_eq!(parse("   "), GotoQuery::Empty);
}

#[test]
fn anything_else_is_invalid() {
    for input in ["x", "42:x", ":7", ":", "-3", "4 2", "1:2:3", "4.5", "é"] {
        assert_eq!(parse(input), GotoQuery::Invalid, "{input:?}");
    }
}

#[test]
fn huge_numbers_saturate() {
    assert_eq!(
        parse("99999999999999999999999999"),
        position(usize::MAX, None)
    );
}
