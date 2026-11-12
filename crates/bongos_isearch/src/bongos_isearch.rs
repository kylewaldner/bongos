//! Incremental search (isearch) on top of the editor. This crate holds the search engine, which
//! finds and counts matches of a compiled search pattern in a multibuffer snapshot in windows, so
//! a search can be canceled between them. The search state and its text logic live in
//! `bongos_isearch_model`.

pub mod engine;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod perf_tests;

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

pub use engine::{
    Cancel, Count, Engine, FindOutcome, InvalidPattern, count, find, find_outcome, in_range,
};

/// The action namespace of the isearch commands.
pub const NAMESPACE: &str = "isearch";

/// Hides the `isearch` actions from the command palette: they only make sense inside a search.
pub fn init(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_namespace(NAMESPACE);
    });
}
