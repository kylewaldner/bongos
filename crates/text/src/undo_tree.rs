//! The undo history of a buffer, kept as a tree.
//!
//! Every transaction that ends with edits becomes a node. Undoing moves the
//! current node up toward the root; editing after an undo adds a new child of
//! the current node, so the undone transactions stay in the tree as a sibling
//! branch instead of being dropped.
//!
//! The classic undo and redo stacks are views of this tree: the undo stack is
//! the path from the root down to the current node, and the redo stack is the
//! chain of active children below the current node.
//!
//! The tree keeps at most a limited number of nodes. When a new node pushes it
//! over the limit, the oldest leaf that nothing needs is removed, repeatedly.
//! A node is needed when it is on the path from the root to the current node,
//! on the path from the root to a pinned node, or on the chain of active
//! children below the current node, so the text and the redo stack never
//! change by pruning. A removed node is always an undone state; its edits stay
//! in the buffer's CRDT.

use crate::{HistoryEntry, Transaction, TransactionId};
use collections::HashMap;
use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};

type NodeIx = usize;

const ROOT_IX: NodeIx = 0;

/// The node limit of a new tree.
pub const DEFAULT_UNDO_NODE_LIMIT: usize = 10_000;

/// The smallest node limit; lower limits are raised to it.
pub const MIN_UNDO_NODE_LIMIT: usize = 100;

/// Identifies a node of an [`UndoTree`]: the id of the node's transaction, or
/// [`UndoNodeId::ROOT`] for the root, which stands for the base text.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct UndoNodeId(pub TransactionId);

impl UndoNodeId {
    /// The root node. Its id is a lamport timestamp that no clock hands out.
    pub const ROOT: Self = Self(clock::Lamport::MIN);

    pub fn is_root(self) -> bool {
        self == Self::ROOT
    }

    /// The transaction of this node, or `None` for the root.
    pub fn transaction_id(self) -> Option<TransactionId> {
        if self.is_root() { None } else { Some(self.0) }
    }
}

impl From<TransactionId> for UndoNodeId {
    fn from(id: TransactionId) -> Self {
        Self(id)
    }
}

/// One state of the buffer's history: the transaction that leads to it from
/// its parent. The root has no transaction.
#[derive(Clone, Debug)]
pub struct UndoNode {
    entry: Option<HistoryEntry>,
    parent: Option<NodeIx>,
    /// In creation order.
    children: Vec<NodeIx>,
    /// The child that a redo from this node moves to.
    active_child: Option<NodeIx>,
    seq: u64,
}

impl UndoNode {
    pub fn id(&self) -> UndoNodeId {
        self.entry
            .as_ref()
            .map_or(UndoNodeId::ROOT, |entry| UndoNodeId(entry.transaction.id))
    }

    pub fn entry(&self) -> Option<&HistoryEntry> {
        self.entry.as_ref()
    }

    pub fn transaction(&self) -> Option<&Transaction> {
        self.entry.as_ref().map(|entry| &entry.transaction)
    }

    /// Creation order of the node within its tree; the root is 0.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

#[derive(Clone, Debug)]
pub struct UndoTree {
    nodes: Vec<Option<UndoNode>>,
    free: Vec<NodeIx>,
    ids: HashMap<TransactionId, NodeIx>,
    current: NodeIx,
    pins: HashMap<NodeIx, usize>,
    pruned: usize,
    limit: usize,
    next_seq: u64,
    len: usize,
}

impl Default for UndoTree {
    fn default() -> Self {
        Self::new()
    }
}

impl UndoTree {
    pub fn new() -> Self {
        Self {
            nodes: vec![Some(UndoNode {
                entry: None,
                parent: None,
                children: Vec::new(),
                active_child: None,
                seq: 0,
            })],
            free: Vec::new(),
            ids: HashMap::default(),
            current: ROOT_IX,
            pins: HashMap::default(),
            pruned: 0,
            limit: DEFAULT_UNDO_NODE_LIMIT,
            next_seq: 1,
            len: 1,
        }
    }

