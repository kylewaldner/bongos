//! The `task` actions of task checks, hidden from the command palette while
//! `task_checks.enabled` is false, and their handlers. While it is false each handler only says
//! that task checks are off. While it is true each says that running checks is not available yet;
//! the commands that act on a task's checks arrive with the engine that runs them.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::TaskChecksSettings;

/// New actions in the `task` namespace, shared with the work tree's, the builder's, Linear's and
/// Zed's task runner; no name collides.
pub mod task {
    use gpui::actions;

    actions!(
        task,
        [
            /// Runs the checks the focused task still owes.
            RunChecks,
            /// Runs the checks of the focused task again from a clean start, reusing nothing.
            RunChecksClean,
            /// Runs one check of the focused task.
            RunCheck,
            /// Shows the checks of the focused task and where each stands.
            ShowChecks,
            /// Shows where each check of the focused task comes from.
            ShowCheckSources,
            /// Shows the output of the last run of a check of the focused task.
            ShowCheckOutput,
            /// Opens the checks of the focused task for editing.
            EditChecks,
            /// Stops the checks running for the focused task.
            CancelChecks,
            /// Writes the checks of the focused task to a file in the logs directory.
            DumpChecks,
        ]
    );
}

/// Every action of task checks, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<task::RunChecks>(),
        TypeId::of::<task::RunChecksClean>(),
        TypeId::of::<task::RunCheck>(),
        TypeId::of::<task::ShowChecks>(),
        TypeId::of::<task::ShowCheckSources>(),
        TypeId::of::<task::ShowCheckOutput>(),
        TypeId::of::<task::EditChecks>(),
        TypeId::of::<task::CancelChecks>(),
        TypeId::of::<task::DumpChecks>(),
    ]
}

/// Hides the actions of task checks from the command palette while `task_checks.enabled` is
/// false and shows them while it is true. Call after `command_palette_hooks::init` and
/// [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = TaskChecksSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the task check commands.
pub struct ChecksToast;

/// The message of the last toast of the task check commands, for tests.
#[cfg(test)]
pub(crate) struct LastToast(pub String);

#[cfg(test)]
impl Global for LastToast {}

/// The toast a command shows while `task_checks.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "Task checks are off (task_checks.enabled)";

/// The toast a command shows while `task_checks.enabled` is true and nothing runs checks yet.
pub(crate) const NOT_AVAILABLE_MESSAGE: &str = "Running task checks is not available yet";

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.clone()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<ChecksToast>(), message),
        cx,
    );
}

/// Registers the handler of `A`: the off toast while `task_checks.enabled` is false, the
/// not-available toast while it is true.
fn register_action<A: gpui::Action>(workspace: &mut Workspace) {
    workspace.register_action(
        move |workspace, _: &A, _window: &mut Window, cx: &mut Context<Workspace>| {
            let message = if TaskChecksSettings::get_global(cx).enabled {
                NOT_AVAILABLE_MESSAGE
            } else {
                OFF_MESSAGE
            };
            show_toast(workspace, message.to_string(), cx);
        },
    );
}

/// Registers the handlers of the actions of task checks on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_action::<task::RunChecks>(workspace);
    register_action::<task::RunChecksClean>(workspace);
    register_action::<task::RunCheck>(workspace);
    register_action::<task::ShowChecks>(workspace);
    register_action::<task::ShowCheckSources>(workspace);
    register_action::<task::ShowCheckOutput>(workspace);
    register_action::<task::EditChecks>(workspace);
    register_action::<task::CancelChecks>(workspace);
    register_action::<task::DumpChecks>(workspace);
}
