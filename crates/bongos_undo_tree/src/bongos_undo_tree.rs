//! The undo tree: a view of a buffer's undo history as a tree, in the manner of Emacs `vundo`.
//! This crate holds the `undo_tree` actions and settings, the panel that shows the tree beside
//! its buffer, the quiet preview of undo states and the conversion of a buffer's undo history to
//! a tree shape; the shape of the tree and the
//! wording of its times are in `bongos_undo_tree_model`.

pub mod actions;
pub mod adapter;
pub mod grouping;
pub mod limit;
pub mod open;
pub mod panel;
pub mod preview;
pub mod settings;

#[cfg(test)]
mod grouping_tests;
#[cfg(test)]
mod limit_tests;
#[cfg(test)]
mod panel_tests;
#[cfg(test)]
mod preview_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod test_support;

use bongos_emacs::prefix_arg::{Meaning, declare};
use gpui::App;
use workspace::Workspace;

pub use settings::{UndoGrouping, UndoTreeSettings};

/// Declares that the `undo_tree` actions ignore a prefix argument, turns on the undo grouping
/// the `undo_tree.grouping` setting asks for, and registers `undo_tree::Vundo` in every
/// workspace. The panel's own actions are handled by the panel.
pub fn init(cx: &mut App) {
    grouping::init(cx);
    limit::init(cx);
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        register(workspace);
    })
    .detach();
}

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(open::vundo);
}
