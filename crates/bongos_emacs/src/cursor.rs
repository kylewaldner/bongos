//! `cursor: toggle style`: flips every editor between a block and a bar for the session.
//! Nothing is written to settings; a reload keeps the choice, and an editor with its own
//! `set_cursor_shape_override` keeps that shape.

use editor::CursorStyleOverride;
use gpui::App;

use crate::actions::ToggleStyle;

/// Registers the global `cursor: toggle style` handler.
pub(crate) fn init(cx: &mut App) {
    cx.on_action(|_: &ToggleStyle, cx| CursorStyleOverride::toggle(cx));
}
