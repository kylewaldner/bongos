use crate::case::{Shape, apply, preserves_case, shape, transfer};

#[test]
fn shape_of_plain_words() {
    assert_eq!(shape("foo"), Shape::AsIs);
    assert_eq!(shape("Foo"), Shape::Capitalized);
    assert_eq!(shape("FOO"), Shape::Upper);
}

#[test]
fn shape_of_single_capital_letter_is_upper() {
    assert_eq!(shape("X"), Shape::Upper);
    assert_eq!(shape("x"), Shape::AsIs);
}

#[test]
fn shape_of_camel_case_follows_word_initials() {
    assert_eq!(shape("fetchUser"), Shape::AsIs);
    assert_eq!(shape("FetchUser"), Shape::Capitalized);
    assert_eq!(shape("fOO"), Shape::AsIs);
}

#[test]
fn shape_of_several_words() {
    assert_eq!(shape("FOO BAR"), Shape::Upper);
    assert_eq!(shape("Foo Bar"), Shape::Capitalized);
    assert_eq!(shape("foo Bar"), Shape::AsIs);
}

#[test]
fn shape_treats_a_digit_initial_as_neutral_but_not_uppercase() {
    assert_eq!(shape("1"), Shape::AsIs);
    assert_eq!(shape("123"), Shape::AsIs);
    assert_eq!(shape("A1"), Shape::Upper);
    assert_eq!(shape("Foo 1"), Shape::AsIs);
    assert_eq!(shape("FOO 1"), Shape::Upper);
}

#[test]
fn shape_of_non_ascii_text() {
    assert_eq!(shape("ÉCOLE"), Shape::Upper);
    assert_eq!(shape("École"), Shape::Capitalized);
    assert_eq!(shape("école"), Shape::AsIs);
}

#[test]
fn shape_of_empty_and_letterless_text_is_as_is() {
    assert_eq!(shape(""), Shape::AsIs);
    assert_eq!(shape("-- ()"), Shape::AsIs);
}

#[test]
fn apply_upper_upcases_everything() {
    assert_eq!(apply(Shape::Upper, "load user"), "LOAD USER");
}

#[test]
fn apply_capitalized_upcases_only_word_initials() {
    assert_eq!(apply(Shape::Capitalized, "loadUser"), "LoadUser");
    assert_eq!(apply(Shape::Capitalized, "load user"), "Load User");
}

#[test]
fn apply_as_is_returns_the_replacement() {
    assert_eq!(apply(Shape::AsIs, "loadUser"), "loadUser");
}

#[test]
fn transfer_capitalizes_and_upcases_when_enabled() {
    assert_eq!(transfer(true, "Listener", "handler"), "Handler");
    assert_eq!(transfer(true, "LISTENER", "handler"), "HANDLER");
    assert_eq!(transfer(true, "listener", "handler"), "handler");
}

#[test]
fn transfer_leaves_the_replacement_alone_when_disabled() {
    assert_eq!(transfer(false, "Listener", "handler"), "handler");
    assert_eq!(transfer(false, "LISTENER", "handler"), "handler");
}

#[test]
fn preserves_case_truth_table() {
    assert!(!preserves_case(true, "listener"));
    assert!(!preserves_case(true, "Listener"));
    assert!(!preserves_case(false, "Listener"));
    assert!(preserves_case(false, "listener"));
    assert!(preserves_case(false, "list_1"));
}
