//! When a caret's region counts as active. Feature 08 redefines it here, in one place.

use editor::Editor;
use gpui::App;
use multi_buffer::MultiBufferOffset;
use text::Selection;

/// A caret's region is active when its selection is non-empty.
pub fn is_active(_editor: &Editor, selection: &Selection<MultiBufferOffset>, _cx: &App) -> bool {
    selection.start != selection.end
}
