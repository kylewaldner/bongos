use crate::matching::{char_matches, word_starts};

#[test]
fn lowercase_matches_both_cases_uppercase_only_itself() {
    assert_eq!(char_matches("a", "aAb"), vec![0..1, 1..2]);
    assert_eq!(char_matches("A", "aAb"), vec![1..2]);
    assert_eq!(char_matches("ab", "aBAb"), vec![0..2, 2..4]);
    assert_eq!(char_matches("Ab", "aBAb"), vec![2..4]);
    assert_eq!(char_matches("1", "a1"), vec![1..2]);
    assert!(char_matches("", "abc").is_empty());
}

#[test]
fn overlapping_matches_are_all_found() {
    assert_eq!(char_matches("aa", "aaaa"), vec![0..2, 1..3, 2..4]);
    assert_eq!(char_matches("é", "aÉé"), vec![1..3, 3..5]);
}

#[test]
fn word_starts_skip_mid_word_and_use_before() {
    assert_eq!(
        word_starts('f', "foo_bar fun Foo.f", None),
        vec![0, 8, 12, 16]
    );
    assert_eq!(word_starts('b', "foo_bar", None), Vec::<usize>::new());
    assert_eq!(word_starts('f', "foo", Some('x')), Vec::<usize>::new());
    assert_eq!(word_starts('f', "foo", Some(' ')), vec![0]);
    assert_eq!(word_starts('F', "foo Foo", None), vec![4]);
}
