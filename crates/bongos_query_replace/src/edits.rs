//! Every buffer change of a query replace session, and the undo history that makes the whole
//! session one undo step.
//!
//! Each interactive replacement is its own transaction, finalized before and after so it never
//! groups with the edit before or after it (however close in time) and its id stays valid for `u`.
//! A `!` batch is one transaction per chunk of [`crate::engine::SCAN_CHUNK`] matches. When the
//! session ends, [`Edits::finish`] merges every transaction still alive into the oldest one and
//! gives it the selections from before the session, so one undo reverts the whole session and puts
//! the cursor back. An edit made outside the session while it runs stays its own undo entry above
//! the merged one, so the first undo after the session undoes that edit, not the session.

use crate::engine::{self, Hit};
use bongos_isearch::engine::Cancel;
use bongos_query_replace_model::session::Found;
use editor::Editor;
use gpui::{Context, Window};
use language::{Selection, TransactionId};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use project::search::SearchQuery;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

/// The message of [`ReadOnly`].
pub const READ_ONLY: &str = "Buffer is read-only";

/// The editor became read-only during the session, so nothing was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOnly;

impl fmt::Display for ReadOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(READ_ONLY)
    }
}

impl std::error::Error for ReadOnly {}

/// The id the session model keeps for a replacement that made no transaction (an empty match
/// replaced by nothing). A real transaction never has it.
pub const NO_TRANSACTION: u64 = 0;

/// One interactive replacement made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// Its transaction, `None` when nothing changed (an empty match replaced by nothing).
    pub id: Option<TransactionId>,
    /// The inserted text; the start stays before text inserted there later, the end after.
    pub anchors: Range<Anchor>,
    /// The inserted text as offsets right after the edit.
    pub range: Range<usize>,
}

impl Replacement {
    /// The id to hand to the session model's `on_replaced`, and back to [`Edits::undo`].
    pub fn model_id(&self) -> u64 {
        self.id.map_or(NO_TRANSACTION, TransactionId::as_u64)
    }
}

/// What [`Edits::replace`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replace {
    Replaced(Replacement),
    /// The text at the match changed since it was found (an edit made outside the session), so
    /// nothing was replaced. This is the match found again from where the old one started, to be
    /// asked about instead.
    Changed(Option<Found>),
}

/// What [`Edits::apply_chunk`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub id: Option<TransactionId>,
    pub replaced: usize,
    /// Matches left alone: their text changed since the scan, or they had no replacement.
    pub skipped: usize,
}

/// The anchors of a match found on `snapshot`, as the scan makes them: they keep to the matched
/// text when text is inserted at either side of it.
pub fn match_anchors(snapshot: &MultiBufferSnapshot, range: &Range<usize>) -> Range<Anchor> {
    snapshot.anchor_after(MultiBufferOffset(range.start))
        ..snapshot.anchor_before(MultiBufferOffset(range.end))
}

/// The buffer side of one session, from its start to [`Edits::finish`].
pub struct Edits {
    start_selections: Arc<[Selection<Anchor>]>,
    // The session's live transactions, oldest first.
    transactions: Vec<TransactionId>,
    now: Rc<dyn Fn() -> Instant>,
}

impl Edits {
    /// Starts a session on `editor`: the last undo entry is closed, so the first replacement never
    /// joins the user's last typing, and the selections are saved for the undo of the session.
    pub fn start(editor: &mut Editor, cx: &mut Context<Editor>) -> Self {
        Self::start_with_clock(editor, Rc::new(Instant::now), cx)
    }

    /// [`Edits::start`] with the clock the transactions are stamped with.
    pub fn start_with_clock(
        editor: &mut Editor,
        now: Rc<dyn Fn() -> Instant>,
        cx: &mut Context<Editor>,
    ) -> Self {
        editor.finalize_last_transaction(cx);
        Self {
            start_selections: editor.selections.disjoint_anchors_arc(),
            transactions: Vec::new(),
            now,
        }
    }

    /// The selections from before the session.
    pub fn start_selections(&self) -> &Arc<[Selection<Anchor>]> {
        &self.start_selections
    }

    /// The session's transactions not undone, oldest first.
    pub fn transactions(&self) -> &[TransactionId] {
        &self.transactions
    }

