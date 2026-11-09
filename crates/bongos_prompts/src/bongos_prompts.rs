//! The app side of the bongos prompts editor: the `prompts` settings, the `Prompts` language (the
//! major mode of `prompts.txt` files, whose defaults save a file exactly as typed) and the
//! `prompts` actions, hidden while `prompts.enabled` is false. The file formats and pure rules
//! live in `bongos_prompts_model`, which has no UI dependency.

mod actions;
mod language;
mod settings;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{PromptsToast, action_types, init_palette, prompts, register};
pub use bongos_prompts_model as model;
pub use language::{FILE_NAME, NAME as LANGUAGE_NAME, register_language};
pub use settings::PromptsSettings;

/// Registers the `prompts` settings. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    PromptsSettings::register(cx);
}

#[cfg(test)]
mod prompts_tests;
