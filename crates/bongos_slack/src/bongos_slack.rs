//! The app side of the bongos Slack integration: the `slack` settings and the `slack` actions,
//! hidden from the command palette while `slack.enabled` is false (the default), and the HTTP
//! client of the Slack Web API (`client`). The actions have no handlers yet, so nothing sends a
//! request yet.

mod actions;
pub mod client;
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
#[cfg(test)]
mod client_tests;
