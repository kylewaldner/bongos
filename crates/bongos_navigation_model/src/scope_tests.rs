use super::*;

#[test]
fn parses_directory_glob_and_negated_glob_tokens() {
    let parts = parse("needle -- src/main *.kt !*Test.kt");
    assert_eq!(parts.text, "needle");
    assert_eq!(
        parts.scope,
        vec![
            ScopeToken::Directory("src/main".into()),
            ScopeToken::Include("*.kt".into()),
            ScopeToken::Exclude("*Test.kt".into()),
        ]
    );
    assert_eq!(parts.include_globs(), vec!["src/main/**", "*.kt"]);
    assert_eq!(parts.exclude_globs(), vec!["*Test.kt"]);
}

#[test]
fn input_without_a_separator_has_an_empty_scope() {
    let parts = parse("just text -- not a separator--");
    assert_eq!(parts.text, "just text");
    let plain = parse("plain text");
    assert_eq!(plain.text, "plain text");
    assert!(plain.scope.is_empty());
}
