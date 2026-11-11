//! A bounded ring of marks, oldest first and newest last. Generic over the
//! entry type; the caller decides when two entries are the same place.

use std::collections::VecDeque;

/// The most entries the ring keeps.
pub const MARK_RING_MAX: usize = 16;

#[derive(Debug, Clone)]
pub struct MarkRing<A> {
    entries: VecDeque<A>,
}

impl<A> Default for MarkRing<A> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }
}

impl<A> MarkRing<A> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `a` unless `same(newest, &a)` holds. Drops the oldest entry
    /// when the ring would exceed `MARK_RING_MAX`. Returns whether it changed.
    pub fn push(&mut self, a: A, same: impl FnOnce(&A, &A) -> bool) -> bool {
        if let Some(newest) = self.entries.back()
            && same(newest, &a)
        {
            return false;
        }
        if self.entries.len() >= MARK_RING_MAX {
            self.entries.pop_front();
        }
        self.entries.push_back(a);
        true
    }

    /// Removes and returns the newest entry.
    pub fn pop(&mut self) -> Option<A> {
        self.entries.pop_back()
    }

    /// Keeps only the entries for which `live` holds, preserving order.
    pub fn retain(&mut self, live: impl FnMut(&A) -> bool) {
        self.entries.retain(live);
    }

    pub fn iter_newest_first(&self) -> impl Iterator<Item = &A> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
