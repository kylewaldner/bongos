//! Tests of node surgery on the undo tree (push, push empty, forget, merge and
//! out-of-order undo) and of the lookups scoped to the undo and redo views,
//! against the two-stack oracle of `undo_tree_tests`.

use super::*;
use crate::undo_tree_tests::{Pair, node, reachable};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::collections::BTreeSet;
use std::time::Duration;

/// Every reachable node with its parent, children and active child, sorted
/// by id, followed by the current node.
type Snapshot = (
    Vec<(
        UndoNodeId,
        Option<UndoNodeId>,
        Vec<UndoNodeId>,
        Option<UndoNodeId>,
    )>,
    UndoNodeId,
);

fn snapshot(tree: &UndoTree) -> Snapshot {
    let mut nodes = reachable(tree)
        .into_iter()
        .map(|id| {
            (
                id,
                tree.parent(id),
                tree.children(id),
                tree.active_child(id),
            )
        })
        .collect::<Vec<_>>();
    nodes.sort();
    (nodes, tree.current())
}

/// A transaction with a fresh id and no edits, as another buffer or a
/// language server could hand over.
fn fresh_transaction(buffer: &mut Buffer) -> Transaction {
    Transaction {
        id: buffer.lamport_clock.tick(),
        start: buffer.version.clone(),
        edit_ids: Vec::new(),
    }
}

fn redo_ids(pair: &Pair) -> Vec<TransactionId> {
    pair.buffer
        .history
        .redo_stack()
        .iter()
        .rev()
        .map(|entry| entry.transaction.id)
        .collect()
}

fn undo_ids(pair: &Pair) -> Vec<TransactionId> {
    pair.buffer
        .history
        .undo_stack()
        .iter()
        .map(|entry| entry.transaction.id)
        .collect()
}

#[test]
fn forget_and_merge_reparent_children_in_creation_order_and_hand_over_active_slot() {
    // forget: x and a are children of the root; a has children b, c and d,
    // with d active, and d has the child e, which is current.
    let mut pair = Pair::new(Duration::ZERO);
    let x = pair.transact("x");
    pair.undo();
    let a = pair.transact("a");
    let b = pair.transact("b");
    pair.undo();
    let c = pair.transact("c");
    pair.undo();
    let d = pair.transact("d");
    let e = pair.transact("e");
    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.children(tree.root()), vec![node(x), node(a)]);
    assert_eq!(tree.children(node(a)), vec![node(b), node(c), node(d)]);

    assert_eq!(pair.forget(a), Some(a));
    pair.assert_stacks();
    let tree = pair.buffer.undo_tree();
    assert_eq!(
        tree.children(tree.root()),
        vec![node(x), node(b), node(c), node(d)]
    );
    assert_eq!(tree.active_child(tree.root()), Some(node(d)));
    for child in [b, c, d] {
        assert_eq!(tree.parent(node(child)), Some(tree.root()));
    }
    assert_eq!(tree.current(), node(e));
    assert!(tree.node(node(a)).is_none());
    assert_eq!(undo_ids(&pair), vec![d, e]);

    // Forgetting the current node makes its parent current.
    assert_eq!(pair.forget(e), Some(e));
    pair.assert_stacks();
    assert_eq!(pair.buffer.undo_tree().current(), node(d));
    assert_eq!(pair.buffer.undo_tree().children(node(d)), Vec::new());
    pair.buffer.undo_tree().assert_consistent();

    // merge: p, then q with children r and s (s active), undone to p, so the
    // redo chain is q, s. Merging q into p hands q's children and active slot
    // to p and moves q's edits into p.
    let mut pair = Pair::new(Duration::ZERO);
    let p = pair.transact("p");
    let q = pair.transact("q");
    let r = pair.transact("r");
    pair.undo();
    let s = pair.transact("s");
    pair.undo();
    pair.undo();
    let p_edits = pair.buffer.get_transaction(p).unwrap().edit_ids.clone();
    let q_edits = pair.buffer.get_transaction(q).unwrap().edit_ids.clone();

    assert_eq!(pair.merge(q, p), Some(q));
    pair.assert_stacks();
    pair.assert_lookups(&[p, q, r, s]);
    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.children(node(p)), vec![node(r), node(s)]);
    assert_eq!(tree.active_child(node(p)), Some(node(s)));
    assert_eq!(tree.current(), node(p));
    assert!(tree.node(node(q)).is_none());
    assert_eq!(redo_ids(&pair), vec![s]);
    let mut expected = p_edits;
    expected.extend(q_edits);
    assert_eq!(pair.buffer.get_transaction(p).unwrap().edit_ids, expected);
    pair.buffer.undo_tree().assert_consistent();
}

