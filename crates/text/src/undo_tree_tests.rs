//! Tests of the undo tree. The oracle is the earlier two-stack history: it
//! never clears the redo stack on a push, and its grouping stops at an entry
//! that was ended while the redo stack was not empty, which is where the tree
//! forks.

use super::*;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) struct OracleEntry {
    pub(crate) id: TransactionId,
    pub(crate) edit_ids: Vec<clock::Lamport>,
    first_edit_at: Instant,
    last_edit_at: Instant,
    suppress_grouping: bool,
    /// Ended while the redo stack was not empty (recorded when it starts, as
    /// the redo stack cannot change before it ends): a fork in the tree, so
    /// grouping never merges this entry into the one below it.
    forked: bool,
}

#[derive(Default)]
pub(crate) struct Oracle {
    pub(crate) undo_stack: Vec<OracleEntry>,
    pub(crate) redo_stack: Vec<OracleEntry>,
    pub(crate) depth: usize,
    group_interval: Duration,
}

impl Oracle {
    fn start_transaction(&mut self, id: Option<TransactionId>, now: Instant) {
        self.depth += 1;
        if self.depth == 1 {
            self.undo_stack.push(OracleEntry {
                id: id.expect("an outer transaction has an id"),
                edit_ids: Vec::new(),
                first_edit_at: now,
                last_edit_at: now,
                suppress_grouping: false,
                // The redo stack cannot change before this entry ends.
                forked: !self.redo_stack.is_empty(),
            });
        } else {
            assert_eq!(id, None);
        }
    }

    fn push_undo(&mut self, edit_id: clock::Lamport) {
        self.undo_stack.last_mut().unwrap().edit_ids.push(edit_id);
    }

    fn end_transaction(&mut self, now: Instant) -> Option<TransactionId> {
        self.depth -= 1;
        if self.depth > 0 {
            return None;
        }
        if self.undo_stack.last().unwrap().edit_ids.is_empty() {
            self.undo_stack.pop();
            return None;
        }
        self.redo_stack.clear();
        let entry = self.undo_stack.last_mut().unwrap();
        entry.last_edit_at = now;
        self.group()
    }

    fn group(&mut self) -> Option<TransactionId> {
        let mut count = 0;
        let mut entries = self.undo_stack.iter();
        if let Some(mut entry) = entries.next_back() {
            while let Some(prev_entry) = entries.next_back() {
                if !entry.forked
                    && !prev_entry.suppress_grouping
                    && entry.first_edit_at - prev_entry.last_edit_at < self.group_interval
                {
                    entry = prev_entry;
                    count += 1;
                } else {
                    break;
                }
            }
        }
        self.group_trailing(count)
    }

    fn group_until(&mut self, transaction_id: TransactionId) {
        let mut count = 0;
        for entry in self.undo_stack.iter().rev() {
            if entry.id == transaction_id {
                self.group_trailing(count);
                break;
            } else if entry.suppress_grouping || entry.forked {
                break;
            } else {
                count += 1;
            }
        }
    }

    fn group_trailing(&mut self, n: usize) -> Option<TransactionId> {
        let new_len = self.undo_stack.len() - n;
        let (entries_to_keep, entries_to_merge) = self.undo_stack.split_at_mut(new_len);
        if let Some(last_entry) = entries_to_keep.last_mut() {
            for entry in &*entries_to_merge {
                last_entry.edit_ids.extend_from_slice(&entry.edit_ids);
            }
            if let Some(entry) = entries_to_merge.last() {
                last_entry.last_edit_at = entry.last_edit_at;
            }
        }
        self.undo_stack.truncate(new_len);
        self.undo_stack.last().map(|entry| entry.id)
    }

    fn pop_undo(&mut self) -> Option<TransactionId> {
        let entry = self.undo_stack.pop()?;
        let id = entry.id;
        self.redo_stack.push(entry);
        Some(id)
    }

    fn pop_redo(&mut self) -> Option<TransactionId> {
        let entry = self.redo_stack.pop()?;
        let id = entry.id;
        self.undo_stack.push(entry);
        Some(id)
    }

    // Node surgery on the stacks. Where the old stack history differs from
    // the tree's rules, these follow the tree: a push keeps the redo stack,
    // `remove_from_undo` puts the entry on top of the redo stack, and an id
    // on neither stack gives `None`.

