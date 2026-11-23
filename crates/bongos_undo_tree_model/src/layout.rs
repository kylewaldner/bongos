//! Turns a `Shape` into the rows of the undo tree view, one per state, top to bottom.

use std::time::Instant;

use crate::Shape;

/// The symbol a state is drawn with. When several apply, the precedence is: a state that is both
/// current and the entry state is `CurrentAndEntry`; then `Current`; then `Entry`; then `Marked`;
/// otherwise `Node`. So marking the current or entry state does not hide that it is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeMark {
    /// An ordinary state, `○`.
    Node,
    /// The state the buffer is in now, `●`.
    Current,
    /// The state the tree was opened at, `◆`.
    Entry,
    /// Both the current and the entry state, `◉`.
    CurrentAndEntry,
    /// A state the user marked that is neither current nor entry, `▣`.
    Marked,
}

impl NodeMark {
    /// The character that draws this mark.
    pub fn symbol(self) -> char {
        match self {
            NodeMark::Node => '○',
            NodeMark::Current => '●',
            NodeMark::Entry => '◆',
            NodeMark::CurrentAndEntry => '◉',
            NodeMark::Marked => '▣',
        }
    }
}

/// One line of the undo tree view.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The index of the state in `Shape::nodes`, or `None` for the note row.
    pub node: Option<usize>,
    /// The branch columns drawn before the mark (see `layout`). Empty for the note row.
    pub glyphs: String,
    /// The mark of the state. `NodeMark::Node` for the note row, which draws no mark.
    pub mark: NodeMark,
    /// When the state was made, for `relative_time`; `None` for the note row.
    pub last_edit_at: Option<Instant>,
    /// The text of the note row; `None` for a state row.
    pub note: Option<String>,
}

/// Lays the history out as rows, one per state, in pre-order, without recursion.
///
/// At a fork the older children's subtrees come first, each in a new column one step to the
/// right, and the newest child continues in the parent's column below them. So the newest branch
/// ends at the bottom and an older branch sits above it. Each row's `glyphs` holds one `"│ "` per
/// column to the left of the state's own column (the parent columns that continue below), and the
/// first row of an older branch replaces the last of them with `"├ "`, joining it to its fork.
/// The mark follows the glyphs. For a root `r` with an older child `a` and a newest child `b`,
/// where `a` has two children `c` (older) and `d`:
///
/// ```text
/// ○ r
/// ├ ○ a
/// │ ├ ○ c
/// │ ○ d
/// ○ b
/// ```
///
/// When `shape.pruned` is greater than zero, a note row `Older history discarded (N nodes)`
/// comes first. Indices that are out of range, and states reached twice, are skipped.
pub fn layout(shape: &Shape) -> Vec<Row> {
    let mut rows = Vec::with_capacity(shape.nodes.len() + 1);
    if shape.pruned > 0 {
        rows.push(Row {
            node: None,
            glyphs: String::new(),
            mark: NodeMark::Node,
            last_edit_at: None,
            note: Some(format!("Older history discarded ({} nodes)", shape.pruned)),
        });
    }
    let mut seen = vec![false; shape.nodes.len()];
    // (state, branch depth, first row of an older branch)
    let mut stack: Vec<(usize, usize, bool)> = vec![(0, 0, false)];
    while let Some((index, depth, branch_start)) = stack.pop() {
        let Some(node) = shape.nodes.get(index) else {
            continue;
        };
        match seen.get_mut(index) {
            Some(flag) if !*flag => *flag = true,
            _ => continue,
        }
        let mut glyphs = "│ ".repeat(depth);
        if branch_start {
            glyphs.truncate(glyphs.len().saturating_sub("│ ".len()));
            glyphs.push_str("├ ");
        }
        rows.push(Row {
            node: Some(index),
            glyphs,
            mark: mark_of(shape, index),
            last_edit_at: Some(node.last_edit_at),
            note: None,
        });
        // Pushed in reverse so that the oldest child is popped first and the newest last.
        if let Some((&newest, older)) = node.children.split_last() {
            stack.push((newest, depth, false));
            for &child in older.iter().rev() {
                stack.push((child, depth + 1, true));
            }
        }
    }
    rows
}

fn mark_of(shape: &Shape, index: usize) -> NodeMark {
    let current = index == shape.current;
    let entry = index == shape.entry;
    match (current, entry) {
        (true, true) => NodeMark::CurrentAndEntry,
        (true, false) => NodeMark::Current,
        (false, true) => NodeMark::Entry,
        (false, false) if shape.mark == Some(index) => NodeMark::Marked,
        (false, false) => NodeMark::Node,
    }
}
