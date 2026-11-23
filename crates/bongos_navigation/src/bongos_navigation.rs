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
pub mod test_support;

pub use settings::NavigationSettings;

use ::settings::Settings as _;
use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

/// The action namespace.
pub const NAMESPACE: &str = "navigation";

/// Registers the settings, declares each action's prefix argument, hides the namespace and
/// registers the actions of every workspace.
pub fn init(cx: &mut App) {
    NavigationSettings::register(cx);
    CommandPaletteFilter::update_global(cx, |filter, _| filter.hide_namespace(NAMESPACE));
    actions::init(cx);
}
