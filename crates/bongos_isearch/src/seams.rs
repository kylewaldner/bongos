//! Where isearch hands off to other features.

use bongos_emacs::mark::push_mark;
use editor::Editor;
use gpui::{App, Entity, Window};
use multi_buffer::Anchor;

/// Pushes the search's origin onto the buffer's mark ring, so popping the mark returns there.
/// Every exit but an abort calls it before it sets the final selection: the push is skipped
/// while mark mode is on, and the session keeps mark mode off.
pub fn push_origin(editor: &Entity<Editor>, origin: Anchor, window: &mut Window, cx: &mut App) {
    push_mark(editor, Some(origin), false, window, cx);
}