    // Read API.

    pub fn root(&self) -> UndoNodeId {
        UndoNodeId::ROOT
    }

    /// The node whose state the buffer is in.
    pub fn current(&self) -> UndoNodeId {
        self.id_of(self.current)
    }

    pub fn node(&self, id: UndoNodeId) -> Option<&UndoNode> {
        self.ix_of(id).map(|ix| self.n(ix))
    }

    pub fn parent(&self, id: UndoNodeId) -> Option<UndoNodeId> {
        let ix = self.ix_of(id)?;
        self.n(ix).parent.map(|parent| self.id_of(parent))
    }

    /// The children of a node in creation order; empty for an unknown id.
    pub fn children(&self, id: UndoNodeId) -> Vec<UndoNodeId> {
        self.ix_of(id)
            .map(|ix| {
                self.n(ix)
                    .children
                    .iter()
                    .map(|child| self.id_of(*child))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn active_child(&self, id: UndoNodeId) -> Option<UndoNodeId> {
        let ix = self.ix_of(id)?;
        self.n(ix).active_child.map(|child| self.id_of(child))
    }

    /// The number of nodes, the root included.
    pub fn len(&self) -> usize {
        self.len
    }

    /// The number of nodes removed to keep the tree within its node limit,
    /// over the life of the tree. Resetting the tree does not change it.
    pub fn pruned(&self) -> usize {
        self.pruned
    }

    /// The most nodes the tree keeps, unless every leaf is needed.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The number of edges between the root and the node.
    pub fn depth(&self, id: UndoNodeId) -> Option<usize> {
        let mut ix = self.ix_of(id)?;
        let mut depth = 0;
        while let Some(parent) = self.n(ix).parent {
            depth += 1;
            ix = parent;
        }
        Some(depth)
    }

    /// The transactions to toggle to move the buffer from the state of `from`
    /// to the state of `to`: those from `from` up to the closest common
    /// ancestor (to undo), then those from that ancestor down to `to` (to
    /// redo). Empty when either id is unknown. Takes time linear in the depths
    /// of the two nodes.
    pub fn toggles(&self, from: UndoNodeId, to: UndoNodeId) -> Vec<TransactionId> {
        let (Some(mut from), Some(mut to)) = (self.ix_of(from), self.ix_of(to)) else {
            return Vec::new();
        };
        let mut from_depth = self.ancestors(from).len();
        let mut to_depth = self.ancestors(to).len();
        let mut up = Vec::new();
        let mut down = Vec::new();
        while from_depth > to_depth {
            up.push(from);
            from = self.n(from).parent.expect("deeper than the other node");
            from_depth -= 1;
        }
        while to_depth > from_depth {
            down.push(to);
            to = self.n(to).parent.expect("deeper than the other node");
            to_depth -= 1;
        }
        while from != to {
            up.push(from);
            down.push(to);
            from = self.n(from).parent.expect("the root is a common ancestor");
            to = self.n(to).parent.expect("the root is a common ancestor");
        }
        up.into_iter()
            .chain(down.into_iter().rev())
            .filter_map(|ix| self.transaction_id_of(ix))
            .collect()
    }

    // Views used by the history.

    /// The undo stack, bottom first: the path from the root's child down to
    /// the current node.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn undo_stack(&self) -> Vec<&HistoryEntry> {
        let mut path = self.ancestors(self.current);
        path.pop();
        path.iter()
            .rev()
            .filter_map(|ix| self.n(*ix).entry.as_ref())
            .collect()
    }

    /// The redo stack, bottom first: the active chain below the current node,
    /// deepest first, so the last entry is the next redo.
    #[cfg(test)]
    pub(crate) fn redo_stack(&self) -> Vec<&HistoryEntry> {
        self.redo_chain()
            .iter()
            .rev()
            .filter_map(|ix| self.n(*ix).entry.as_ref())
            .collect()
    }

    pub(crate) fn peek_undo(&self) -> Option<&HistoryEntry> {
        self.n(self.current).entry.as_ref()
    }

    pub(crate) fn peek_redo(&self) -> Option<&HistoryEntry> {
        let next = self.n(self.redo_anchor()).active_child?;
        self.n(next).entry.as_ref()
    }

    pub(crate) fn current_entry_mut(&mut self) -> Option<&mut HistoryEntry> {
        let current = self.current;
        self.n_mut(current).entry.as_mut()
    }

    /// Finds a transaction on the undo or redo view only. Undone sibling
    /// branches are not visible here.
    pub(crate) fn scoped_transaction(&self, id: TransactionId) -> Option<&Transaction> {
        let ix = self.scoped_ix(id)?;
        self.n(ix).entry.as_ref().map(|entry| &entry.transaction)
    }

    pub(crate) fn scoped_transaction_mut(&mut self, id: TransactionId) -> Option<&mut Transaction> {
        let ix = self.scoped_ix(id)?;
        self.n_mut(ix)
            .entry
            .as_mut()
            .map(|entry| &mut entry.transaction)
    }

    // History operations.

    /// Adds a pending child of the current node and makes it current. It
    /// becomes its parent's active child only when it ends with edits.
    pub(crate) fn start_pending(&mut self, entry: HistoryEntry) {
        let parent = self.current;
        let ix = self.alloc(entry, parent);
        self.n_mut(parent).children.push(ix);
        self.current = ix;
    }

    /// Ends the pending node: an empty one is removed, one with edits stays
    /// and becomes its parent's active child, so the old redo chain becomes a
    /// sibling branch. Returns the node's entry when it stays.
    pub(crate) fn end_pending(&mut self) -> Option<&mut HistoryEntry> {
        let ix = self.current;
        let is_empty = self
            .n(ix)
            .entry
            .as_ref()
            .is_none_or(|entry| entry.transaction.edit_ids.is_empty());
        if is_empty {
            if ix != ROOT_IX {
                self.unlink(ix);
                self.release(ix);
            }
            None
        } else {
            if let Some(parent) = self.n(ix).parent {
                self.n_mut(parent).active_child = Some(ix);
            }
            self.n_mut(ix).entry.as_mut()
        }
    }

    pub(crate) fn pop_undo(&mut self) -> Option<&HistoryEntry> {
        let ix = self.current;
        let parent = self.n(ix).parent?;
        self.current = parent;
        self.n(ix).entry.as_ref()
    }

    pub(crate) fn pop_redo(&mut self) -> Option<&HistoryEntry> {
        let next = self.n(self.redo_anchor()).active_child?;
        self.current = next;
        self.n(next).entry.as_ref()
    }

    /// Undoes up to and including `id` if it is on the undo view. Returns the
    /// undone transactions, the most recent first.
    pub(crate) fn undo_until(&mut self, id: TransactionId) -> Vec<Transaction> {
        let Some(&target) = self.ids.get(&id) else {
            return Vec::new();
        };
        if !self.is_on_undo_view(target) {
            return Vec::new();
        }
        let mut undone = Vec::new();
        let mut ix = self.current;
        loop {
            undone.extend(self.n(ix).entry.as_ref().map(|e| e.transaction.clone()));
            if ix == target {
                break;
            }
            ix = self.n(ix).parent.expect("target is an ancestor");
        }
        self.current = self.n(target).parent.expect("target is not the root");
        undone
    }

    /// Redoes down to and including `id` if it is on the redo view. Returns
    /// the redone transactions, the next redo first.
    pub(crate) fn redo_until(&mut self, id: TransactionId) -> Vec<Transaction> {
        let Some(&target) = self.ids.get(&id) else {
            return Vec::new();
        };
        let chain = self.redo_chain();
        let Some(position) = chain.iter().position(|ix| *ix == target) else {
            return Vec::new();
        };
        self.current = target;
        chain[..=position]
            .iter()
            .filter_map(|ix| self.n(*ix).entry.as_ref().map(|e| e.transaction.clone()))
            .collect()
    }

    /// Merges the trailing `count` nodes of the undo view into the node above
    /// them and returns the id of the current node afterwards.
    pub(crate) fn group_trailing(&mut self, count: usize) -> Option<TransactionId> {
        if count > 0 {
            let mut merged = Vec::with_capacity(count);
            let mut target = self.current;
            for _ in 0..count {
                merged.push(target);
                target = self
                    .n(target)
                    .parent
                    .expect("grouping stops below the root");
            }
            let last = self.current;
            let last_edit_at = self.n(last).entry.as_ref().map(|e| e.last_edit_at);
            let mut edit_ids = Vec::new();
            for ix in merged.iter().rev() {
                if let Some(entry) = &self.n(*ix).entry {
                    edit_ids.extend_from_slice(&entry.transaction.edit_ids);
                }
            }
            let children = std::mem::take(&mut self.n_mut(last).children);
            let active_child = self.n(last).active_child;
            for child in &children {
                self.n_mut(*child).parent = Some(target);
            }
            {
                let target_node = self.n_mut(target);
                target_node.children = children;
                target_node.active_child = active_child;
                if let Some(entry) = target_node.entry.as_mut() {
                    entry.transaction.edit_ids.extend(edit_ids);
                    if let Some(last_edit_at) = last_edit_at {
                        entry.last_edit_at = last_edit_at;
                    }
                }
            }
            for ix in merged {
                self.release(ix);
            }
            self.current = target;
        }
        self.transaction_id_of(self.current)
    }

    /// How many trailing nodes `group` merges: walking up from the current
    /// node, each node joins its parent when the two are closer than
    /// `interval`, the parent does not suppress grouping, the parent has no
    /// other child and no pin, and the node itself has no pin.
    pub(crate) fn group_count(&self, interval: Duration) -> usize {
        let mut count = 0;
        let mut ix = self.current;
        while let Some(parent) = self.n(ix).parent {
            let (Some(entry), Some(prev)) = (&self.n(ix).entry, &self.n(parent).entry) else {
                break;
            };
            if !prev.suppress_grouping
                && !self.is_pinned(ix)
                && entry.first_edit_at - prev.last_edit_at < interval
                && self.can_merge_into(parent)
            {
                count += 1;
                ix = parent;
            } else {
                break;
            }
        }
        count
    }

    /// How many trailing nodes `group_until(id)` merges into `id`, or `None`
    /// when it stops before reaching `id`.
    pub(crate) fn group_until_count(&self, id: TransactionId) -> Option<usize> {
        let mut count = 0;
        let mut ix = self.current;
        loop {
            let entry = self.n(ix).entry.as_ref()?;
            if entry.transaction.id == id {
                return Some(count);
            }
            if entry.suppress_grouping || self.is_pinned(ix) {
                return None;
            }
            let parent = self.n(ix).parent?;
            if !self.can_merge_into(parent) {
                return None;
            }
            count += 1;
            ix = parent;
        }
    }

    /// Inserts a new node between the current node and its active child and
    /// makes it current, so the redo chain below is unchanged. An id already
    /// in an undone branch is first removed from there; an id already on the
    /// undo or redo view has its entry replaced in place.
    pub(crate) fn push(&mut self, entry: HistoryEntry) {
        let id = entry.transaction.id;
        if let Some(&existing) = self.ids.get(&id) {
            if self.is_scoped(existing) {
                debug_assert!(false, "transaction {id:?} pushed twice");
                self.n_mut(existing).entry = Some(entry);
                return;
            }
            self.unlink(existing);
            self.release(existing);
        }
        let current = self.current;
        let ix = self.alloc(entry, current);
        self.insert_below_current(ix);
        self.current = ix;
    }

    /// Removes a transaction on the undo or redo view from the tree, giving
    /// its children to its parent.
    pub(crate) fn forget(&mut self, id: TransactionId) -> Option<Transaction> {
        let ix = self.scoped_ix(id)?;
        self.unlink(ix);
        let node = self.release(ix);
        node.entry.map(|entry| entry.transaction)
    }

    /// Undoes a transaction on the undo view out of order: it is moved to be
    /// the next redo, ahead of the old redo chain.
    pub(crate) fn remove_from_undo(&mut self, id: TransactionId) -> Option<&HistoryEntry> {
        let &ix = self.ids.get(&id)?;
        if !self.is_on_undo_view(ix) {
            return None;
        }
        self.unlink(ix);
        self.insert_below_current(ix);
        self.n(ix).entry.as_ref()
    }

    pub(crate) fn is_pinned(&self, ix: NodeIx) -> bool {
        self.pins.get(&ix).is_some_and(|count| *count > 0)
    }

    /// Protects a node, and the path from the root to it, from pruning. Pins
    /// count: a node pinned twice needs two unpins. Returns `false` and does
    /// nothing when the node is not in the tree, for instance because it was
    /// pruned.
    pub(crate) fn pin(&mut self, id: UndoNodeId) -> bool {
        let Some(ix) = self.ix_of(id) else {
            return false;
        };
        *self.pins.entry(ix).or_insert(0) += 1;
        true
    }

    /// Releases one pin of a node. Returns `false` and does nothing when the
    /// node is not in the tree or has no pin.
    pub(crate) fn unpin(&mut self, id: UndoNodeId) -> bool {
        let Some(ix) = self.ix_of(id) else {
            return false;
        };
        match self.pins.get_mut(&ix) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                self.pins.remove(&ix);
            }
            None => return false,
        }
        true
    }

