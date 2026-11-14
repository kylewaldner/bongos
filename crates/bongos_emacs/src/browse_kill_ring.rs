//! Browse-kill-ring. Lists the kill ring in the minibuffer, newest first, narrowed by a
//! case-insensitive substring, with the highlighted entry previewed under the list. Enter
//! inserts the chosen entry: right after a yank it replaces the yanked text, as a yank-pop does,
//! and otherwise it inserts at point, as a yank does. C-g changes nothing.

use bongos_emacs_model::kill_ring::Entry;
use bongos_minibuffer::{Candidate, Outcome, Session, Source, Value, message, open};
use editor::{Editor, kill_ring};
use futures::channel::mpsc;
use gpui::{Action as _, App, Entity, Focusable as _, SharedString, Window};

use crate::actions::{BrowseKillRing, Yank};
use crate::last_command::record;
use crate::yank::{replace_yanked, yank_entry, yank_state, yanked_here};

/// The name the prompt is routed under (`minibuffer.enabled_for`) and its key context.
const MINIBUFFER_NAME: &str = "kill_ring";
const KEY_CONTEXT: &str = "KillRing";

/// What a row of the list stands for: its ring index, a copy of the entry (so the confirm inserts
/// what was listed even if the ring changed meanwhile) and the text of its preview.
pub(super) struct Row {
    pub(super) index: usize,
    pub(super) entry: Entry,
    pub(super) preview: String,
}

/// The rows for `query`, newest first, a repeated text only once.
pub(super) fn candidates(query: &str, cx: &App) -> Vec<Candidate> {
    let Some(ring) = kill_ring::read(cx) else {
        return Vec::new();
    };
    ring.picker_view(query)
        .into_iter()
        .filter_map(|row| {
            let entry = ring.get(row.index)?.clone();
            Some(Candidate::new(
                row.label,
                Row {
                    index: row.index,
                    entry,
                    preview: row.preview,
                },
            ))
        })
        .collect()
}

/// The text under the list for the highlighted row.
fn detail(value: &Value, _: &App) -> Option<SharedString> {
    value
        .downcast_ref::<Row>()
        .map(|row| row.preview.clone().into())
}

pub(crate) fn browse_kill_ring_command(
    _: &BrowseKillRing,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if editor.read(cx).read_only(cx) {
        message(window, cx, "Buffer is read-only");
        return;
    }
    kill_ring::capture_clipboard(cx);
    if kill_ring::read(cx).is_none_or(|ring| ring.is_empty()) {
        message(window, cx, "Kill ring is empty");
        return;
    }
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let after_yank = yanked_here(editor, window, cx).is_some();
    let on_confirm = editor.downgrade();
    let on_cancel = editor.downgrade();
    let session = Session::completing_read(MINIBUFFER_NAME, "Kill ring: ")
        .key_context(KEY_CONTEXT)
        .source(Source::Dynamic(Box::new(|query, _cancel, cx| {
            let (mut sender, receiver) = mpsc::channel(1);
            sender.try_send(candidates(&query, cx)).ok();
            receiver
        })))
        .detail(detail)
        .on_confirm(move |outcome, window, cx| {
            let Some(editor) = on_confirm.upgrade() else {
                return;
            };
            window.focus(&editor.focus_handle(cx), cx);
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(row) = value.downcast_ref::<Row>() else {
                return;
            };
            insert(&editor, row, after_yank, window, cx);
        })
        .on_cancel(move |window, cx| {
            if let Some(editor) = on_cancel.upgrade() {
                window.focus(&editor.focus_handle(cx), cx);
            }
        });
    workspace.update(cx, |workspace, cx| {
        open(workspace, session, window, cx);
    });
}

/// Inserts the chosen row: over the yanked text when the prompt was opened right after a yank
/// that is still there, else at point.
fn insert(editor: &Entity<Editor>, row: &Row, after_yank: bool, window: &mut Window, cx: &mut App) {
    if editor.read(cx).read_only(cx) {
        message(window, cx, "Buffer is read-only");
        return;
    }
    let replaced = after_yank
        && yank_state(editor, window, cx).is_some_and(|(ranges, _)| {
            replace_yanked(editor, row.index, &row.entry, ranges, window, cx)
        });
    if !replaced {
        yank_entry(editor, row.index, &row.entry, window, cx);
    }
    // The Enter key's own action is recorded after this handler returns; record the insert as a
    // yank once it has, so that a yank-pop right after continues from the chosen entry.
    window.defer(cx, |window, cx| {
        record(window, Some(Yank::name_for_type()), cx);
    });
}
