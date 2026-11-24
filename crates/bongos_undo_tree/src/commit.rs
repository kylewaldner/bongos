//! Committing the state under the undo tree's cursor to the real buffer (`RET`).
//!
//! The commit is one undoable step: a new child of the state the buffer was in when the tree
//! opened, whose text is the chosen state's text and whose selections are the ones that state
//! was left with. Undoing it returns to the entry state, and every earlier state stays in the
//! tree. The new text is diffed against the buffer in the background; when the buffer changed
//! meanwhile, nothing is applied. Nothing is saved to disk.

use std::{ops::Range, time::Instant};

use editor::{Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use gpui::{App, Entity, Window};
use language::{Buffer, Diff};

/// The echo when the buffer changed while the commit was being prepared.
pub const NOTHING_COMMITTED_MESSAGE: &str = "Buffer changed; nothing committed";

/// What a commit applies, captured when it starts.
pub struct CommitRequest {
    pub buffer: Entity<Buffer>,
    pub editor: Entity<Editor>,
    /// The real buffer's version when the tree opened; the commit applies only on it.
    pub entry_version: clock::Global,
    /// The chosen state's selections, as offsets in its text.
    pub selections: Vec<Range<MultiBufferOffset>>,
}

impl CommitRequest {
    /// Whether the real buffer is still in the version the commit was prepared against.
    pub fn is_current(&self, cx: &App) -> bool {
        self.buffer.read(cx).version() == self.entry_version
    }

    /// Applies `diff` as one transaction at `now`, kept apart from the transactions around it
    /// even when they are within the buffer's grouping interval, and selects the chosen state's
    /// selections.
    pub fn apply(self, diff: Diff, now: Instant, window: &mut Window, cx: &mut App) {
        let Self {
            buffer,
            editor,
            selections,
            ..
        } = self;
        buffer.update(cx, |buffer, _| {
            buffer.finalize_last_transaction();
        });
        editor.update(cx, |editor, cx| {
            editor.with_selection_effects_deferred(window, cx, |editor, window, cx| {
                editor.start_transaction_at(now, window, cx);
                buffer.update(cx, |buffer, cx| buffer.apply_diff(diff, cx));
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::newest()),
                    window,
                    cx,
                    |s| s.select_ranges(selections),
                );
                editor.end_transaction_at(now, cx);
            });
        });
        buffer.update(cx, |buffer, _| {
            buffer.finalize_last_transaction();
        });
    }
}
