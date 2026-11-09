use crate::{common_prefix, count_text, texts};

#[test]
fn common_prefix_of_several_items() {
    assert_eq!(common_prefix(["foobar", "foobaz", "foo"]), "foo");
    assert_eq!(common_prefix(["foobar", "foobaz"]), "fooba");
    assert_eq!(common_prefix(["abc", "xyz"]), "");
}

#[test]
fn common_prefix_edge_cases() {
    assert_eq!(common_prefix(std::iter::empty::<&str>()), "");
    assert_eq!(common_prefix(["only"]), "only");
    assert_eq!(common_prefix(["same", "same"]), "same");
    assert_eq!(common_prefix(["", "abc"]), "");
    assert_eq!(common_prefix(["abc", ""]), "");
    assert_eq!(common_prefix(["abc", "abcdef"]), "abc");
    assert_eq!(common_prefix(["abcdef", "abc"]), "abc");
}

#[test]
fn common_prefix_is_case_sensitive_and_char_based() {
    assert_eq!(common_prefix(["Foo", "foo"]), "");
    assert_eq!(common_prefix(["fooBar", "foobar"]), "foo");
    assert_eq!(common_prefix(["café", "cafè"]), "caf");
    assert_eq!(common_prefix(["日本語", "日本人"]), "日本");
    assert_eq!(common_prefix(["日本語", "ab"]), "");
}

#[test]
fn count_text_is_matches_over_candidates() {
    assert_eq!(count_text(3, 417), "3/417");
    assert_eq!(count_text(0, 0), "0/0");
    assert_eq!(count_text(100_000, 100_000), "100000/100000");
}

#[test]
fn texts_keep_their_order() {
    let list = texts(["b", "a", "c"]);
    assert_eq!(
        list.iter().map(|text| &**text).collect::<Vec<_>>(),
        vec!["b", "a", "c"]
    );
}
