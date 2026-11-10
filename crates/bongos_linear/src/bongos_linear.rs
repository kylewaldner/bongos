//! The app side of the bongos Linear sync: the `linear` settings and the `task` and `linear`
//! actions of the sync, hidden from the command palette while `linear.enabled` is false. While it
//! is false each handler only says that the sync is off, and nothing here reaches the network.
//! While it is true the handlers hand their command to the installed [`LinearRequests`], of which
//! this step installs none, so they do nothing yet; the link, unlink and open commands are handled
//! in `link` instead, `task: new from linear` in `import` (with its preview in `preview`), and
//! `task: refresh from linear` in `refresh`, which also refreshes on opening the Work view and on
//! focusing a task, and `task: create missing subtickets` in `subtickets`, which also offers the
//! missing subtickets after a proposal is applied under a linked group, and
//! `task: show linear suggestions` in `suggestions`. `linear: set token` and `linear: clear token`
//! open the masked token modal and clear the stored token instead (see [`token`]). No keys are
//! bound to these actions yet; they are reached from the command palette.

mod actions;
mod checklist;
mod chips;
pub(crate) mod client;
pub mod dump;
mod import;
mod link;
mod needs_you;
mod preview;
mod refresh;
mod settings;
mod subtickets;
mod suggestions;
pub mod sync;
pub mod token;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{
    Command, LinearRequests, LinearToast, action_types, init_palette, install_requests, linear,
    register, task,
};
pub use settings::LinearSettings;

/// Registers the `linear` settings and creates the sync, the refresh triggers, the row chips and
/// the subtickets watcher with its needs-you source. Call before [`init_palette`], and after
/// `bongos_work::init`, whose store and focus the triggers and the watcher attach to.
pub fn init(cx: &mut App) {
    LinearSettings::register(cx);
    sync::install(cx);
    refresh::install(cx);
    chips::install(cx);
    subtickets::install(cx);
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
mod chips_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod dump_tests;
#[cfg(test)]
mod import_tests;
#[cfg(test)]
mod link_tests;
#[cfg(test)]
mod refresh_tests;
#[cfg(test)]
mod subtickets_tests;
#[cfg(test)]
mod suggestions_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod token_tests;
