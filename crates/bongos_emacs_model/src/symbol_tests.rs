use crate::symbol::*;

fn jump(text: &str, offset: usize, direction: Direction) -> Option<SymbolJump> {
    next_occurrence(text, offset, direction)
}

#[test]
fn longer_words_containing_the_symbol_are_skipped() {
    let text = "downloadExportUseCase downloadExportUseCaseTest downloadExportUseCase";
    let first = jump(text, 3, Direction::Forward).unwrap();
    assert_eq!(first.range, 48..69);
    assert_eq!(first.caret, 51);
    assert!(!first.wrapped);
}

#[test]
fn dots_end_a_word() {
    let text = "a.foo + foo";
    let jump = jump(text, 3, Direction::Forward).unwrap();
    assert_eq!(jump.range, 8..11);
    assert_eq!(jump.caret, 9);
}

#[test]
fn caret_is_clamped_into_the_occurrence() {
    assert_eq!(caret_within(&(10..13), 2), 12);
    assert_eq!(caret_within(&(10..13), 3), 13);
    assert_eq!(caret_within(&(10..13), 9), 13);
    let text = "foo foo";
    let jump = jump(text, 3, Direction::Forward).unwrap();
    assert_eq!(jump.range, 4..7);
    assert_eq!(jump.caret, 7);
}

#[test]
fn forward_wraps_past_the_end() {
    let text = "foo bar foo";
    let jump = jump(text, 9, Direction::Forward).unwrap();
    assert_eq!(jump.range, 0..3);
    assert_eq!(jump.caret, 1);
    assert!(jump.wrapped);
}

#[test]
fn backward_wraps_past_the_start() {
    let text = "foo bar foo";
    let jump = jump(text, 1, Direction::Backward).unwrap();
    assert_eq!(jump.range, 8..11);
    assert_eq!(jump.caret, 9);
    assert!(jump.wrapped);
    let back = self::jump(text, 9, Direction::Backward).unwrap();
    assert_eq!(back.range, 0..3);
    assert!(!back.wrapped);
}

#[test]
fn a_single_occurrence_wraps_onto_itself() {
    let text = "alpha beta";
    for direction in [Direction::Forward, Direction::Backward] {
        let jump = jump(text, 2, direction).unwrap();
        assert_eq!(jump.range, 0..5);
        assert_eq!(jump.caret, 2);
        assert!(jump.wrapped);
    }
}

#[test]
fn no_symbol_at_point() {
    assert_eq!(symbol_at("", 0), None);
    assert_eq!(symbol_at("foo  bar", 4), None);
    assert_eq!(symbol_at("foo + bar", 4), None);
    assert_eq!(symbol_at("(foo)", 0), None);
    assert_eq!(jump("a  b", 2, Direction::Forward), None);
    assert_eq!(jump("", 0, Direction::Backward), None);
}

#[test]
fn symbol_at_covers_inside_start_and_end() {
    let text = "foo_1 bar";
    assert_eq!(symbol_at(text, 0), Some(0..5));
    assert_eq!(symbol_at(text, 3), Some(0..5));
    assert_eq!(symbol_at(text, 5), Some(0..5));
    assert_eq!(symbol_at(text, 6), Some(6..9));
    assert_eq!(symbol_at(text, 9), Some(6..9));
}

#[test]
fn caret_at_a_words_end_takes_that_word() {
    let text = "foo.bar foo";
    assert_eq!(symbol_at(text, 3), Some(0..3));
    let jump = jump(text, 3, Direction::Forward).unwrap();
    assert_eq!(jump.range, 8..11);
    assert_eq!(jump.caret, 11);
}

#[test]
fn non_ascii_letters_are_symbol_characters() {
    let text = "été naïve été";
    assert_eq!(symbol_at(text, 2), Some(0..5));
    let jump = jump(text, 2, Direction::Forward).unwrap();
    assert_eq!(jump.range, 13..18);
    assert_eq!(jump.caret, 15);
}

#[test]
fn occurrences_respect_within_and_limit() {
    let text = "été naïve été naïve été";
    let all = occurrences(text, "été", 0..text.len(), 10);
    assert_eq!(all, vec![0..5, 13..18, 26..31]);
    assert_eq!(
        occurrences(text, "été", 0..text.len(), 2),
        vec![0..5, 13..18]
    );
    assert_eq!(
        occurrences(text, "été", 6..text.len(), 10),
        vec![13..18, 26..31]
    );
    assert_eq!(occurrences(text, "été", 0..18, 10), vec![0..5, 13..18]);
    assert_eq!(occurrences(text, "été", 0..17, 10), vec![0..5]);
    assert_eq!(occurrences(text, "naïve", 0..1000, 10), vec![6..12, 19..25]);
    assert_eq!(
        occurrences(text, "été", 0..text.len(), 0),
        Vec::<std::ops::Range<usize>>::new()
    );
}

#[test]
fn occurrences_are_whole_words_only() {
    let text = "foo foobar barfoo a.foo foo_x";
    assert_eq!(
        occurrences(text, "foo", 0..text.len(), 10),
        vec![0..3, 20..23]
    );
}
