//! The shape of an undo history: a tree of states, each pointing at its parent and children by
//! index into one list.

use std::time::Instant;

/// One state of the undo history.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeNode {
    /// A key that identifies the state in the buffer's own history.
    pub key: u64,
    /// The index of the state this one was edited from, or `None` for the root.
    pub parent: Option<usize>,
    /// The indices of the states edited from this one, oldest first.
    pub children: Vec<usize>,
    /// When the edit that made this state happened.
    pub last_edit_at: Instant,
}

/// A whole undo history. Indices in `parent`, `children`, `current`, `entry` and `mark` point
/// into `nodes`.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// Every state, the root first.
    pub nodes: Vec<ShapeNode>,
    /// The state the buffer is in now.
    pub current: usize,
    /// The state the buffer was in when the tree was opened.
    pub entry: usize,
    /// The state the user marked, if any.
    pub mark: Option<usize>,
    /// How many of the oldest states were dropped to stay within the node limit.
    pub pruned: usize,
}
