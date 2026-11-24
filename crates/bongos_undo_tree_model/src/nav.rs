//! Moving the cursor around the undo tree. Each function takes the history and the state the
//! cursor is on, and returns the state to move to, or `None` when there is nowhere to go. All walks
//! are iterative and stop on a bad index or a cycle.

use crate::Shape;

/// What the caller says when `next` or `previous` finds no other branch.
pub const NO_OTHER_BRANCH: &str = "No other branch";

/// Moves toward newer states. When the cursor is a strict ancestor of the current state, this is
/// the child on the path to the current state; otherwise it is the newest (last) child. `None`
/// when the cursor has no children.
pub fn forward(shape: &Shape, cursor: usize) -> Option<usize> {
    let node = shape.nodes.get(cursor)?;
    let mut at = shape.current;
    if at != cursor {
        for _ in 0..shape.nodes.len() {
            let Some(parent) = shape.nodes.get(at).and_then(|node| node.parent) else {
                break;
            };
            if parent == cursor {
                if node.children.contains(&at) {
                    return Some(at);
                }
                break;
            }
            at = parent;
        }
    }
    node.children.last().copied()
}

/// Moves toward older states: the parent, or `None` at the root.
pub fn backward(shape: &Shape, cursor: usize) -> Option<usize> {
    shape.nodes.get(cursor)?.parent
}

/// Moves to the next branch: from the cursor, walk up to the nearest state (the cursor itself
/// included) whose parent has two or more children, and return that state's next sibling in
/// creation order. `None` when there is no such fork, or when that state is already the last
/// sibling; there is no wrap, so the walk stops at the end.
pub fn next(shape: &Shape, cursor: usize) -> Option<usize> {
    let (parent, position) = fork_position(shape, cursor)?;
    shape.nodes.get(parent)?.children.get(position + 1).copied()
}

/// Moves to the previous branch, the mirror of `next`: the previous sibling of the nearest state
/// at or above the cursor whose parent forks. `None` when there is no fork or it is already the
/// first sibling.
pub fn previous(shape: &Shape, cursor: usize) -> Option<usize> {
    let (parent, position) = fork_position(shape, cursor)?;
    shape
        .nodes
        .get(parent)?
        .children
        .get(position.checked_sub(1)?)
        .copied()
}

/// The parent of the nearest ancestor-or-self whose parent has two or more children, and the
/// position of that ancestor among those children.
fn fork_position(shape: &Shape, cursor: usize) -> Option<(usize, usize)> {
    let mut at = cursor;
    for _ in 0..shape.nodes.len() {
        let parent = shape.nodes.get(at)?.parent?;
        let siblings = &shape.nodes.get(parent)?.children;
        if siblings.len() >= 2 {
            let position = siblings.iter().position(|&child| child == at)?;
            return Some((parent, position));
        }
        at = parent;
    }
    None
}
