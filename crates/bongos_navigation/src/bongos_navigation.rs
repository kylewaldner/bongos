//! Navigation pickers: the buffer switcher, go to file, find in files and the lines picker, with
//! the `navigation` actions that start and drive them.

pub mod actions;
pub mod find;
pub mod goto_file;
pub mod goto_file_open;
pub mod lines;
pub mod preview;
pub mod registry;
pub mod settings;
pub mod switcher;
pub mod switcher_kill;
pub mod target;
#[cfg(test)]
pub mod test_support;

pub use settings::NavigationSettings;

use ::settings::Settings as _;
use gpui::App;

/// The action namespace.
pub const NAMESPACE: &str = "navigation";

/// Registers the settings, declares each action's prefix argument and registers the actions of
/// every workspace. The namespace is shown in the command palette.
pub fn init(cx: &mut App) {
    NavigationSettings::register(cx);
    actions::init(cx);
}
