//! Tests of the undo tree's node limit: which leaf is pruned first, what is
//! never pruned, how pins nest and what resetting the tree leaves behind.

use super::*;
use crate::undo_tree_tests::node;
use std::time::{Duration, Instant};

/// A buffer driven one transaction at a time. Transactions never group, and
/// nothing reads the clock after the buffer is made.
struct Fixture {
    buffer: Buffer,
    now: Instant,
}

impl Fixture {
    fn new() -> Self {
        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), "");
        buffer.set_group_interval(Duration::ZERO);
        Self {
            buffer,
            now: Instant::now(),
        }
    }

    fn with_limit(limit: usize) -> Self {
        let mut fixture = Self::new();
        fixture.buffer.set_undo_node_limit(limit);
        fixture
    }

    /// One transaction with one edit at the end; returns its id.
    fn transact(&mut self, text: &str) -> TransactionId {
        self.buffer.start_transaction_at(self.now);
        let end = self.buffer.len();
        self.buffer.edit([(end..end, text)]);
        self.buffer.end_transaction_at(self.now).unwrap().0
    }

    fn transact_many(&mut self, count: usize) -> Vec<TransactionId> {
        (0..count).map(|_| self.transact("x")).collect()
    }

    fn undo(&mut self) -> Option<TransactionId> {
        self.buffer.undo().map(|(id, _)| id)
    }

    fn redo(&mut self) -> Option<TransactionId> {
        self.buffer.redo().map(|(id, _)| id)
    }

    fn undo_times(&mut self, count: usize) {
        for _ in 0..count {
            self.undo().unwrap();
        }
    }

    fn len(&self) -> usize {
        self.buffer.undo_tree().len()
    }

    fn pruned(&self) -> usize {
        self.buffer.undo_tree().pruned()
    }

    fn has(&self, id: TransactionId) -> bool {
        self.buffer.undo_tree().node(node(id)).is_some()
    }

    fn assert_consistent(&self) {
        self.buffer.undo_tree().assert_consistent();
    }
}

#[test]
fn prune_oldest_unprotected_leaf_when_over_the_default_limit() {
    let mut f = Fixture::new();
    assert_eq!(f.buffer.undo_tree().limit(), DEFAULT_UNDO_NODE_LIMIT);
    assert_eq!(DEFAULT_UNDO_NODE_LIMIT, 10_000);

    // `old` is an undone leaf created first. `tip` is the end of a chain that
    // is undone by one step before a sibling branch grows from its parent,
    // so it is the second-oldest leaf that nothing needs.
    let old = f.transact("a");
    f.undo().unwrap();
    let chain = f.transact_many(5_000);
    let tip = *chain.last().unwrap();
    f.undo().unwrap();
    let branch = f.transact("y");
    while f.len() < DEFAULT_UNDO_NODE_LIMIT {
        f.transact("z");
    }
    assert_eq!(f.len(), 10_000);
    assert_eq!(f.pruned(), 0);
    assert!(f.has(old) && f.has(tip));

    // Node 10,001: the oldest unprotected leaf goes, and only that one.
    f.transact("w");
    assert_eq!(f.len(), 10_000);
    assert_eq!(f.pruned(), 1);
    assert!(!f.has(old));
    assert!(f.has(tip) && f.has(branch));

    // The next addition takes the next oldest.
    f.transact("v");
    assert_eq!(f.len(), 10_000);
    assert_eq!(f.pruned(), 2);
    assert!(!f.has(tip));
    assert!(f.has(branch));
    assert_eq!(
        f.buffer.undo_tree().parent(node(branch)),
        Some(node(chain[4_998]))
    );
    f.assert_consistent();
}

#[test]
fn childless_parent_becomes_a_candidate() {
    let mut f = Fixture::new();
    // An undone branch p, q, r, then a main chain from the root.
    let p = f.transact("p");
    let q = f.transact("q");
    let r = f.transact("r");
    f.undo_times(3);
    f.transact_many(98);
    assert_eq!(f.len(), 102);

    // Lowering the limit prunes r, which leaves q childless, so q goes next.
    f.buffer.set_undo_node_limit(100);
    assert_eq!(f.len(), 100);
    assert_eq!(f.pruned(), 2);
    assert!(!f.has(r) && !f.has(q));
    assert!(f.has(p));
    assert_eq!(f.buffer.undo_tree().children(node(p)), Vec::new());

    // p is a leaf now, and the oldest one, so the next addition removes it.
    f.transact("x");
    assert_eq!(f.len(), 100);
    assert_eq!(f.pruned(), 3);
    assert!(!f.has(p));
    f.assert_consistent();
}

