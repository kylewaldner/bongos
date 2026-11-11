//! `cursor: toggle style`: flips every editor between a block and a bar for the session.
//! Nothing is written to settings; a reload keeps the choice, and an editor with its own
//! `set_cursor_shape_override` keeps that shape.

use editor::{CursorStyleOverride, EditorSettings};
use gpui::App;
use language::CursorShape;
use settings::Settings as _;

use crate::actions::ToggleStyle;

/// Registers the global `cursor: toggle style` handler.
pub(crate) fn init(cx: &mut App) {
    cx.on_action(|_: &ToggleStyle, cx| toggle(cx));
}

/// Makes the shape the editors draw now (the previous toggle, else the settings) the other of
/// block and bar.
fn toggle(cx: &mut App) {
    let current = CursorStyleOverride::get(cx)
        .or(EditorSettings::get_global(cx).cursor_shape)
        .unwrap_or_default();
    let next = if current == CursorShape::Block {
        CursorShape::Bar
    } else {
        CursorShape::Block
    };
    CursorStyleOverride::set(next, cx);
}
