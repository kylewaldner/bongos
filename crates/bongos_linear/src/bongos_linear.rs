//! The app side of the bongos Linear sync: the `linear` settings and the `task` and `linear`
//! actions of the sync, hidden from the command palette while `linear.enabled` is false. While it
//! is false each handler only says that the sync is off, and nothing here reaches the network.
//! While it is true the handlers hand their command to the installed [`LinearRequests`], of which
//! this step installs none, so they do nothing yet. The keys are bound in the Emacs keymaps, not
//! here.

mod actions;
mod settings;

use ::settings::Settings as _;
use gpui::App;

pub use actions::{
    Command, LinearRequests, LinearToast, action_types, init_palette, install_requests, linear,
    register, task,
};
pub use settings::LinearSettings;

/// Registers the `linear` settings. Call before [`init_palette`].
pub fn init(cx: &mut App) {
    LinearSettings::register(cx);
}

#[cfg(test)]
mod actions_tests;