    /// A pushed entry. Its `forked` flag only approximates the tree's fork
    /// rule, so grouping across pushed entries is not modeled exactly.
    pub(crate) fn push(&mut self, id: TransactionId, edit_ids: Vec<clock::Lamport>, now: Instant) {
        self.undo_stack.push(OracleEntry {
            id,
            edit_ids,
            first_edit_at: now,
            last_edit_at: now,
            suppress_grouping: false,
            forked: !self.redo_stack.is_empty(),
        });
    }

    pub(crate) fn forget(&mut self, id: TransactionId) -> Option<OracleEntry> {
        if let Some(ix) = self.undo_stack.iter().rposition(|entry| entry.id == id) {
            Some(self.undo_stack.remove(ix))
        } else if let Some(ix) = self.redo_stack.iter().rposition(|entry| entry.id == id) {
            Some(self.redo_stack.remove(ix))
        } else {
            None
        }
    }

    pub(crate) fn merge(
        &mut self,
        transaction: TransactionId,
        destination: TransactionId,
    ) -> Option<TransactionId> {
        self.transaction(destination)?;
        let forgotten = self.forget(transaction)?;
        if let Some(entry) = self
            .undo_stack
            .iter_mut()
            .chain(self.redo_stack.iter_mut())
            .find(|entry| entry.id == destination)
        {
            entry.edit_ids.extend_from_slice(&forgotten.edit_ids);
        }
        Some(forgotten.id)
    }

    pub(crate) fn remove_from_undo(&mut self, id: TransactionId) -> Option<TransactionId> {
        let ix = self.undo_stack.iter().rposition(|entry| entry.id == id)?;
        let entry = self.undo_stack.remove(ix);
        self.redo_stack.push(entry);
        Some(id)
    }

    /// The edits of a transaction on either stack.
    pub(crate) fn transaction(&self, id: TransactionId) -> Option<&[clock::Lamport]> {
        self.undo_stack
            .iter()
            .chain(self.redo_stack.iter())
            .find(|entry| entry.id == id)
            .map(|entry| entry.edit_ids.as_slice())
    }
}

/// A buffer and the oracle, driven together with fixed instants.
pub(crate) struct Pair {
    pub(crate) buffer: Buffer,
    pub(crate) oracle: Oracle,
    base: Instant,
    pub(crate) now: Instant,
    pub(crate) ended: Vec<TransactionId>,
    pub(crate) edits: Vec<clock::Lamport>,
}