    /// Sets the node limit, raised to [`MIN_UNDO_NODE_LIMIT`] if lower, and
    /// prunes down to it.
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(MIN_UNDO_NODE_LIMIT);
        self.prune();
    }

    /// Removes the oldest unneeded leaf, by creation order, until the tree is
    /// within its limit or no leaf can go. A node left without children
    /// becomes a candidate itself. The tree stays over its limit rather than
    /// lose a node on the path to the current node or a pinned node, or on the
    /// chain of active children below the current node.
    pub(crate) fn prune(&mut self) {
        if self.len <= self.limit {
            return;
        }
        let needed = self.needed_nodes();
        let mut candidates = BinaryHeap::new();
        for (ix, slot) in self.nodes.iter().enumerate() {
            if let Some(node) = slot
                && node.children.is_empty()
                && !needed[ix]
            {
                candidates.push(Reverse((node.seq, ix)));
            }
        }
        while self.len > self.limit {
            let Some(Reverse((_, ix))) = candidates.pop() else {
                break;
            };
            let parent = self.n(ix).parent.expect("the root is always needed");
            self.unlink(ix);
            self.release(ix);
            self.pruned += 1;
            let parent_node = self.n(parent);
            if parent_node.children.is_empty() && !needed[parent] {
                candidates.push(Reverse((parent_node.seq, parent)));
            }
        }
    }

