//! The app side of bongos workflows (feature 20): the `workflows` settings and the `workflow`
//! actions, hidden from the command palette while `workflows.enabled` is false. While it is false
//! each handler only says that workflows are off, nothing is read or written and no run starts.

mod actions;
mod commands;
pub mod definitions;
mod engine;
mod settings;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{WorkflowToast, action_types, init_palette, register, workflow};
pub use engine::WorkflowEngine;
pub use settings::WorkflowsSettings;

/// Registers the `workflows` settings and, when `workflows.enabled` is already true, creates the
/// [`WorkflowEngine`]; while it is false nothing is read or written. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    WorkflowsSettings::register(cx);
    engine::init(cx);
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod definitions_tests;
