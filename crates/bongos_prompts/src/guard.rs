//! The boundary guard (design D10). A join (`backspace` at a body's first column, `C-d` or `C-k`
//! at the end of the row above a marker, `M-d` across it) would pull a marker into a body line and
//! show its text. Over a rule the editor moves by display rows, so some of these take the whole
//! marker line instead, which would merge two blocks without a region. After each local edit
//! transaction that lost a marker either way, the guard undoes exactly that transaction, leaves
//! no redo entry, and says why. Only a region kill may remove a marker line whole: Kyle had a
//! selection that was not empty before the command, and the edit began with it. (`C-k` widens an
//! empty selection itself just before it cuts; that is not a region.) The guard never writes text
//! of its own.
//!
//! So that "exactly that transaction" holds, the guard also keeps the undo groups of a prompts
//! buffer itself: each local edit is finalized, and an edit that follows the previous one within the
//! group interval is merged into it, so a run of typing stays one undo step while a join always
//! opens a transaction of its own.

use std::ops::Range;
use std::time::Instant;

use editor::{Editor, SelectionEffects};
use gpui::{Context, Window};
use language::{Point, TextBufferSnapshot, TransactionId};

use crate::actions::show_toast;
use crate::mode::{PromptsMode, mode_of, sync};

/// The toast of a reverted join.
pub const JOIN_MESSAGE: &str =
    "A rule cannot be joined into a line; select across it to merge boxes";

/// The guard's state in one editor.
#[derive(Default)]
pub(crate) struct GuardState {
    /// A local transaction began and its `Edited` has not come yet. Undo and redo emit `Edited`
    /// without beginning a transaction, which is how the guard tells them apart.
    begun: bool,
    /// The synced batches of edits that lost a marker, since the last `Edited`.
    pub(crate) losses: Vec<Loss>,
    /// The undo step the last local edits were merged into.
    run: Option<TypingRun>,
}

/// A synced batch of edits that lost a marker line.
pub(crate) struct Loss {
    /// The buffer version just before the batch.
    pub(crate) before: clock::Global,
    /// Whether some of a lost marker's text stayed, joined into another line.
    pub(crate) joined: bool,
}

struct TypingRun {
    id: TransactionId,
    /// The buffer version right after the run's last edit.
    after: clock::Global,
    /// When the run's last edit was made.
    at: Instant,
}

/// Whether some offset of `start..end` is not inside any of `ranges` (sorted, disjoint).
fn survives(ranges: &[Range<usize>], start: usize, end: usize) -> bool {
    let mut pos = start;
    for range in ranges {
        if range.end <= pos {
            continue;
        }
        if range.start > pos {
            break;
        }
        pos = range.end;
        if pos >= end {
            return false;
        }
    }
    pos < end
}

fn covers(ranges: &[Range<usize>], offset: usize) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= offset && offset < range.end)
}

/// Whether the edits (`deleted`: the old ranges they replaced, sorted) joined the marker line at
/// `row` of `old` into a neighbor: a line break next to it went while some of its text stayed. A
/// region kill that takes the whole marker line is a merge of two blocks, not a join.
pub(crate) fn is_join(old: &TextBufferSnapshot, deleted: &[Range<usize>], row: u32) -> bool {
    let start = old.point_to_offset(Point::new(row, 0));
    let end = start + old.line_len(row) as usize;
    if !survives(deleted, start, end) {
        return false;
    }
    let break_before = start > 0 && covers(deleted, start - 1);
    let break_after = end < old.len() && covers(deleted, end);
    break_before || break_after
}

/// `EditorEvent::TransactionBegun`: a local edit is under way.
pub(crate) fn on_transaction_begun(mode: &mut PromptsMode) {
    mode.guard.begun = true;
}

/// `EditorEvent::Edited`: reverts the transaction when it was a local join, else keeps the undo
/// groups. Undo and redo (no transaction began) only reset the typing run.
pub(crate) fn on_edited(
    mode: &mut PromptsMode,
    editor: &mut Editor,
    transaction_id: TransactionId,
    window: Option<&mut Window>,
    cx: &mut Context<Editor>,
) {
    sync(mode, editor, cx);
    let buffer = mode.buffer.clone();
    let begun = std::mem::take(&mut mode.guard.begun);
    let losses = std::mem::take(&mut mode.guard.losses);
    let (on_top, start) = {
        let buffer = buffer.read(cx);
        let on_top = buffer
            .peek_undo_stack()
            .is_some_and(|entry| entry.transaction_id() == transaction_id);
        let start = buffer
            .get_transaction(transaction_id)
            .map(|transaction| transaction.start.clone());
        (on_top, start)
    };
    let Some(start) = start.filter(|_| begun && on_top) else {
        mode.guard.run = None;
        return;
    };
    let mut losses = losses
        .iter()
        .filter(|loss| loss.before.observed_all(&start))
        .peekable();
    let lost = losses.peek().is_some();
    let joined = losses.any(|loss| loss.joined);
    let region = mode.region && had_region(editor, transaction_id);
    if joined || (lost && !region) {
        revert(mode, editor, transaction_id, window, cx);
        return;
    }
    let (now, interval, version) = {
        let buffer = buffer.read(cx);
        (
            Instant::now(),
            buffer.transaction_group_interval(),
            buffer.version(),
        )
    };
    let run = match mode.guard.run.take() {
        Some(run) if run.id == transaction_id => run.id,
        // A region kill that merged two blocks stays an undo step of its own.
        Some(run) if !lost && run.after == start && now.duration_since(run.at) < interval => {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.merge_transactions(transaction_id, run.id, cx)
            });
            run.id
        }
        _ => transaction_id,
    };
    editor.finalize_last_transaction(cx);
    mode.guard.run = Some(TypingRun {
        id: run,
        after: version,
        at: now,
    });
}

/// Whether a selection was not empty when the transaction began: the edit acted on a region. The
/// anchors are compared, not resolved, since the edit may have deleted the text between them; the
/// editor anchors both ends of an empty selection alike.
fn had_region(editor: &mut Editor, transaction_id: TransactionId) -> bool {
    let mut region = false;
    editor.modify_transaction_selection_history(transaction_id, |(selections, _)| {
        region = selections
            .iter()
            .any(|selection| selection.start != selection.end);
    });
    region
}

/// Undoes and forgets the join, puts the selections back where they were before it, and says why.
fn revert(
    mode: &mut PromptsMode,
    editor: &mut Editor,
    transaction_id: TransactionId,
    window: Option<&mut Window>,
    cx: &mut Context<Editor>,
) {
    let mut before = None;
    editor.modify_transaction_selection_history(transaction_id, |(selections, _)| {
        before = Some(selections.to_vec());
    });
    editor.buffer().update(cx, |buffer, cx| {
        buffer.undo_transaction(transaction_id, cx);
        buffer.forget_transaction(transaction_id, cx);
    });
    mode.guard.run = None;
    mode.stats.guard_reverts += 1;
    if let (Some(selections), Some(window)) = (before, window) {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_anchors(selections)
        });
    }
    if let Some(workspace) = editor.workspace() {
        workspace.update(cx, |workspace, cx| {
            show_toast(workspace, JOIN_MESSAGE.to_string(), cx)
        });
    }
}

/// Call after the mode's own edits (mark-sent, `prompts: new`), which do not go through an editor
/// transaction: it closes their undo step, so a join typed right after is reverted alone and never
/// takes the edit with it, and starts a new typing run.
pub fn finalize_own_edit(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.finalize_last_transaction(cx);
    if let Some(mode) = mode_of(editor) {
        mode.borrow_mut().guard.run = None;
    }
}
