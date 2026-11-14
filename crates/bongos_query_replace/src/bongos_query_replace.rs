//! Query replace in the editor. The engine finds the matches of a session inside its scope (the
//! region, the rest of the buffer, or the excerpt at point) on a multibuffer snapshot, with their
//! capture groups, one at a time for the questions or in chunks for a `!` batch. The edits make
//! every buffer change of a session and turn them into one undo step. The session state machine,
//! the replacement templates and the case transfer live in `bongos_query_replace_model`.

pub mod edits;
pub mod engine;

#[cfg(test)]
mod edits_tests;
#[cfg(test)]
mod engine_tests;

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

/// The action namespace of the query replace commands.
pub const NAMESPACE: &str = "query_replace";

/// Hides the query replace actions from the command palette.
pub fn init(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| filter.hide_namespace(NAMESPACE));
}