    /// Drops every node but the current one, which becomes the root: the
    /// tree is one node at the current state with no transaction, and all
    /// pins are cleared. The count of pruned nodes is kept, because nothing
    /// is pruned here. The buffer's text and its edits are not touched.
    pub(crate) fn reset(&mut self) {
        let limit = self.limit;
        let pruned = self.pruned;
        *self = Self::new();
        self.limit = limit;
        self.pruned = pruned;
    }

    /// Marks the nodes that pruning must keep: the paths from the root to the
    /// current node, to the end of the active chain below it and to every
    /// pinned node.
    fn needed_nodes(&self) -> Vec<bool> {
        let mut needed = vec![false; self.nodes.len()];
        let mut mark_path = |mut ix: NodeIx| {
            while !needed[ix] {
                needed[ix] = true;
                match self.n(ix).parent {
                    Some(parent) => ix = parent,
                    None => break,
                }
            }
        };
        mark_path(self.current);
        let mut chain_end = self.redo_anchor();
        while let Some(child) = self.n(chain_end).active_child {
            chain_end = child;
        }
        mark_path(chain_end);
        for ix in self.pins.keys() {
            mark_path(*ix);
        }
        needed
    }

    /// Panics unless the arena, the free list, the id map and the parent and
    /// child links agree: one live node per id, no id of a freed node, every
    /// link between live nodes and every active child among its children.
    #[cfg(test)]
    pub(crate) fn assert_consistent(&self) {
        let mut live = 0;
        for (ix, slot) in self.nodes.iter().enumerate() {
            let Some(node) = slot else {
                assert!(
                    self.free.contains(&ix),
                    "slot {ix} is neither live nor free"
                );
                continue;
            };
            live += 1;
            assert!(
                !self.free.contains(&ix),
                "live slot {ix} is on the free list"
            );
            match &node.entry {
                Some(entry) => assert_eq!(
                    self.ids.get(&entry.transaction.id),
                    Some(&ix),
                    "the id map misses node {ix}"
                ),
                None => assert_eq!(ix, ROOT_IX, "only the root has no entry"),
            }
            match node.parent {
                Some(parent) => {
                    let parent = self.nodes[parent].as_ref().expect("live parent");
                    assert_eq!(
                        parent.children.iter().filter(|c| **c == ix).count(),
                        1,
                        "node {ix} is not listed once by its parent"
                    );
                }
                None => assert_eq!(ix, ROOT_IX, "only the root has no parent"),
            }
            for child in &node.children {
                let child = self.nodes[*child].as_ref().expect("live child");
                assert_eq!(child.parent, Some(ix), "child of {ix} points elsewhere");
            }
            if let Some(active) = node.active_child {
                assert!(
                    node.children.contains(&active),
                    "active child of {ix} is not a child"
                );
            }
        }
        assert_eq!(live, self.len);
        assert_eq!(self.ids.len() + 1, self.len, "the id map has stale ids");
        for (id, ix) in &self.ids {
            let node = self.nodes[*ix]
                .as_ref()
                .expect("the id map points at a freed slot");
            assert_eq!(node.id(), UndoNodeId(*id));
        }
        for ix in self.pins.keys() {
            assert!(self.nodes[*ix].is_some(), "pin on a freed slot");
        }
        assert!(self.nodes[self.current].is_some(), "current is freed");
    }