#[test]
fn remove_from_undo_lands_between_current_and_old_active_child() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    let d = pair.transact("d");
    pair.undo();
    pair.undo();
    assert_eq!(redo_ids(&pair), vec![c, d]);

    // An entry below the current node.
    assert_eq!(pair.remove_from_undo(a), Some(a));
    pair.assert_stacks();
    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.current(), node(b));
    assert_eq!(tree.parent(node(b)), Some(tree.root()));
    assert_eq!(tree.parent(node(a)), Some(node(b)));
    assert_eq!(tree.children(node(a)), vec![node(c)]);
    assert_eq!(tree.active_child(node(a)), Some(node(c)));
    assert_eq!(redo_ids(&pair), vec![a, c, d]);

    // The current entry itself.
    assert_eq!(pair.remove_from_undo(b), Some(b));
    pair.assert_stacks();
    assert_eq!(pair.buffer.undo_tree().current(), UndoNodeId::ROOT);
    assert_eq!(redo_ids(&pair), vec![b, a, c, d]);
    pair.buffer.undo_tree().assert_consistent();

    for id in [b, a, c, d] {
        assert_eq!(pair.redo(), Some(id));
        pair.assert_stacks();
    }
    assert_eq!(pair.redo(), None);
    assert_eq!(pair.buffer.text(), "abcd");
}

#[test]
fn push_and_push_empty_after_undo_keep_the_redo_chain() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let z = pair.transact("z");
    pair.undo();
    let b = pair.transact("b");
    let c = pair.transact("c");
    pair.undo();
    pair.undo();
    assert_eq!(redo_ids(&pair), vec![b, c]);

    let t = fresh_transaction(&mut pair.buffer);
    let t_id = t.id;
    pair.push(t);
    pair.assert_stacks();
    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.current(), node(t_id));
    assert_eq!(tree.children(node(a)), vec![node(z), node(t_id)]);
    assert_eq!(tree.active_child(node(a)), Some(node(t_id)));
    assert_eq!(tree.children(node(t_id)), vec![node(b)]);
    assert_eq!(tree.active_child(node(t_id)), Some(node(b)));
    assert_eq!(redo_ids(&pair), vec![b, c]);

    let e = pair.push_empty();
    pair.assert_stacks();
    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.current(), node(e));
    assert_eq!(tree.children(node(t_id)), vec![node(e)]);
    assert_eq!(tree.parent(node(b)), Some(node(e)));
    assert_eq!(undo_ids(&pair), vec![a, t_id, e]);
    assert_eq!(redo_ids(&pair), vec![b, c]);
    pair.buffer.undo_tree().assert_consistent();

    assert_eq!(pair.redo(), Some(b));
    assert_eq!(pair.redo(), Some(c));
    assert_eq!(pair.buffer.text(), "abc");
}

#[test]
fn undo_push_empty_forget_leaves_current_and_redo_chain_unchanged() {
    let mut pair = Pair::new(Duration::ZERO);
    pair.transact("a");
    let b = pair.transact("b");
    pair.undo();
    pair.transact("y");
    pair.undo();
    pair.redo();
    pair.transact("c");
    pair.transact("d");

    // Format on save at each point of the history, the root included.
    for _ in 0..5 {
        let before = snapshot(pair.buffer.undo_tree());
        let redo_before = redo_ids(&pair);
        let e = pair.push_empty();
        pair.assert_stacks();
        assert_eq!(pair.forget(e), Some(e));
        pair.assert_stacks();
        assert_eq!(snapshot(pair.buffer.undo_tree()), before);
        assert_eq!(redo_ids(&pair), redo_before);
        pair.buffer.undo_tree().assert_consistent();
        pair.undo();
    }
    assert_eq!(pair.buffer.undo_tree().current(), UndoNodeId::ROOT);
    assert!(pair.buffer.get_transaction(b).is_none());
}

