//! The app side of the bongos Slack integration: the `slack` settings and the `slack` actions,
//! hidden from the command palette while `slack.enabled` is false (the default), the HTTP
//! client of the Slack Web API (`client`), and the connection (`connection`, `token_prompt`):
//! `slack: connect` and `slack: disconnect`, the token in the secret store and the files beside
//! it (`allowed`).

mod actions;
pub mod allowed;
pub mod client;
pub mod connection;
mod settings;
pub mod token_prompt;

use ::settings::Settings as _;
use gpui::App;
use workspace::Workspace;

pub use actions::{action_types, init_palette, register, slack};
pub use settings::SlackSettings;

/// Registers the `slack` settings, reads the connection state from `connection.toml` (the
/// credential store is not touched), installs the Slack client over the application's HTTP client
/// and registers the action handlers on every workspace. Call once the HTTP client is set and
/// before [`init_palette`].
pub fn init(cx: &mut App) {
    SlackSettings::register(cx);
    connection::install(connection::default_dir(), cx);
    client::init_client(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod connection_tests;