    /// Replaces the match `found` at `anchors` with `with` as one undo entry. The match is first
    /// found again from its start: when the text there is no longer that match, nothing changes
    /// and the match found instead is returned.
    #[allow(clippy::too_many_arguments)]
    pub fn replace(
        &mut self,
        editor: &mut Editor,
        query: &SearchQuery,
        scope: &Range<Anchor>,
        anchors: Range<Anchor>,
        found: &Found,
        with: &str,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Result<Replace, ReadOnly> {
        if editor.read_only(cx) {
            return Err(ReadOnly);
        }
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let start = anchors.start.to_offset(&snapshot);
        let end = anchors.end.to_offset(&snapshot).max(start);
        let again = engine::next(&snapshot, query, start, scope, &Cancel::new());
        let unchanged = again
            .as_ref()
            .is_some_and(|again| again.range == (start.0..end.0) && again.groups == found.groups);
        if !unchanged {
            return Ok(Replace::Changed(again));
        }

        let new_anchors = snapshot.anchor_before(start)..snapshot.anchor_after(end);
        let with: Arc<str> = with.into();
        let id = self.transact(editor, window, cx, |editor, cx| {
            editor.edit([(start..end, with)], cx)
        });
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let range =
            new_anchors.start.to_offset(&snapshot).0..new_anchors.end.to_offset(&snapshot).0;
        Ok(Replace::Replaced(Replacement {
            id,
            anchors: new_anchors,
            range,
        }))
    }

    /// Undoes the replacement whose model id is `id` (`u`) and forgets it, so a later redo never
    /// brings it back. False when the session has no such transaction.
    pub fn undo(
        &mut self,
        id: u64,
        editor: &mut Editor,
        cx: &mut Context<Editor>,
    ) -> Result<bool, ReadOnly> {
        if editor.read_only(cx) {
            return Err(ReadOnly);
        }
        let Some(index) = self
            .transactions
            .iter()
            .position(|transaction| transaction.as_u64() == id)
        else {
            return Ok(false);
        };
        let transaction = self.transactions.remove(index);
        editor.buffer().update(cx, |buffer, cx| {
            buffer.undo_transaction(transaction, cx);
            buffer.forget_transaction(transaction, cx);
        });
        Ok(true)
    }

    /// Replaces one chunk of a `!` batch as one undo entry, with a single edit of all its ranges.
    /// A match whose text is no longer what the scan found, or that has no replacement, is left
    /// alone. Meant to be called chunk by chunk from a task that yields between them.
    pub fn apply_chunk(
        &mut self,
        editor: &mut Editor,
        chunk: Vec<Hit>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Result<Applied, ReadOnly> {
        if editor.read_only(cx) {
            return Err(ReadOnly);
        }
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut skipped = 0;
        let mut ranges = Vec::with_capacity(chunk.len());
        for hit in chunk {
            let start = hit.anchors.start.to_offset(&snapshot);
            let end = hit.anchors.end.to_offset(&snapshot).max(start);
            let Some(replacement) = hit.replacement else {
                skipped += 1;
                continue;
            };
            let current = snapshot.text_for_range(start..end);
            if !current.flat_map(str::chars).eq(hit.found.matched().chars()) {
                skipped += 1;
                continue;
            }
            ranges.push((start..end, Arc::<str>::from(replacement)));
        }
        let replaced = ranges.len();
        let id = if ranges.is_empty() {
            None
        } else {
            self.transact(editor, window, cx, |editor, cx| editor.edit(ranges, cx))
        };
        Ok(Applied {
            id,
            replaced,
            skipped,
        })
    }

    /// Ends the session (every way it ends): the live transactions are merged, newest first, into
    /// the oldest of them, which then restores the selections from before the session when undone
    /// and the final selections when redone. Returns that transaction.
    pub fn finish(self, editor: &mut Editor, cx: &mut Context<Editor>) -> Option<TransactionId> {
        let (&destination, newer) = self.transactions.split_first()?;
        editor.buffer().update(cx, |buffer, cx| {
            for &transaction in newer.iter().rev() {
                buffer.merge_transactions(transaction, destination, cx);
            }
        });
        let end_selections = editor.selections.disjoint_anchors_arc();
        let start_selections = self.start_selections;
        editor.modify_transaction_selection_history(destination, |(start, end)| {
            *start = start_selections;
            *end = Some(end_selections);
        });
        Some(destination)
    }

    /// `edit` as one transaction stamped with the session clock, closed on both sides so it is
    /// its own undo entry. This is `Editor::transact` with the time taken from the clock.
    fn transact(
        &mut self,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
        edit: impl FnOnce(&mut Editor, &mut Context<Editor>),
    ) -> Option<TransactionId> {
        editor.finalize_last_transaction(cx);
        let now = (self.now)();
        let id = editor.with_selection_effects_deferred(window, cx, |editor, window, cx| {
            editor.start_transaction_at(now, window, cx);
            edit(editor, cx);
            editor.end_transaction_at(now, cx)
        });
        editor.finalize_last_transaction(cx);
        if let Some(id) = id
            && self.transactions.last() != Some(&id)
        {
            self.transactions.push(id);
        }
        id
    }
}