#[test]
fn lookups_return_none_for_undone_sibling_branch() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    pair.undo();
    pair.undo();
    let d = pair.transact("d");
    let all = [a, b, c, d];

    pair.assert_lookups(&all);
    for id in [b, c] {
        assert!(pair.buffer.get_transaction(id).is_none());
        assert!(pair.buffer.history.transaction_mut(id).is_none());
        assert!(pair.buffer.undo_tree().node(node(id)).is_some());
    }
    assert!(pair.buffer.get_transaction(a).is_some());
    assert!(pair.buffer.get_transaction(d).is_some());

    // The redo view is visible, the undone branch still is not.
    pair.undo();
    pair.assert_lookups(&all);
    assert!(pair.buffer.get_transaction(d).is_some());
    assert!(pair.buffer.get_transaction(b).is_none());
    pair.undo();
    pair.assert_lookups(&all);
    assert!(pair.buffer.history.transaction_mut(d).is_some());
    assert!(pair.buffer.history.transaction_mut(c).is_none());
}

#[test]
fn stale_id_forget_merge_and_remove_from_undo_return_none_and_change_nothing() {
    // A buffer with only the root.
    let mut pair = Pair::new(Duration::ZERO);
    let unknown = fresh_transaction(&mut pair.buffer).id;
    assert_eq!(pair.forget(unknown), None);
    assert_eq!(pair.merge(unknown, unknown), None);
    assert_eq!(pair.remove_from_undo(unknown), None);
    assert_eq!(pair.buffer.undo_tree().len(), 1);
    pair.assert_stacks();

    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    pair.undo();
    pair.undo();
    let d = pair.transact("d");
    let e = pair.transact("e");
    pair.undo();
    let before = snapshot(pair.buffer.undo_tree());
    let text = pair.buffer.text();
    let a_edits = pair.buffer.get_transaction(a).unwrap().edit_ids.clone();

    for stale in [b, c, unknown] {
        assert_eq!(pair.forget(stale), None);
        assert_eq!(pair.merge(stale, a), None);
        assert_eq!(pair.merge(a, stale), None);
        assert_eq!(pair.merge(stale, e), None);
        assert_eq!(pair.remove_from_undo(stale), None);
        pair.assert_stacks();
    }
    // Only entries on the undo view can be undone out of order.
    assert_eq!(pair.remove_from_undo(e), None);
    pair.assert_stacks();

    assert_eq!(snapshot(pair.buffer.undo_tree()), before);
    assert_eq!(pair.buffer.text(), text);
    assert_eq!(pair.buffer.get_transaction(a).unwrap().edit_ids, a_edits);
    assert_eq!(pair.buffer.undo_tree().current(), node(d));
    pair.buffer.undo_tree().assert_consistent();
}

#[test]
fn push_of_id_in_undone_branch_leaves_one_node_for_it() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    pair.undo();
    pair.undo();
    let d = pair.transact("d");
    let len = pair.buffer.undo_tree().len();

    let stale = pair
        .buffer
        .undo_tree()
        .node(node(b))
        .unwrap()
        .transaction()
        .unwrap()
        .clone();
    pair.push(stale);
    pair.assert_stacks();
    pair.assert_lookups(&[a, b, c, d]);

    let tree = pair.buffer.undo_tree();
    tree.assert_consistent();
    assert_eq!(tree.len(), len);
    let found = reachable(tree);
    assert_eq!(found.iter().filter(|id| **id == node(b)).count(), 1);
    assert_eq!(found.len(), len);
    assert_eq!(tree.current(), node(b));
    assert_eq!(tree.parent(node(b)), Some(node(d)));
    assert_eq!(tree.children(node(b)), Vec::new());
    // c took b's place in a's children.
    assert_eq!(tree.children(node(a)), vec![node(c), node(d)]);
    assert_eq!(tree.parent(node(c)), Some(node(a)));
    assert_eq!(undo_ids(&pair), vec![a, d, b]);
}

