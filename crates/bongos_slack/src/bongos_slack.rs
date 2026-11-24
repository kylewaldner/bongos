//! The app side of the bongos Slack integration: the `slack` settings and the `slack` actions,
//! hidden from the command palette while `slack.enabled` is false (the default), the HTTP
//! client of the Slack Web API (`client`), the connection (`connection`, `token_prompt`):
//! `slack: connect` and `slack: disconnect`, the token in the secret store and the files beside
//! it (`allowed`), the service every request goes through (`service`), and the status bar item
//! with `slack: status` (`status_item`).

mod actions;
pub mod allowed;
pub mod client;
pub mod connection;
pub mod read_buffer;
mod read_commands;
pub mod read_view;
pub mod service;
mod settings;
pub mod status_item;
pub mod token_prompt;

use ::settings::Settings as _;
use gpui::App;
use workspace::Workspace;

pub use actions::{action_types, init_palette, register, slack};
pub use service::{SlackError, SlackService, SlackState};
pub use settings::SlackSettings;
pub use status_item::SlackStatusItem;

/// Registers the `slack` settings, reads the connection state from `connection.toml` (the
/// credential store is not touched), installs the Slack client over the application's HTTP
/// client and the Slack service, and registers the action handlers on every workspace. Nothing
/// here sends a request. Call once the HTTP client is set and before [`init_palette`].
pub fn init(cx: &mut App) {
    SlackSettings::register(cx);
    connection::install(connection::default_dir(), cx);
    client::init_client(cx);
    SlackService::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod connection_tests;
#[cfg(test)]
mod read_buffer_tests;
#[cfg(test)]
mod read_view_tests;
#[cfg(test)]
mod service_tests;