#[test]
fn current_path_pinned_path_and_active_chain_are_never_removed() {
    let mut f = Fixture::new();
    // An undone chain off the root that nothing needs.
    let junk = f.transact_many(20);
    f.undo_times(20);
    // a1, a2, a3, then a branch b1, b2 off a2, with b2 pinned.
    let a1 = f.transact("a");
    let a2 = f.transact("a");
    let a3 = f.transact("a");
    f.undo().unwrap();
    let b1 = f.transact("b");
    let b2 = f.transact("b");
    assert!(f.buffer.pin_undo_node(node(b2)));
    // A chain of 100 nodes off a1, with its last two undone: the current
    // node is c98 and the redo chain is c99, c100.
    f.undo_times(3);
    let c = f.transact_many(100);
    f.undo_times(2);
    assert_eq!(f.buffer.undo_tree().current(), node(c[97]));
    assert_eq!(f.len(), 126);
    let text = f.buffer.text();

    f.buffer.set_undo_node_limit(100);
    // Only a3 and the junk were unprotected; the rest stays over the limit.
    assert_eq!(f.len(), 105);
    assert_eq!(f.pruned(), 21);
    assert!(!f.has(a3));
    assert!(junk.iter().all(|id| !f.has(*id)));
    for id in [a1, a2, b1, b2].iter().chain(c.iter()) {
        assert!(f.has(*id));
    }
    assert_eq!(f.buffer.text(), text);
    f.assert_consistent();

    // Releasing the pin frees b2, b1 and then a2, which is left childless.
    // a1 stays: the current path goes through it.
    assert!(f.buffer.unpin_undo_node(node(b2)));
    f.buffer.set_undo_node_limit(100);
    assert_eq!(f.len(), 102);
    assert_eq!(f.pruned(), 24);
    assert!(!f.has(b2) && !f.has(b1) && !f.has(a2));
    assert!(f.has(a1));

    // The redo chain is whole.
    assert_eq!(f.redo(), Some(c[98]));
    assert_eq!(f.redo(), Some(c[99]));
    assert_eq!(f.redo(), None);
    f.assert_consistent();
}

#[test]
fn limit_is_exceeded_when_every_leaf_is_protected() {
    let mut f = Fixture::new();
    let chain = f.transact_many(150);
    f.buffer.set_undo_node_limit(100);
    assert_eq!(f.buffer.undo_tree().limit(), 100);
    assert_eq!(f.len(), 151);
    assert_eq!(f.pruned(), 0);

    // The same holds when the tip is only on the redo chain.
    f.undo_times(5);
    f.buffer.set_undo_node_limit(100);
    assert_eq!(f.len(), 151);
    assert!(f.has(*chain.last().unwrap()));

    // And a new node on top is kept as well.
    for _ in 0..5 {
        f.redo().unwrap();
    }
    f.transact("x");
    assert_eq!(f.len(), 152);
    assert_eq!(f.pruned(), 0);
    f.assert_consistent();
}

#[test]
fn limits_below_the_floor_are_raised_and_pruned_counts_each_removal() {
    let mut f = Fixture::new();
    f.buffer.set_undo_node_limit(1);
    assert_eq!(MIN_UNDO_NODE_LIMIT, 100);
    assert_eq!(f.buffer.undo_tree().limit(), 100);

    // 50 undone leaves and a chain of 49: 100 nodes with the root, which is
    // at the limit, not over it.
    let leaves = (0..50)
        .map(|_| {
            let id = f.transact("l");
            f.undo().unwrap();
            id
        })
        .collect::<Vec<_>>();
    f.transact_many(49);
    assert_eq!(f.len(), 100);
    assert_eq!(f.pruned(), 0);

    for removed in 1..=3 {
        f.transact("x");
        assert_eq!(f.len(), 100);
        assert_eq!(f.pruned(), removed);
        assert!(!f.has(leaves[removed - 1]));
        assert!(f.has(leaves[removed]));
    }

    // Resetting is not pruning.
    f.buffer.reset_undo_tree();
    assert_eq!(f.pruned(), 3);
    assert_eq!(f.len(), 1);
    f.assert_consistent();
}

