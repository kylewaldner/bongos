//! The app side of the bongos Linear sync: the `linear` settings and the `task` and `linear`
//! actions of the sync, hidden from the command palette while `linear.enabled` is false. While it
//! is false each handler only says that the sync is off, and nothing here reaches the network.
//! While it is true the handlers hand their command to the installed [`LinearRequests`], of which
//! this step installs none, so they do nothing yet. `linear: set token` and `linear: clear token`
//! open the masked token modal and clear the stored token instead (see [`token`]). No keys are
//! bound to these actions yet; they are reached from the command palette.

mod actions;
pub(crate) mod client;
pub mod dump;
mod settings;
pub mod sync;
pub mod token;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{
    Command, LinearRequests, LinearToast, action_types, init_palette, install_requests, linear,
    register, task,
};
pub use settings::LinearSettings;

/// Registers the `linear` settings and creates the sync. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    LinearSettings::register(cx);
    sync::install(cx);
}

/// Installs the client that talks to Linear over the application's HTTP client. Call once the
/// HTTP client is set. Until then `linear: set token` cannot check a token and says so.
pub fn init_client(cx: &mut App) {
    token::install_client_factory(
        client::http_factory(cx.http_client(), cx.background_executor().clone()),
        cx,
    );
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod dump_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod token_tests;
