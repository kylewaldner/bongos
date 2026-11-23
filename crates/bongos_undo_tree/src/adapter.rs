//! Turns a buffer's undo history into the plain [`Shape`] that the tree's layout and navigation
//! work on.

use std::time::Instant;

use bongos_undo_tree_model::{Shape, ShapeNode};
use collections::HashMap;
use text::{UndoNodeId, UndoTree};

/// A [`Shape`] together with the undo node behind each of its indices.
#[derive(Clone, Debug)]
pub struct TreeShape {
    pub shape: Shape,
    ids: Vec<UndoNodeId>,
    indices: HashMap<UndoNodeId, usize>,
}

impl TreeShape {
    /// The undo node at an index of the shape.
    pub fn id(&self, index: usize) -> Option<UndoNodeId> {
        self.ids.get(index).copied()
    }

    /// The index of an undo node in the shape.
    pub fn index(&self, id: UndoNodeId) -> Option<usize> {
        self.indices.get(&id).copied()
    }
}

/// The key a node has in a [`Shape`]: its transaction's lamport timestamp, 0 for the root.
pub fn node_key(id: UndoNodeId) -> u64 {
    id.0.as_u64()
}

/// Builds the shape of `tree`. Nodes are listed in pre-order, the root first and children
/// oldest first. `entry` is the node the buffer was in when the tree was opened; when it is not
/// in the tree the current node stands for it. The root has no edit, so it gets `root_time`.
/// The walk uses an explicit stack, so a very deep history cannot overflow the call stack.
pub fn shape_of(tree: &UndoTree, entry: UndoNodeId, root_time: Instant) -> TreeShape {
    let mut nodes = Vec::with_capacity(tree.len());
    let mut ids = Vec::with_capacity(tree.len());
    let mut indices = HashMap::default();
    let mut stack = vec![(tree.root(), None::<usize>)];
    while let Some((id, parent)) = stack.pop() {
        let index = nodes.len();
        let last_edit_at = tree
            .node(id)
            .and_then(|node| node.entry())
            .map_or(root_time, |entry| entry.last_edit_at());
        nodes.push(ShapeNode {
            key: node_key(id),
            parent,
            children: Vec::new(),
            last_edit_at,
        });
        if let Some(parent) = parent {
            let parent_node: &mut ShapeNode = &mut nodes[parent];
            parent_node.children.push(index);
        }
        ids.push(id);
        indices.insert(id, index);
        for child in tree.children(id).into_iter().rev() {
            stack.push((child, Some(index)));
        }
    }
    let current = indices.get(&tree.current()).copied().unwrap_or(0);
    let entry = indices.get(&entry).copied().unwrap_or(current);
    TreeShape {
        shape: Shape {
            nodes,
            current,
            entry,
            mark: None,
            pruned: tree.pruned(),
        },
        ids,
        indices,
    }
}