    // Internals.

    fn n(&self, ix: NodeIx) -> &UndoNode {
        self.nodes[ix].as_ref().expect("live node")
    }

    fn n_mut(&mut self, ix: NodeIx) -> &mut UndoNode {
        self.nodes[ix].as_mut().expect("live node")
    }

    fn id_of(&self, ix: NodeIx) -> UndoNodeId {
        self.n(ix).id()
    }

    fn transaction_id_of(&self, ix: NodeIx) -> Option<TransactionId> {
        self.n(ix).entry.as_ref().map(|entry| entry.transaction.id)
    }

    fn ix_of(&self, id: UndoNodeId) -> Option<NodeIx> {
        if id.is_root() {
            Some(ROOT_IX)
        } else {
            self.ids.get(&id.0).copied()
        }
    }

    /// The node and its ancestors, the node first and the root last.
    fn ancestors(&self, mut ix: NodeIx) -> Vec<NodeIx> {
        let mut path = vec![ix];
        while let Some(parent) = self.n(ix).parent {
            path.push(parent);
            ix = parent;
        }
        path
    }

    fn can_merge_into(&self, parent: NodeIx) -> bool {
        self.n(parent).children.len() == 1 && !self.is_pinned(parent)
    }

    /// The node whose active chain is the redo stack: the current node, or
    /// its parent while the current node is a pending transaction that has
    /// not ended yet.
    fn redo_anchor(&self) -> NodeIx {
        let current = self.current;
        match self.n(current).parent {
            Some(parent) if self.n(parent).active_child != Some(current) => parent,
            _ => current,
        }
    }

