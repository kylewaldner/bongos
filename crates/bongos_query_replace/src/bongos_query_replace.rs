//! Query replace in the editor. The engine finds the matches of a session inside its scope (the
//! region, the rest of the buffer, or the excerpt at point) on a multibuffer snapshot, with their
//! capture groups, one at a time for the questions or in chunks for a `!` batch, which replaces on
//! the main thread one chunk at a time and keeps the window drawing between chunks. The edits make
//! every buffer change of a session and turn them into one undo step. A search that ends in a query replace hands over its string and settings, and only the
//! replacement is asked. A key that is not the
//! decision view's ends the session and runs in the editor. The starters ask for the
//! strings and begin a replace; a batch replaces without asking. The session state machine,
//! the replacement templates and the case transfer live in `bongos_query_replace_model`.

pub mod actions;
pub mod display;
pub mod edits;
pub mod engine;
mod intercept;
mod isearch;
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
mod intercept_tests;
#[cfg(test)]
mod isearch_tests;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod prompts_tests;
#[cfg(test)]
mod view_tests;

use gpui::App;

/// The action namespace of the query replace commands.
pub const NAMESPACE: &str = "query_replace";

/// Registers the starters and the commands of a running replace. The actions stay visible in the
/// command palette, so `M-x replace-string` and the other starters work.
pub fn init(cx: &mut App) {
    prompts::init(cx);
    starters::init(cx);
    isearch::init(cx);
    session::init(cx);
}