impl Pair {
    pub(crate) fn new(group_interval: Duration) -> Self {
        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), "");
        buffer.set_group_interval(group_interval);
        let base = Instant::now();
        Self {
            buffer,
            oracle: Oracle {
                group_interval,
                ..Oracle::default()
            },
            base,
            now: base,
            ended: Vec::new(),
            edits: Vec::new(),
        }
    }

    pub(crate) fn at(&mut self, millis: u64) {
        self.now = self.base + Duration::from_millis(millis);
    }

    pub(crate) fn start(&mut self) {
        let id = self.buffer.start_transaction_at(self.now);
        self.oracle.start_transaction(id, self.now);
    }

    pub(crate) fn edit(&mut self, text: &str) {
        let end = self.buffer.len();
        let op = self.buffer.edit([(end..end, text)]);
        self.oracle.push_undo(op.timestamp());
        self.edits.push(op.timestamp());
    }

    pub(crate) fn end(&mut self) -> Option<TransactionId> {
        let id = self.buffer.end_transaction_at(self.now).map(|(id, _)| id);
        assert_eq!(id, self.oracle.end_transaction(self.now));
        if let Some(id) = id {
            self.ended.push(id);
        }
        id
    }

    /// One outer transaction with one edit; returns its id after grouping.
    pub(crate) fn transact(&mut self, text: &str) -> TransactionId {
        self.start();
        self.edit(text);
        self.end().unwrap()
    }

    pub(crate) fn undo(&mut self) -> Option<TransactionId> {
        let id = self.buffer.undo().map(|(id, _)| id);
        assert_eq!(id, self.oracle.pop_undo());
        id
    }

    pub(crate) fn redo(&mut self) -> Option<TransactionId> {
        let id = self.buffer.redo().map(|(id, _)| id);
        assert_eq!(id, self.oracle.pop_redo());
        id
    }

    fn group_until(&mut self, id: TransactionId) {
        self.buffer.group_until_transaction(id);
        self.oracle.group_until(id);
    }

    pub(crate) fn push(&mut self, transaction: Transaction) {
        self.oracle
            .push(transaction.id, transaction.edit_ids.clone(), self.now);
        self.buffer.push_transaction(transaction, self.now);
    }

    pub(crate) fn push_empty(&mut self) -> TransactionId {
        let id = self.buffer.push_empty_transaction(self.now);
        self.oracle.push(id, Vec::new(), self.now);
        id
    }

    pub(crate) fn forget(&mut self, id: TransactionId) -> Option<TransactionId> {
        let forgotten = self.buffer.forget_transaction(id).map(|t| t.id);
        assert_eq!(forgotten, self.oracle.forget(id).map(|entry| entry.id));
        forgotten
    }

    pub(crate) fn merge(
        &mut self,
        transaction: TransactionId,
        destination: TransactionId,
    ) -> Option<TransactionId> {
        let merged = self
            .buffer
            .history
            .merge_transactions(transaction, destination)
            .map(|t| t.id);
        assert_eq!(merged, self.oracle.merge(transaction, destination));
        merged
    }

    pub(crate) fn remove_from_undo(&mut self, id: TransactionId) -> Option<TransactionId> {
        let removed = self.buffer.undo_transaction(id).map(|_| id);
        assert_eq!(removed, self.oracle.remove_from_undo(id));
        removed
    }

    /// Checks `get_transaction`, `transaction` and `transaction_mut` against
    /// the oracle's stacks for each id.
    pub(crate) fn assert_lookups(&mut self, ids: &[TransactionId]) {
        for id in ids {
            let expected = self.oracle.transaction(*id).map(|edits| edits.to_vec());
            assert_eq!(
                self.buffer.get_transaction(*id).map(|t| t.edit_ids.clone()),
                expected,
                "get_transaction({id:?})"
            );
            assert_eq!(
                self.buffer
                    .history
                    .transaction(*id)
                    .map(|t| t.edit_ids.clone()),
                expected,
                "transaction({id:?})"
            );
            assert_eq!(
                self.buffer
                    .history
                    .transaction_mut(*id)
                    .map(|t| t.edit_ids.clone()),
                expected,
                "transaction_mut({id:?})"
            );
        }
    }

    pub(crate) fn assert_stacks(&self) {
        let view = |entries: Vec<HistoryEntry>| {
            entries
                .into_iter()
                .map(|entry| (entry.transaction.id, entry.transaction.edit_ids))
                .collect::<Vec<_>>()
        };
        let oracle = |entries: &[OracleEntry]| {
            entries
                .iter()
                .map(|entry| (entry.id, entry.edit_ids.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            view(self.buffer.history.undo_stack()),
            oracle(&self.oracle.undo_stack),
            "undo stacks differ"
        );
        assert_eq!(
            view(self.buffer.history.redo_stack()),
            oracle(&self.oracle.redo_stack),
            "redo stacks differ"
        );
        assert_eq!(
            self.buffer.peek_undo_stack().map(|e| e.transaction_id()),
            self.oracle.undo_stack.last().map(|e| e.id)
        );
        assert_eq!(
            self.buffer.peek_redo_stack().map(|e| e.transaction_id()),
            self.oracle.redo_stack.last().map(|e| e.id)
        );
    }
}

pub(crate) fn node(id: TransactionId) -> UndoNodeId {
    UndoNodeId(id)
}

/// Every node reachable from the root by following children.
pub(crate) fn reachable(tree: &UndoTree) -> Vec<UndoNodeId> {
    let mut found = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(id) = stack.pop() {
        found.push(id);
        stack.extend(tree.children(id));
    }
    found
}

#[test]
fn undo_then_edit_leaves_undone_transactions_as_a_sibling_branch() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    assert_eq!(pair.undo(), Some(c));
    assert_eq!(pair.undo(), Some(b));
    let d = pair.transact("d");
    pair.assert_stacks();
    assert_eq!(pair.buffer.text(), "ad");

    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.len(), 5);
    assert_eq!(tree.current(), node(d));
    assert_eq!(tree.children(tree.root()), vec![node(a)]);
    assert_eq!(tree.children(node(a)), vec![node(b), node(d)]);
    assert_eq!(tree.active_child(node(a)), Some(node(d)));
    assert_eq!(tree.children(node(b)), vec![node(c)]);
    assert_eq!(tree.parent(node(c)), Some(node(b)));
    assert_eq!(tree.depth(node(c)), Some(3));
    assert_eq!(pair.redo(), None);

    // The undone branch is out of the scoped lookups.
    assert!(pair.buffer.get_transaction(b).is_none());
    assert!(pair.buffer.get_transaction(c).is_none());
    assert!(pair.buffer.get_transaction(a).is_some());
    assert!(pair.buffer.forget_transaction(c).is_none());
    assert_eq!(pair.buffer.undo_tree().len(), 5);
}

