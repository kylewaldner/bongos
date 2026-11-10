use crate::comment::covers_whole_lines;

fn covers(text: &str, region: std::ops::Range<usize>) -> bool {
    covers_whole_lines(text, region)
}

#[test]
fn a_region_over_a_whole_line_covers_it() {
    let text = "  foo\n  bar\nbaz";
    assert!(covers(text, 2..5));
    assert!(covers(text, 0..5));
    assert!(covers(text, 0..6));
    assert!(covers(text, 2..11));
    assert!(covers(text, 0..text.len()));
}

#[test]
fn a_region_inside_a_line_does_not() {
    let text = "  foo\n  bar\nbaz";
    assert!(!covers(text, 3..5));
    assert!(!covers(text, 2..4));
    assert!(!covers(text, 3..4));
    assert!(!covers(text, 2..9));
    assert!(!covers(text, 3..11));
}

#[test]
fn trailing_whitespace_and_blank_lines_do_not_count() {
    assert!(covers("foo  \nbar", 0..3));
    assert!(covers("foo \t\nbar", 0..3));
    assert!(covers("a\n\nb", 0..4));
    assert!(!covers("a\n\nb", 1..4));
    assert!(covers("a\n\nb", 0..3));
    assert!(!covers("a\n\nb c", 0..5));
}

#[test]
fn an_empty_region_counts_as_its_line() {
    assert!(covers("foo bar", 3..3));
    assert!(covers("", 0..0));
}

#[test]
fn a_region_ending_after_a_newline_ends_on_the_line_before() {
    assert!(covers("foo\nbar", 0..4));
    assert!(!covers("foo\nbar", 1..4));
}

#[test]
fn non_ascii_text_counts_bytes() {
    assert!(covers("é\nß", 0..2));
    assert!(covers("é\nß", 0..5));
    assert!(!covers("éé", 2..4));
}