    /// The redo chain, the next redo first.
    fn redo_chain(&self) -> Vec<NodeIx> {
        let mut chain = Vec::new();
        let mut ix = self.redo_anchor();
        while let Some(child) = self.n(ix).active_child {
            chain.push(child);
            ix = child;
        }
        chain
    }

    fn is_on_undo_view(&self, target: NodeIx) -> bool {
        if target == ROOT_IX {
            return false;
        }
        let mut ix = self.current;
        loop {
            if ix == target {
                return true;
            }
            match self.n(ix).parent {
                Some(parent) => ix = parent,
                None => return false,
            }
        }
    }

    fn is_on_redo_view(&self, target: NodeIx) -> bool {
        let anchor = self.redo_anchor();
        let mut ix = target;
        while let Some(parent) = self.n(ix).parent {
            if self.n(parent).active_child != Some(ix) {
                return false;
            }
            if parent == anchor {
                return true;
            }
            ix = parent;
        }
        false
    }

    fn is_scoped(&self, ix: NodeIx) -> bool {
        self.is_on_undo_view(ix) || self.is_on_redo_view(ix)
    }

    fn scoped_ix(&self, id: TransactionId) -> Option<NodeIx> {
        let &ix = self.ids.get(&id)?;
        self.is_scoped(ix).then_some(ix)
    }

