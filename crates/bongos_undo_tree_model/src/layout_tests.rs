//! Tests of `layout`, on shapes built from parent lists.

use std::time::{Duration, Instant};

use crate::{NodeMark, Shape, ShapeNode, layout};

/// Builds a shape from `parents` (the root's is `None`), with state `i` edited `i` seconds after
/// a fixed base instant. Children are in index order, which is creation order.
pub fn shape_from(parents: &[Option<usize>], current: usize) -> Shape {
    let base = Instant::now();
    let mut nodes: Vec<ShapeNode> = parents
        .iter()
        .enumerate()
        .map(|(index, &parent)| ShapeNode {
            key: index as u64,
            parent,
            children: Vec::new(),
            last_edit_at: base + Duration::from_secs(index as u64),
        })
        .collect();
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = *parent {
            nodes[parent].children.push(index);
        }
    }
    Shape {
        nodes,
        current,
        entry: current,
        mark: None,
        pruned: 0,
    }
}

fn order(shape: &Shape) -> Vec<usize> {
    layout(shape).iter().filter_map(|row| row.node).collect()
}

#[test]
fn undo_tree_layout_chain_is_one_column() {
    let shape = shape_from(&[None, Some(0), Some(1), Some(2)], 3);
    let rows = layout(&shape);
    assert_eq!(order(&shape), vec![0, 1, 2, 3]);
    assert!(rows.iter().all(|row| row.glyphs.is_empty()));
}

#[test]
fn undo_tree_layout_fork_puts_older_branch_above_newest() {
    // 0 has an older child 1 (with child 2) and a newest child 3.
    let shape = shape_from(&[None, Some(0), Some(1), Some(0)], 3);
    let rows = layout(&shape);
    assert_eq!(order(&shape), vec![0, 1, 2, 3]);
    assert_eq!(rows[1].glyphs, "├ ");
    assert_eq!(rows[2].glyphs, "│ ");
    assert_eq!(rows[3].glyphs, "");
}

#[test]
fn undo_tree_layout_glyphs_at_nested_forks() {
    // 0 -> {1, 4}; 1 -> {2, 3}.
    let shape = shape_from(&[None, Some(0), Some(1), Some(1), Some(0)], 4);
    let glyphs: Vec<String> = layout(&shape).into_iter().map(|row| row.glyphs).collect();
    assert_eq!(order(&shape), vec![0, 1, 2, 3, 4]);
    assert_eq!(glyphs, vec!["", "├ ", "│ ├ ", "│ ", ""]);
}

#[test]
fn undo_tree_layout_marks_for_current_entry_both_and_marked() {
    let mut shape = shape_from(&[None, Some(0), Some(1), Some(2), Some(3), Some(4)], 1);
    shape.entry = 2;
    shape.mark = Some(3);
    let marks: Vec<NodeMark> = layout(&shape).iter().map(|row| row.mark).collect();
    assert_eq!(
        marks,
        vec![
            NodeMark::Node,
            NodeMark::Current,
            NodeMark::Entry,
            NodeMark::Marked,
            NodeMark::Node,
            NodeMark::Node,
        ]
    );
    // Both beats marked, and current beats marked.
    shape.entry = 1;
    shape.mark = Some(1);
    assert_eq!(layout(&shape)[1].mark, NodeMark::CurrentAndEntry);
    shape.entry = 2;
    assert_eq!(layout(&shape)[1].mark, NodeMark::Current);
    let symbols: String = [
        NodeMark::Node,
        NodeMark::Current,
        NodeMark::Entry,
        NodeMark::CurrentAndEntry,
        NodeMark::Marked,
    ]
    .iter()
    .map(|mark| mark.symbol())
    .collect();
    assert_eq!(symbols, "○●◆◉▣");
}

#[test]
fn undo_tree_layout_pruned_history_adds_a_note_row() {
    let mut shape = shape_from(&[None, Some(0)], 1);
    shape.pruned = 7;
    let rows = layout(&shape);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].node, None);
    assert_eq!(
        rows[0].note.as_deref(),
        Some("Older history discarded (7 nodes)")
    );
    assert_eq!(rows[1].node, Some(0));
    shape.pruned = 0;
    assert_eq!(layout(&shape).len(), 2);
}

#[test]
fn undo_tree_layout_deep_chain_does_not_recurse() {
    let parents: Vec<Option<usize>> = (0..10_000usize).map(|index| index.checked_sub(1)).collect();
    let shape = shape_from(&parents, 9_999);
    let rows = layout(&shape);
    assert_eq!(rows.len(), 10_000);
    assert!(rows.iter().all(|row| row.glyphs.is_empty()));
}
