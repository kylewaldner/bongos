use crate::template::{TemplateError, parse};

fn rx(to: &str) -> crate::template::Template {
    parse(to, true).unwrap()
}

#[test]
fn group_escape_expands_on_multiline_text() {
    let groups = [Some("a\nb"), Some("a"), Some("b")];
    assert_eq!(rx("<\\2\n\\1>").expand(&groups, 0), "<b\na>");
}

#[test]
fn whole_match_escape_expands_multiline_match() {
    let groups = [Some("one\ntwo"), Some("one")];
    assert_eq!(rx("[\\&]\n\\1").expand(&groups, 0), "[one\ntwo]\none");
}

#[test]
fn counter_escape_starts_at_zero_and_counts_up() {
    assert_eq!(rx("n\\#\nm\\#").expand(&[Some("x")], 0), "n0\nm0");
    assert_eq!(rx("n\\#").expand(&[Some("x")], 3), "n3");
}

#[test]
fn double_backslash_expands_to_one_backslash() {
    assert_eq!(rx("a\\\\b").expand(&[Some("x")], 0), "a\\b");
}

#[test]
fn bad_escapes_report_the_backslash_offset() {
    assert_eq!(
        parse("ab\\x", true),
        Err(TemplateError::BadEscape { at: 2 })
    );
    assert_eq!(parse("\\0", true), Err(TemplateError::BadEscape { at: 0 }));
    assert_eq!(parse("é\\", true), Err(TemplateError::BadEscape { at: 2 }));
    assert_eq!(
        parse("\\\\\\q", true),
        Err(TemplateError::BadEscape { at: 2 })
    );
}

#[test]
fn bad_escape_message_names_the_valid_escapes() {
    let message = TemplateError::BadEscape { at: 0 }.to_string();
    assert_eq!(
        message,
        "Invalid use of `\\` in replacement text; use \\1..\\9, \\&, \\#, \\\\"
    );
}

#[test]
fn text_mode_keeps_backslashes_literal() {
    let template = parse("\\1 \\& \\x \\", false).unwrap();
    assert_eq!(template.max_group(), 0);
    assert_eq!(
        template.expand(&[Some("m"), Some("g")], 5),
        "\\1 \\& \\x \\"
    );
}

#[test]
fn max_group_is_the_highest_named_group() {
    assert_eq!(rx("\\1\\3\\2").max_group(), 3);
    assert_eq!(rx("\\&\\#plain").max_group(), 0);
    assert_eq!(rx("").max_group(), 0);
}

#[test]
fn unset_and_out_of_range_groups_expand_to_nothing() {
    let groups = [Some("m"), None, Some("two")];
    assert_eq!(rx("[\\1][\\2][\\9]").expand(&groups, 0), "[][two][]");
    assert_eq!(rx("[\\1]").expand(&[], 0), "[]");
}

#[test]
fn empty_template_expands_to_empty_string() {
    assert_eq!(rx("").expand(&[Some("m")], 0), "");
    assert_eq!(parse("", false).unwrap().expand(&[Some("m")], 0), "");
}
