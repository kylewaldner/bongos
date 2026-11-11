//! When a caret's region counts as active, in one place.

use editor::Editor;
use gpui::App;
use multi_buffer::MultiBufferOffset;
use text::Selection;

/// A caret's region is active when mark mode is on (even with an empty selection, right after
/// the mark is set) or its selection is non-empty.
pub fn is_active(editor: &Editor, selection: &Selection<MultiBufferOffset>, _cx: &App) -> bool {
    editor.selection_mark_mode() || selection.start != selection.end
}
