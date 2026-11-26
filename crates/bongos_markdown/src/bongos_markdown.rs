//! The app side of the bongos markdown editor: the `markdown` settings, the per-buffer mode
//! (live view or source), the addon that carries it on markdown editors and gives them the live
//! typography, and the `markdown` actions, hidden from the command palette for now. The pure
//! parsing and block logic lives in `bongos_markdown_model`.

mod actions;
mod mode;
mod settings;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{COMMANDS_VISIBLE, action_types, init_palette, markdown};
pub use mode::{
    KEY_CONTEXT, LIVE_FONT_SIZE, LIVE_LINE_HEIGHT, MarkdownAddon, MarkdownModes, Mode, is_attached,
    mode_for_buffer, set_mode,
};
pub use settings::MarkdownSettings;

/// Registers the `markdown` settings, the prefix argument meanings of the actions, their
/// workspace handlers, and watches new editors for the markdown addon. Call before
/// [`init_palette`].
pub fn init(cx: &mut App) {
    MarkdownSettings::register(cx);
    actions::init(cx);
    mode::init(cx);
}

#[cfg(test)]
mod markdown_tests;
