use super::*;

#[test]
fn the_current_item_is_last_and_the_previous_item_is_first() {
    let items = vec![("a", 5), ("b", 9), ("c", 1), ("d", 7)];
    assert_eq!(order(items.clone(), Some("b")), vec!["d", "a", "c", "b"]);
    assert_eq!(order(items, None), vec!["b", "d", "a", "c"]);
}
