//! Query replace in the editor. The engine finds the matches of a session inside its scope (the
//! region, the rest of the buffer, or the excerpt at point) on a multibuffer snapshot, with their
//! capture groups, one at a time for the questions or in chunks for a `!` batch, which replaces on
//! the main thread one chunk at a time and keeps the window drawing between chunks. The edits make
//! every buffer change of a session and turn them into one undo step. The starters ask for the
//! strings and begin a replace; a batch replaces without asking. The session state machine,
//! the replacement templates and the case transfer live in `bongos_query_replace_model`.

pub mod actions;
pub mod display;
pub mod edits;
pub mod engine;
pub mod prompts;
pub mod session;
pub mod starters;
pub mod view;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod display_tests;
#[cfg(test)]
mod edits_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod prompts_tests;
#[cfg(test)]
mod view_tests;

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

/// The action namespace of the query replace commands.
pub const NAMESPACE: &str = "query_replace";

/// Hides the query replace actions from the command palette and registers the starters and
/// the commands of a running replace.
pub fn init(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| filter.hide_namespace(NAMESPACE));
    prompts::init(cx);
    starters::init(cx);
    session::init(cx);
}