#[test]
fn redo_after_undo_moves_along_the_chain_and_adds_no_node() {
    let mut pair = Pair::new(Duration::ZERO);
    assert_eq!(pair.buffer.undo_tree().len(), 1);
    assert_eq!(pair.buffer.undo_tree().current(), UndoNodeId::ROOT);
    assert_eq!(pair.undo(), None);
    assert_eq!(pair.redo(), None);

    let a = pair.transact("a");
    let b = pair.transact("b");
    let c = pair.transact("c");
    assert_eq!(pair.undo(), Some(c));
    assert_eq!(pair.undo(), Some(b));
    assert_eq!(pair.buffer.undo_tree().current(), node(a));
    assert_eq!(pair.redo(), Some(b));
    assert_eq!(pair.buffer.undo_tree().current(), node(b));
    assert_eq!(pair.buffer.text(), "ab");
    assert_eq!(pair.redo(), Some(c));
    assert_eq!(pair.redo(), None);
    assert_eq!(pair.buffer.text(), "abc");
    assert_eq!(pair.buffer.undo_tree().len(), 4);
    for id in [a, b] {
        assert_eq!(pair.buffer.undo_tree().children(node(id)).len(), 1);
    }

    // A transaction that ends without edits leaves no node, nested or not.
    pair.start();
    pair.start();
    assert_eq!(pair.end(), None);
    assert_eq!(pair.end(), None);
    assert_eq!(pair.buffer.undo_tree().len(), 4);
    assert_eq!(pair.buffer.undo_tree().current(), node(c));
    pair.assert_stacks();
}

#[test]
fn group_and_group_until_stop_at_a_fork() {
    let mut pair = Pair::new(Duration::from_millis(300));
    pair.at(0);
    let a = pair.transact("a");
    pair.at(1_000);
    let b = pair.transact("b");
    assert_ne!(a, b);
    assert_eq!(pair.undo(), Some(b));

    // Close enough in time to group with `a`, but `a` now has two children.
    pair.at(100);
    let c = pair.transact("c");
    assert_ne!(c, a);
    pair.assert_stacks();
    assert_eq!(pair.buffer.undo_tree().parent(node(c)), Some(node(a)));

    // With no fork, a close transaction groups into the one before it.
    pair.at(150);
    assert_eq!(pair.transact("d"), c);
    pair.assert_stacks();
    assert_eq!(pair.buffer.undo_tree().children(node(c)), Vec::new());

    // `group_until` stops at a fork too.
    pair.at(5_000);
    let e = pair.transact("e");
    assert_eq!(pair.undo(), Some(e));
    pair.at(6_000);
    let f = pair.transact("f");
    pair.group_until(c);
    pair.assert_stacks();
    assert_eq!(pair.buffer.undo_tree().parent(node(f)), Some(node(c)));
    assert_eq!(
        pair.buffer.undo_tree().children(node(c)),
        vec![node(e), node(f)]
    );

    // And merges when nothing forks.
    pair.at(10_000);
    let g = pair.transact("g");
    pair.group_until(f);
    pair.assert_stacks();
    assert!(pair.buffer.undo_tree().node(node(g)).is_none());
    assert_eq!(pair.buffer.undo_tree().current(), node(f));
    assert_eq!(pair.buffer.text(), "acdfg");
}

#[test]
fn toggles_between_two_branches() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    let b = pair.transact("b");
    let b_text = pair.buffer.text();
    assert_eq!(pair.undo(), Some(b));
    let c = pair.transact("c");
    let c_text = pair.buffer.text();

    let tree = pair.buffer.undo_tree();
    assert_eq!(tree.toggles(node(b), node(c)), vec![b, c]);
    assert_eq!(tree.toggles(node(c), node(b)), vec![c, b]);
    assert_eq!(tree.toggles(tree.root(), node(c)), vec![a, c]);
    assert_eq!(tree.toggles(node(c), tree.root()), vec![c, a]);
    assert_eq!(tree.toggles(node(c), node(a)), vec![c]);
    assert_eq!(tree.toggles(node(b), node(b)), Vec::new());

    // Toggling those transactions moves the text between the two states.
    let toggle = |buffer: &mut Buffer, from: TransactionId, to: TransactionId| {
        let ids = buffer.undo_tree().toggles(node(from), node(to));
        let mut counts = HashMap::default();
        for id in ids {
            let transaction = buffer.undo_tree().node(node(id)).unwrap().transaction();
            for edit_id in transaction.unwrap().edit_ids.clone() {
                counts.insert(edit_id, buffer.undo_count(edit_id) + 1);
            }
        }
        buffer.undo_operations(counts);
    };
    toggle(&mut pair.buffer, c, b);
    assert_eq!(pair.buffer.text(), b_text);
    toggle(&mut pair.buffer, b, c);
    assert_eq!(pair.buffer.text(), c_text);
}

