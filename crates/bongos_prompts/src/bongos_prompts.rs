//! The app side of the bongos prompts editor: the `prompts` settings, the `Prompts` language (the
//! major mode of `prompts.txt` files, whose defaults save a file exactly as typed), the prompts
//! mode (an editor addon that draws marker lines as rules, brightens the current block, dims sent
//! ones, keeps the mode line, moves point off the rules and reverts edits that would join a rule
//! into a line), the prompts pane provider of the Chats panel, and the `prompts`
//! actions (navigation, new, copy, send and the auto-submit toggle among them), hidden while `prompts.enabled` is false. The
//! file formats and pure rules live in `bongos_prompts_model`, which has no UI dependency.

mod actions;
mod attach;
mod commands;
mod display;
#[cfg_attr(not(debug_assertions), allow(dead_code))]
mod dump;
mod guard;
mod language;
mod mode;
mod mode_line;
mod provider;
mod send;
mod settings;
mod skip;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{PromptsToast, action_types, init_palette, prompts, register};
pub use bongos_prompts_model as model;
pub use display::{PromptsCurrent, rule_label};
pub use dump::{dump_path, dump_text};
pub use guard::{JOIN_MESSAGE, finalize_own_edit};
pub use language::{FILE_NAME, NAME as LANGUAGE_NAME, register_language};
pub use mode::{KEY_CONTEXT, ModeStats, PromptsAddon, PromptsMode, is_attached, mode_of};
pub use mode_line::{PromptsModeLine, mode_line};
pub use provider::PromptsPaneProvider;
pub use settings::PromptsSettings;

/// Registers the `prompts` settings, watches new editors for the prompts mode (which attaches only
/// while `prompts.enabled` is true) and installs the prompts pane provider, whatever the setting
/// says. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    PromptsSettings::register(cx);
    mode::init(cx);
    provider::install(cx);
}

#[cfg(test)]
mod attach_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod mode_tests;
#[cfg(test)]
mod prompts_tests;
#[cfg(test)]
mod send_tests;
#[cfg(test)]
mod skip_tests;