/// Runs seeded random operations, surgery included, checking the stacks and
/// lookups against the oracle after each and the tree's shape now and then.
fn run_surgery(seed: u64) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut pair = Pair::new(Duration::ZERO);
    let mut known: Vec<TransactionId> = Vec::new();
    let mut live: BTreeSet<TransactionId> = BTreeSet::new();
    // Transactions out of the tree, which may be pushed back.
    let mut detached: Vec<Transaction> = Vec::new();
    let mut millis = 0;
    let mut last_shape_check = 0;

    for step in 0..2_000 {
        millis += rng.random_range(0..600);
        pair.at(millis);
        let depth = pair.oracle.depth;
        let pick = |rng: &mut StdRng, pair: &Pair, known: &[TransactionId]| {
            let scoped = pair
                .oracle
                .undo_stack
                .iter()
                .chain(pair.oracle.redo_stack.iter())
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            if !scoped.is_empty() && rng.random_bool(0.6) {
                Some(scoped[rng.random_range(0..scoped.len())])
            } else if !known.is_empty() {
                Some(known[rng.random_range(0..known.len())])
            } else {
                None
            }
        };
        let mut touched = Vec::new();
        match rng.random_range(0..100) {
            0..20 if depth < 3 => pair.start(),
            20..38 if depth > 0 => pair.edit("x"),
            38..52 if depth > 0 => {
                if let Some(id) = pair.end() {
                    known.push(id);
                    live.insert(id);
                }
            }
            52..62 if depth == 0 => {
                pair.undo();
            }
            62..72 if depth == 0 => {
                pair.redo();
            }
            72..77 if depth == 0 => {
                let transaction = fresh_transaction(&mut pair.buffer);
                known.push(transaction.id);
                live.insert(transaction.id);
                pair.push(transaction);
            }
            77..80 if depth == 0 => {
                // Push back a detached transaction, or one from an undone
                // branch.
                if !detached.is_empty() && rng.random_bool(0.5) {
                    let transaction = detached.swap_remove(rng.random_range(0..detached.len()));
                    live.insert(transaction.id);
                    touched.push(transaction.id);
                    pair.push(transaction);
                } else {
                    let tree = pair.buffer.undo_tree();
                    let stale = reachable(tree)
                        .into_iter()
                        .filter_map(|id| id.transaction_id())
                        .filter(|id| pair.oracle.transaction(*id).is_none())
                        .collect::<Vec<_>>();
                    if !stale.is_empty() {
                        let id = stale[rng.random_range(0..stale.len())];
                        let transaction = tree.node(node(id)).unwrap().transaction().unwrap();
                        let transaction = transaction.clone();
                        touched.push(id);
                        pair.push(transaction);
                    }
                }
            }
            80..85 if depth == 0 => {
                let id = pair.push_empty();
                known.push(id);
                live.insert(id);
            }
            85..90 if depth == 0 => {
                if let Some(id) = pick(&mut rng, &pair, &known) {
                    touched.push(id);
                    let transaction = pair.buffer.history.transaction(id).cloned();
                    if pair.forget(id).is_some() {
                        live.remove(&id);
                        detached.push(transaction.unwrap());
                    }
                }
            }
            90..95 if depth == 0 => {
                if let (Some(from), Some(to)) =
                    (pick(&mut rng, &pair, &known), pick(&mut rng, &pair, &known))
                {
                    touched.extend([from, to]);
                    let transaction = pair.buffer.history.transaction(from).cloned();
                    if pair.merge(from, to).is_some() {
                        live.remove(&from);
                        detached.push(transaction.unwrap());
                    }
                }
            }
            95..100 if depth == 0 => {
                if let Some(id) = pick(&mut rng, &pair, &known) {
                    touched.push(id);
                    pair.remove_from_undo(id);
                }
            }
            _ => {}
        }
        pair.assert_stacks();
        for _ in 0..4 {
            if let Some(id) = pick(&mut rng, &pair, &known) {
                touched.push(id);
            }
        }
        pair.assert_lookups(&touched);
        // A pending transaction is a node that is not ended yet.
        if pair.oracle.depth == 0 && step >= last_shape_check + 25 {
            last_shape_check = step;
            assert_shape(&pair, &live);
        }
    }
    while pair.oracle.depth > 0 {
        if let Some(id) = pair.end() {
            known.push(id);
            live.insert(id);
        }
        pair.assert_stacks();
    }
    pair.assert_lookups(&known);
    assert_shape(&pair, &live);
}

/// Every ended or pushed transaction not forgotten or merged away is exactly
/// one reachable node, and the tree has no other node.
fn assert_shape(pair: &Pair, live: &BTreeSet<TransactionId>) {
    let tree = pair.buffer.undo_tree();
    tree.assert_consistent();
    let found = reachable(tree);
    assert_eq!(found.len(), tree.len());
    let found = found
        .into_iter()
        .filter_map(|id| id.transaction_id())
        .collect::<Vec<_>>();
    let unique = found.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), found.len(), "an id has two nodes");
    assert_eq!(&unique, live);
}

#[test]
fn seeded_2000_operation_runs_with_surgery_match_the_oracle() {
    for seed in 0..6 {
        run_surgery(seed);
    }
}