#[test]
fn undo_count_per_edit() {
    let mut pair = Pair::new(Duration::ZERO);
    let a = pair.transact("a");
    pair.start();
    pair.edit("b");
    pair.edit("c");
    let b = pair.end().unwrap();
    let a_edit = pair.edits[0];
    let b_edits = [pair.edits[1], pair.edits[2]];
    let counts = |pair: &Pair| {
        (
            pair.buffer.undo_count(a_edit),
            b_edits.map(|edit| pair.buffer.undo_count(edit)),
        )
    };
    assert_eq!(counts(&pair), (0, [0, 0]));
    assert_eq!(pair.undo(), Some(b));
    assert_eq!(counts(&pair), (0, [1, 1]));
    assert_eq!(pair.redo(), Some(b));
    assert_eq!(counts(&pair), (0, [2, 2]));
    assert_eq!(pair.undo(), Some(b));
    assert_eq!(pair.undo(), Some(a));
    assert_eq!(counts(&pair), (1, [3, 3]));
    assert_eq!(pair.buffer.text(), "");
}

/// Runs seeded random operations on the pair, checking the stacks after each.
fn run_random(pair: &mut Pair, rng: &mut StdRng, operations: usize, with_group_until: bool) {
    let mut millis = 0;
    for _ in 0..operations {
        millis += rng.random_range(0..600);
        pair.at(millis);
        let depth = pair.oracle.depth;
        match rng.random_range(0..100) {
            0..25 if depth < 3 => pair.start(),
            25..50 if depth > 0 => pair.edit("x"),
            50..70 if depth > 0 => {
                pair.end();
            }
            70..82 if depth == 0 => {
                pair.undo();
            }
            82..94 if depth == 0 => {
                pair.redo();
            }
            94..100 if with_group_until => {
                let stack = &pair.oracle.undo_stack;
                if !stack.is_empty() {
                    let ix = rng.random_range(0..stack.len());
                    let id = stack[ix].id;
                    pair.group_until(id);
                }
            }
            _ => {}
        }
        pair.assert_stacks();
    }
    while pair.oracle.depth > 0 {
        pair.end();
        pair.assert_stacks();
    }
}

#[test]
fn seeded_runs_give_the_oracle_stacks() {
    for seed in 0..10 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut pair = Pair::new(Duration::from_millis(300));
        run_random(&mut pair, &mut rng, 2_000, true);
        assert_eq!(
            pair.buffer.undo_tree().len(),
            reachable(pair.buffer.undo_tree()).len()
        );
    }
}

#[test]
fn every_transaction_ever_ended_stays_reachable() {
    // Without grouping every ended transaction is its own node.
    for seed in 0..5 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut pair = Pair::new(Duration::ZERO);
        run_random(&mut pair, &mut rng, 2_000, false);
        let tree = pair.buffer.undo_tree();
        let found = reachable(tree);
        assert_eq!(found.len(), tree.len());
        assert_eq!(tree.len(), pair.ended.len() + 1);
        for id in &pair.ended {
            assert!(found.contains(&node(*id)), "transaction {id:?} lost");
        }
    }

    // With grouping every edit stays in exactly one reachable node.
    for seed in 0..5 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut pair = Pair::new(Duration::from_millis(300));
        run_random(&mut pair, &mut rng, 2_000, true);
        let tree = pair.buffer.undo_tree();
        let mut edits = reachable(tree)
            .into_iter()
            .filter_map(|id| tree.node(id).unwrap().transaction())
            .flat_map(|transaction| transaction.edit_ids.clone())
            .collect::<Vec<_>>();
        edits.sort();
        let mut expected = pair.edits.clone();
        expected.sort();
        assert_eq!(edits, expected);
    }

    // Remote operations add no node.
    let mut local = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), "");
    let mut remote = Buffer::new(ReplicaId::new(8), BufferId::new(1).unwrap(), "");
    let ops = vec![remote.edit([(0..0, "remote")])];
    local.apply_ops(ops);
    assert_eq!(local.text(), "remote");
    assert_eq!(local.undo_tree().len(), 1);
    assert_eq!(local.undo(), None);
}
