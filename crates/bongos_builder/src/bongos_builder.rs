//! The app side of the bongos task builder: the `task_builder` settings, the `Proposal` language
//! (the major mode of `*.proposal.txt` files, whose defaults save a file exactly as typed), the
//! `task` and `proposal` actions of the builder, hidden while `task_builder.enabled` is false, and
//! the format reference written to `~/.bongos/task-builder/format.txt`. The proposal format and
//! its pure rules live in `bongos_builder_model`, which has no UI dependency.

mod actions;
mod format_file;
mod language;
mod settings;

use std::sync::Arc;

use ::settings::Settings as _;
use fs::Fs;
use gpui::App;

pub use actions::{BuilderToast, action_types, init_palette, proposal, register, task};
pub use bongos_builder_model as model;
pub use format_file::{BuilderDirs, FORMAT_FILE_NAME};
pub use language::{FILE_SUFFIX, NAME as LANGUAGE_NAME, register_language};
pub use settings::TaskBuilderSettings;

/// Registers the `task_builder` settings and writes the format reference, whatever the setting
/// says. Call before [`init_palette`].
pub fn init(fs: Arc<dyn Fs>, dirs: BuilderDirs, cx: &mut App) {
    TaskBuilderSettings::register(cx);
    format_file::write_at_init(fs, dirs, cx);
}

#[cfg(test)]
mod builder_tests;