#[test]
fn grouping_never_merges_a_pinned_node_away() {
    let mut f = Fixture::new();
    let a = f.transact("a");
    let b = f.transact("b");
    let c = f.transact("c");
    assert!(f.buffer.pin_undo_node(node(c)));

    // Grouping up to `a` would merge the pinned `c` into `b`, so it stops.
    f.buffer.group_until_transaction(a);
    assert_eq!(f.len(), 4);
    assert!(f.has(a) && f.has(b) && f.has(c));
    assert_eq!(f.buffer.undo_tree().current(), node(c));
    f.assert_consistent();

    // The pin survived: it can be released, and then grouping merges.
    assert!(f.buffer.unpin_undo_node(node(c)));
    assert!(!f.buffer.unpin_undo_node(node(c)));
    f.assert_consistent();
    f.buffer.group_until_transaction(a);
    assert!(f.has(a) && !f.has(b) && !f.has(c));
    assert_eq!(f.buffer.undo_tree().current(), node(a));
    f.assert_consistent();
}

#[test]
fn pins_nest_and_unbalanced_unpins_are_ignored() {
    let mut f = Fixture::with_limit(100);
    let leaf = f.transact("l");
    f.undo().unwrap();
    let spare = f.transact("m");
    f.undo().unwrap();

    // An unbalanced unpin does nothing, and does not bank a credit.
    assert!(!f.buffer.unpin_undo_node(node(spare)));
    assert!(f.buffer.pin_undo_node(node(spare)));
    assert!(f.buffer.unpin_undo_node(node(spare)));
    assert!(!f.buffer.unpin_undo_node(node(spare)));

    assert!(f.buffer.pin_undo_node(node(leaf)));
    assert!(f.buffer.pin_undo_node(node(leaf)));
    f.transact_many(97);
    assert_eq!(f.len(), 100);

    // Over the limit, the unpinned spare goes and the pinned leaf stays.
    f.transact("x");
    assert_eq!(f.len(), 100);
    assert_eq!(f.pruned(), 1);
    assert!(!f.has(spare));
    assert!(f.has(leaf));

    // Nothing else can go: the limit is exceeded.
    f.transact("x");
    assert_eq!(f.len(), 101);
    assert_eq!(f.pruned(), 1);

    // One unpin of two still protects the leaf.
    assert!(f.buffer.unpin_undo_node(node(leaf)));
    f.transact("x");
    assert_eq!(f.len(), 102);
    assert_eq!(f.pruned(), 1);
    assert!(f.has(leaf));

    // The second unpin releases it, and the next addition prunes it.
    assert!(f.buffer.unpin_undo_node(node(leaf)));
    assert!(!f.buffer.unpin_undo_node(node(leaf)));
    f.transact("x");
    assert_eq!(f.len(), 102);
    assert_eq!(f.pruned(), 2);
    assert!(!f.has(leaf));

    // A pruned node cannot be pinned, and neither can an unknown one.
    assert!(!f.buffer.pin_undo_node(node(leaf)));
    assert!(!f.buffer.pin_undo_node(node(spare)));
    assert!(!f.buffer.unpin_undo_node(node(leaf)));
    assert!(f.buffer.pin_undo_node(UndoNodeId::ROOT));
    f.assert_consistent();
}

#[test]
fn reset_leaves_one_root_at_the_current_text() {
    let mut f = Fixture::new();
    let a = f.transact("a");
    let b = f.transact("b");
    f.undo().unwrap();
    let c = f.transact("c");
    let d = f.transact("d");
    f.undo().unwrap();
    assert!(f.buffer.pin_undo_node(node(b)));
    assert_eq!(f.buffer.text(), "ac");
    assert_eq!(f.len(), 5);

    f.buffer.reset_undo_tree();
    let tree = f.buffer.undo_tree();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree.current(), tree.root());
    assert_eq!(tree.children(tree.root()), Vec::new());
    assert_eq!(tree.active_child(tree.root()), None);
    assert!(tree.node(tree.root()).unwrap().transaction().is_none());
    for id in [a, b, c, d] {
        assert!(tree.node(node(id)).is_none());
    }
    assert_eq!(f.buffer.text(), "ac");
    assert_eq!(f.undo(), None);
    assert_eq!(f.redo(), None);
    assert_eq!(f.buffer.text(), "ac");
    // The pin went with the node.
    assert!(!f.buffer.pin_undo_node(node(b)));
    assert!(!f.buffer.unpin_undo_node(node(b)));
    f.assert_consistent();

    // The history works afterwards, and its undo stops at the new root.
    let e = f.transact("e");
    assert_eq!(f.buffer.text(), "ace");
    assert_eq!(f.undo(), Some(e));
    assert_eq!(f.buffer.text(), "ac");
    assert_eq!(f.undo(), None);
    f.assert_consistent();
}
