//! The app side of the bongos task builder: the `task_builder` settings, the `Proposal` language
//! (the major mode of `*.proposal.txt` files, whose defaults save a file exactly as typed), the
//! `task` and `proposal` actions of the builder, hidden while `task_builder.enabled` is false, and
//! the format reference written to `~/.bongos/task-builder/format.txt`, the plan todo of a new task,
//! the plan chat, what every chat is told about proposals, the proposal view, and the preview,
//! apply and reject of a proposal, `task: propose` and the switches of the automatic apply. The
//! proposal format and its pure rules live in `bongos_builder_model`, which has no UI dependency.

mod actions;
mod apply;
mod auto_apply;
mod choose;
mod context;
#[cfg(debug_assertions)]
mod dump;
mod format_file;
mod language;
mod live;
mod plan;
mod preview_view;
mod propose;
mod settings;
mod view;

use std::sync::Arc;

use ::settings::Settings as _;
use fs::Fs;
use gpui::App;

pub use actions::{BuilderToast, action_types, init_palette, proposal, register, task};
pub use auto_apply::{auto_apply_allowed, effective_auto_apply};
pub use bongos_builder_model as model;
pub use context::{MAX_PRINTED_LINES, PLAN_TYPE, plan_section, pointer_section};
pub use format_file::{BuilderDirs, FORMAT_FILE_NAME};
pub use language::{FILE_SUFFIX, NAME as LANGUAGE_NAME, register_language};
pub use plan::plan_todos;
pub use propose::is_untouched;
pub use settings::TaskBuilderSettings;
pub use view::{Mark, ProposalView};

/// Registers the `task_builder` settings, writes the format reference and installs the hooks of
/// the work tree and the chats (the plan todo of a new task, the section of every chat's context),
/// whatever the setting says; the hooks do nothing while `task_builder.enabled` is false. Call
/// before [`init_palette`].
pub fn init(fs: Arc<dyn Fs>, dirs: BuilderDirs, cx: &mut App) {
    TaskBuilderSettings::register(cx);
    plan::install_hooks(cx);
    context::install(dirs.clone(), cx);
    format_file::write_at_init(fs, dirs, cx);
}

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod builder_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod propose_tests;
#[cfg(test)]
mod view_tests;
