//! The app side of the bongos Slack integration: the `slack` settings and the `slack` actions,
//! hidden from the command palette while `slack.enabled` is false (the default). The actions
//! have no handlers yet, and nothing here reaches the network.

mod actions;
mod settings;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{action_types, init_palette, slack};
pub use settings::SlackSettings;

/// Registers the `slack` settings. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    SlackSettings::register(cx);
}

#[cfg(test)]
mod actions_tests;
