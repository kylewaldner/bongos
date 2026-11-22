//! The app side of bongos workflows (feature 20): the `workflows` settings, the `workflow`
//! actions (hidden from the command palette while `workflows.enabled` is false), the engine with
//! its run lifecycle, the `workflows` live-modes and needs-you sources, gate toasts and the
//! status bar item. Workflows are on by default, and enabling starts nothing by itself. While
//! `workflows.enabled` is false each handler only says that workflows are off, nothing is read or
//! written and no run starts.

mod actions;
pub mod agent_confinement;
pub mod builtin_merge;
pub mod builtin_nodes;
pub mod builtin_run;
pub mod builtin_wait;
mod commands;
pub mod completion;
pub mod definitions;
pub mod dispatch;
mod dispatch_agent;
mod dispatch_program;
mod engine;
pub mod gates;
pub mod landing;
pub mod lifecycle;
pub mod mcp;
pub mod node_status;
pub mod outcomes;
pub mod picker;
pub mod process;
pub mod project_sources;
pub mod run_view;
pub mod runner_claude;
pub mod runner_codex;
pub mod runner_program;
pub mod runs_panel;
pub mod runtime;
mod settings;
pub mod shipped;
pub mod sources;
pub mod status_item;
pub mod summary;
pub mod task_link;
pub mod worktrees;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{WorkflowToast, action_types, init_palette, register, workflow};
pub use builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
pub use completion::{CompletionHook, CompletionHooks};
pub use engine::{EngineRoots, EngineRunner, SharedLifecycle, WorkflowEngine};
pub use lifecycle::{Refusal, RefusalKind, RunId, RunStatus, StartRequest, WorkflowEvent};
pub use node_status::{NodeStatus, NodeStatusSource, NodeStatusSources};
pub use runs_panel::{RunsPanel, init_panel};
pub use settings::WorkflowsSettings;
pub use status_item::WorkflowStatus;

/// Registers the `workflows` settings and, when `workflows.enabled` is already true, creates the
/// [`WorkflowEngine`]; while it is false nothing is read or written. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    WorkflowsSettings::register(cx);
    landing::register(cx);
    builtin_run::register(cx);
    engine::init(cx);
}

/// Gives this test process a job roots registry of its own, as the app's startup does with its
/// data directory: confining a program node records its writable roots there. The registry lies
/// in a fresh temporary directory, so no two test processes share one, removed when the process
/// exits.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_test_job_roots() {
    static DIRECTORY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    #[cfg(unix)]
    extern "C" fn remove_directory() {
        if let Some(directory) = DIRECTORY.get() {
            std::fs::remove_dir_all(directory).ok();
        }
    }
    let directory = tempfile::TempDir::new()
        .expect("a temporary directory for the job roots registry")
        .keep();
    util::job_roots::init(&directory.join("roots.json")).ok();
    DIRECTORY.set(directory).ok();
    // SAFETY: registers a function that only reads a static and removes a directory.
    #[cfg(unix)]
    unsafe {
        libc::atexit(remove_directory);
    }
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod builtin_run_tests;
#[cfg(test)]
mod builtin_wait_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod dispatch_flow_tests;
#[cfg(test)]
mod dispatch_tests;
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
mod settings_tests;
#[cfg(test)]
mod shipped_tests;
#[cfg(test)]
mod worktree_tests;
