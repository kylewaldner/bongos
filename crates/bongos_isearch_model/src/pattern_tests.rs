use crate::pattern::{compile, escape};
use crate::spec::{CaseMode, SearchSpec};

fn spec(string: &str) -> SearchSpec {
    SearchSpec {
        string: string.to_string(),
        ..SearchSpec::new(false)
    }
}

#[test]
fn plain_text_is_a_text_query() {
    let compiled = compile(&spec("foo.bar"));
    assert_eq!(compiled.pattern, "foo.bar");
    assert!(!compiled.regex);
    assert!(!compiled.whole_word);
    assert!(!compiled.case_sensitive);
}

#[test]
fn lax_space_run_becomes_a_lazy_gap() {
    let compiled = compile(&spec("foo bar"));
    assert!(compiled.regex);
    assert_eq!(compiled.pattern, "foo.*?bar");
    let compiled = compile(&spec("a.b   c+d"));
    assert_eq!(compiled.pattern, r"a\.b.*?c\+d");
}

#[test]
fn lax_pattern_has_no_newline_text() {
    let compiled = compile(&spec("foo bar"));
    assert!(!compiled.pattern.contains('\n'));
    assert!(!compiled.pattern.contains("\\n"));
    assert!(!compiled.pattern.contains("[^"));
}

#[test]
fn leading_and_trailing_spaces_stay_literal() {
    let compiled = compile(&spec("foo "));
    assert!(!compiled.regex);
    assert_eq!(compiled.pattern, "foo ");
    let compiled = compile(&spec(" foo"));
    assert_eq!(compiled.pattern, " foo");
    let compiled = compile(&spec("  foo bar  "));
    assert!(compiled.regex);
    assert_eq!(compiled.pattern, "  foo.*?bar  ");
}

#[test]
fn lax_off_keeps_the_string() {
    let mut spec = spec("foo bar");
    spec.lax = false;
    let compiled = compile(&spec);
    assert!(!compiled.regex);
    assert_eq!(compiled.pattern, "foo bar");
}

#[test]
fn regexp_is_never_rewritten() {
    let mut spec = spec("foo bar.*");
    spec.regexp = true;
    let compiled = compile(&spec);
    assert!(compiled.regex);
    assert_eq!(compiled.pattern, "foo bar.*");
}

#[test]
fn symbol_is_an_escaped_whole_word_regex() {
    let mut spec = spec("my_var.x");
    spec.symbol = true;
    let compiled = compile(&spec);
    assert!(compiled.regex);
    assert!(compiled.whole_word);
    assert_eq!(compiled.pattern, r"my_var\.x");
}

#[test]
fn case_follows_the_spec() {
    assert!(compile(&spec("Foo")).case_sensitive);
    let mut spec = spec("Foo");
    spec.case = CaseMode::Insensitive;
    assert!(!compile(&spec).case_sensitive);
}

#[test]
fn typed_backslash_then_n_stays_literal() {
    let mut spec = spec("a\\n b");
    spec.lax = true;
    let compiled = compile(&spec);
    assert_eq!(compiled.pattern, "a\\\\n.*?b");
    assert_eq!(escape("\\n"), "\\\\n");
}

#[test]
fn escape_covers_the_regex_metacharacters() {
    for character in "\\.+*?()|[]{}^$#&-~".chars() {
        assert_eq!(escape(&character.to_string()), format!("\\{character}"));
    }
    assert_eq!(escape("abc 123_é\n"), "abc 123_é\n");
}
