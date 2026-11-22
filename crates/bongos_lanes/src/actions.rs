//! The `lanes` actions, hidden from the command palette while `lanes.enabled` is false. While it
//! is false each one only says that lanes are off. Lanes run in the engine host; while they are
//! on, these commands say that this window does not reach it yet.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::LanesSettings;

pub mod lanes {
    use gpui::actions;

    actions!(
        lanes,
        [
            /// Starts the lanes of the project.
            Start,
            /// Stops the lanes of the project at their next job boundary.
            Stop,
            /// Shows each lane, what it owns and what it waits on.
            Show,
        ]
    );
}

/// Every action of lanes, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<lanes::Start>(),
        TypeId::of::<lanes::Stop>(),
        TypeId::of::<lanes::Show>(),
    ]
}

/// Hides the actions of lanes from the command palette while `lanes.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = LanesSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the lane commands.
pub struct LanesToast;

/// The message of the last toast of the lane commands, for tests.
#[cfg(test)]
pub(crate) struct LastToast(pub String);

#[cfg(test)]
impl Global for LastToast {}

/// The toast a command shows while `lanes.enabled` is false.
pub const OFF_MESSAGE: &str = "Lanes are off (lanes.enabled)";

/// The toast a command shows while lanes are on: the engine host runs them.
pub const ENGINE_HOST_MESSAGE: &str = "Lanes run in the engine host, which this window does not reach yet";

fn show_toast(workspace: &mut Workspace, message: &str, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.to_string()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LanesToast>(), message.to_string()),
        cx,
    );
}

fn register_action<A: gpui::Action>(workspace: &mut Workspace) {
    workspace.register_action(
        |workspace, _: &A, _: &mut Window, cx: &mut Context<Workspace>| {
            let message = if LanesSettings::get_global(cx).enabled {
                ENGINE_HOST_MESSAGE
            } else {
                OFF_MESSAGE
            };
            show_toast(workspace, message, cx);
        },
    );
}

/// Registers the handlers of the actions of lanes on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_action::<lanes::Start>(workspace);
    register_action::<lanes::Stop>(workspace);
    register_action::<lanes::Show>(workspace);
}
