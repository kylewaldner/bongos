//! The jump to a chosen candidate. The origin is pushed on its buffer's mark ring first (unless
//! mark mode is on there, which makes the jump extend the region), then the target editor gets a
//! single selection at the candidate and, when it is another editor, its pane takes the focus.
//! Nothing scrolls, nothing is edited and mark mode is only read.

use bongos_emacs::mark::push_mark;
use bongos_window::targets::targets;
use editor::{Editor, SelectionEffects, ToOffset as _};
use gpui::{App, Entity, Focusable as _, Window};

use crate::visible::Candidate;

/// Jumps from the editor `origin` to `target`. Does nothing when the target's editor is gone
/// (its pane was closed after the labels were drawn).
pub fn jump(origin: Entity<Editor>, target: Candidate, window: &mut Window, cx: &mut App) {
    let Some(editor) = target.editor.upgrade() else {
        return;
    };
    push_mark(&origin, None, false, window, cx);
    let same_editor = editor == origin;
    if !same_editor {
        keep_only_primary(&origin, window, cx);
    }
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = target.range.start.to_offset(&snapshot);
        let tail = if editor.selection_mark_mode() {
            editor
                .selections
                .newest_anchor()
                .tail()
                .to_offset(&snapshot)
        } else {
            head
        };
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([tail..head]);
        });
    });
    if !same_editor {
        focus(&editor, window, cx);
    }
}

/// Removes every selection of `editor` but the newest, which stays as it is.
fn keep_only_primary(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        if editor.selections.count() <= 1 {
            return;
        }
        let primary = editor.selections.newest_anchor().clone();
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_anchors(vec![primary]);
        });
    });
}

/// Focuses the pane whose active item is `editor`, or the editor itself when no pane of its
/// workspace shows it.
fn focus(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let pane = editor.read(cx).workspace().and_then(|workspace| {
        targets(workspace.read(cx), window, cx)
            .into_iter()
            .find(|pane| {
                pane.read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<Editor>())
                    .is_some_and(|item| item == *editor)
            })
    });
    match pane {
        Some(pane) => window.focus(&pane.focus_handle(cx), cx),
        None => window.focus(&editor.focus_handle(cx), cx),
    }
}
