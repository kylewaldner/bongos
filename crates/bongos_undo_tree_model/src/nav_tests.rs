//! Tests of the cursor moves in `nav`.

use crate::nav::{backward, forward, next, previous};

use crate::layout_tests::shape_from;

#[test]
fn undo_tree_forward_prefers_the_current_path() {
    // 0 -> {1, 2}; 1 -> 3. Current is 3, so forward from 0 goes to 1, not the newest child 2.
    let shape = shape_from(&[None, Some(0), Some(0), Some(1)], 3);
    assert_eq!(forward(&shape, 0), Some(1));
    assert_eq!(forward(&shape, 1), Some(3));
    // Off the current path, or at the current state, it takes the newest child.
    let shape = shape_from(&[None, Some(0), Some(0), Some(1)], 2);
    assert_eq!(forward(&shape, 0), Some(2));
    assert_eq!(forward(&shape, 1), Some(3));
    assert_eq!(forward(&shape, 3), None);
    assert_eq!(backward(&shape, 3), Some(1));
    assert_eq!(backward(&shape, 0), None);
}

#[test]
fn undo_tree_next_and_previous_walk_up_to_a_fork_and_stop_at_the_ends() {
    // 0 -> {1, 3}; 1 -> 2; 3 -> 4.
    let shape = shape_from(&[None, Some(0), Some(1), Some(0), Some(3)], 2);
    assert_eq!(next(&shape, 2), Some(3));
    assert_eq!(next(&shape, 1), Some(3));
    assert_eq!(next(&shape, 4), None);
    assert_eq!(previous(&shape, 4), Some(1));
    assert_eq!(previous(&shape, 2), None);
    // No fork above the root.
    assert_eq!(next(&shape, 0), None);
    assert_eq!(previous(&shape, 0), None);
    // A chain has no other branch.
    let chain = shape_from(&[None, Some(0), Some(1)], 2);
    assert_eq!(next(&chain, 2), None);
    assert_eq!(previous(&chain, 2), None);
}
