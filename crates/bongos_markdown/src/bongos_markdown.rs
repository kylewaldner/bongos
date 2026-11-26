//! The app side of the bongos markdown editor: the `markdown` settings, the per-buffer mode
//! (live view or source), the addon that carries it on markdown editors and gives them the live
//! typography, the background parse of the buffer's index, the live layers (inline styles for
//! now), and the `markdown` actions (block moves, insert block, checkbox toggle and table cell
//! moves), hidden from the command palette for now. The pure parsing and block logic lives in
//! `bongos_markdown_model`.

mod actions;
mod commands;
mod index;
mod layers;
mod mode;
mod settings;
mod styles;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{COMMANDS_VISIBLE, action_types, init_palette, markdown};
pub use commands::{
    END_OF_TABLE, HOST, INSERT_PROMPT, NO_BLOCK_ABOVE, NO_BLOCK_BELOW, NOT_A_TASK, NOT_MARKDOWN,
};
pub use index::{IndexState, ParseStats};
pub use layers::{FALLBACK_SCREEN_ROWS, LiveContext, LiveLayer, default_layers, window};
pub use mode::{
    KEY_CONTEXT, LIVE_FONT_SIZE, LIVE_LINE_HEIGHT, Live, MarkdownAddon, MarkdownModes, Mode,
    TABLE_KEY_CONTEXT, fresh_index, is_attached, mode_for_buffer, parse_stats, set_mode,
};
pub use settings::MarkdownSettings;
pub use styles::{MarkdownCodeBlock, Style, StyleLayer};

/// Registers the `markdown` settings, the prefix argument meanings of the actions, their
/// workspace handlers, and watches new editors for the markdown addon. Call before
/// [`init_palette`].
pub fn init(cx: &mut App) {
    MarkdownSettings::register(cx);
    actions::init(cx);
    mode::init(cx);
}

#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod markdown_tests;