    fn alloc(&mut self, entry: HistoryEntry, parent: NodeIx) -> NodeIx {
        let id = entry.transaction.id;
        debug_assert!(!UndoNodeId(id).is_root(), "the root id is reserved");
        let node = UndoNode {
            entry: Some(entry),
            parent: Some(parent),
            children: Vec::new(),
            active_child: None,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.len += 1;
        let ix = if let Some(ix) = self.free.pop() {
            self.nodes[ix] = Some(node);
            ix
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        };
        self.ids.insert(id, ix);
        ix
    }

    /// Frees a node that is no longer linked into the tree.
    fn release(&mut self, ix: NodeIx) -> UndoNode {
        let node = self.nodes[ix].take().expect("live node");
        if let Some(entry) = &node.entry
            && self.ids.get(&entry.transaction.id) == Some(&ix)
        {
            self.ids.remove(&entry.transaction.id);
        }
        self.pins.remove(&ix);
        self.free.push(ix);
        self.len -= 1;
        node
    }

    /// Takes a node out of the tree, keeping every other node: its children
    /// take its place among its parent's children, its active child takes its
    /// place as the parent's active child, and if it was current its parent
    /// becomes current.
    fn unlink(&mut self, ix: NodeIx) {
        let parent = self.n(ix).parent.expect("the root is never unlinked");
        let children = std::mem::take(&mut self.n_mut(ix).children);
        let active_child = self.n_mut(ix).active_child.take();
        self.n_mut(ix).parent = None;
        for child in &children {
            self.n_mut(*child).parent = Some(parent);
        }
        let parent_node = self.n_mut(parent);
        if let Some(position) = parent_node.children.iter().position(|c| *c == ix) {
            parent_node.children.splice(position..=position, children);
        }
        if parent_node.active_child == Some(ix) {
            parent_node.active_child = active_child;
        }
        if self.current == ix {
            self.current = parent;
        }
    }

    /// Links an unlinked node between the current node and its active child,
    /// which moves under the node with its subtree and stays active there.
    /// The node becomes the current node's active child.
    fn insert_below_current(&mut self, ix: NodeIx) {
        let current = self.current;
        let old_active = self.n_mut(current).active_child.take();
        if let Some(old_active) = old_active {
            self.n_mut(current)
                .children
                .retain(|child| *child != old_active);
            self.n_mut(old_active).parent = Some(ix);
        }
        {
            let node = self.n_mut(ix);
            node.parent = Some(current);
            node.children = old_active.into_iter().collect();
            node.active_child = old_active;
        }
        let current_node = self.n_mut(current);
        current_node.children.push(ix);
        current_node.active_child = Some(ix);
    }
}
