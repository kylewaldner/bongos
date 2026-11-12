//! The app side of bongos workflows (feature 20): the `workflows` settings, the `workflow`
//! actions (hidden from the command palette while `workflows.enabled` is false), the engine with
//! its run lifecycle, and the `workflows` live-modes source. While `workflows.enabled` is false
//! each handler only says that workflows are off, nothing is read or written and no run starts.

mod actions;
pub mod builtin_merge;
pub mod builtin_wait;
mod commands;
pub mod definitions;
mod engine;
pub mod lifecycle;
pub mod mcp;
pub mod process;
pub mod runner_claude;
pub mod runner_codex;
pub mod runner_program;
pub mod runtime;
mod settings;
pub mod sources;
pub mod worktrees;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{WorkflowToast, action_types, init_palette, register, workflow};
pub use engine::{EngineRunner, SharedLifecycle, WorkflowEngine};
pub use lifecycle::{Refusal, RefusalKind, RunId, RunStatus, StartRequest, WorkflowEvent};
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
mod builtin_wait_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod program_tests;
#[cfg(test)]
mod runner_claude_tests;
#[cfg(test)]
mod runner_codex_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod worktree_tests;
