//! The undo tree: a view of a buffer's undo history as a tree, in the manner of Emacs `vundo`.
//! This crate holds the `undo_tree` actions and settings, the quiet preview of undo states and
//! the conversion of a buffer's undo history to a tree shape; the shape of the tree and the
//! wording of its times are in `bongos_undo_tree_model`.

pub mod actions;
pub mod adapter;
pub mod grouping;
pub mod preview;
pub mod settings;

#[cfg(test)]
mod grouping_tests;
#[cfg(test)]
mod preview_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod test_support;

use bongos_emacs::prefix_arg::{Meaning, declare};
use gpui::App;

pub use settings::{UndoGrouping, UndoTreeSettings};

/// Declares that the `undo_tree` actions ignore a prefix argument, and turns on the undo grouping
/// the `undo_tree.grouping` setting asks for.
pub fn init(cx: &mut App) {
    grouping::init(cx);
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
}
