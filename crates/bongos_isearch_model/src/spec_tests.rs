use crate::spec::{CaseMode, SearchSpec};

fn spec(string: &str, regexp: bool) -> SearchSpec {
    SearchSpec {
        string: string.to_string(),
        ..SearchSpec::new(regexp)
    }
}

#[test]
fn new_session_defaults() {
    let spec = SearchSpec::new(false);
    assert_eq!(spec.case, CaseMode::Smart);
    assert!(spec.lax);
    assert!(!spec.symbol);
    assert!(!spec.regexp);
    assert!(SearchSpec::new(true).regexp);
}

#[test]
fn smart_case_follows_uppercase() {
    assert!(!spec("foo", false).case_sensitive());
    assert!(spec("Foo", false).case_sensitive());
    assert!(!spec("", false).case_sensitive());
}

#[test]
fn smart_case_ignores_escaped_letters_in_regexp() {
    assert!(!spec(r"foo\S+", true).case_sensitive());
    assert!(!spec(r"\W\B", true).case_sensitive());
    assert!(spec(r"foo\SBar", true).case_sensitive());
    assert!(spec(r"\\S", true).case_sensitive());
    assert!(spec(r"\S", false).case_sensitive());
}

#[test]
fn explicit_modes_win_over_smart_case() {
    let mut spec = spec("Foo", false);
    spec.case = CaseMode::Insensitive;
    assert!(!spec.case_sensitive());
    spec.case = CaseMode::Sensitive;
    assert!(spec.case_sensitive());
}

#[test]
fn toggle_sets_the_opposite_of_the_effective_value() {
    let mut lower = spec("foo", false);
    lower.toggle_case();
    assert_eq!(lower.case, CaseMode::Sensitive);
    lower.toggle_case();
    assert_eq!(lower.case, CaseMode::Insensitive);

    let mut upper = spec("Foo", false);
    upper.toggle_case();
    assert_eq!(upper.case, CaseMode::Insensitive);
}
