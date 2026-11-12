//! Incremental search (isearch) on top of the editor. This crate holds the `isearch` actions, the
//! search engine, which finds and counts matches of a compiled search pattern in a multibuffer
//! snapshot in windows, so a search can be canceled between them, and the session, which hosts a
//! search in the minibuffer and moves the editor's cursor with every change of the string. The
//! search state and its text logic live in `bongos_isearch_model`.

pub mod actions;
pub mod engine;
pub mod seams;
pub mod session;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod session_tests;

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

pub use engine::{
    Cancel, Count, Engine, FindOutcome, InvalidPattern, count, find, find_outcome, in_range,
};

/// The action namespace of the isearch commands.
pub const NAMESPACE: &str = "isearch";

/// Registers the starters on every editor and the in-session commands on every workspace, and
/// hides the `isearch` actions from the command palette until the feature is turned on.
pub fn init(cx: &mut App) {
    session::init(cx);
    CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_namespace(NAMESPACE);
    });
}
