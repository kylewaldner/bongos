//! The `task` and `linear` actions of the Linear sync, hidden from the command palette while
//! `linear.enabled` is false, and their handlers. While it is false each handler only says that
//! the sync is off; while it is true each runs its own module's handler. The token commands open
//! the token modal and clear the stored token (see `token`); `linear: dump state` writes the state
//! of the sync (see `dump`). The three link commands (`task: link to linear`, `unlink from
//! linear`, `open in linear`) act on the task at point and are handled in `link`, `task: create
//! missing subtickets` in `subtickets`, `task: show linear suggestions` in `suggestions` and
//! `task: copy pr title` in `conventions`. The actions are bound under `C-c w L` (see
//! the emacs keymap) and reachable from the command palette.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::LinearSettings;
use crate::conventions;
use crate::dump;
use crate::import;
use crate::link;
use crate::refresh;
use crate::subtickets_actions;
use crate::suggestions;
use crate::token;

/// New actions in the `task` namespace, shared with the work tree's, the builder's and Zed's task
/// runner; no name collides.
pub mod task {
    use gpui::actions;

    actions!(
        task,
        [
            /// Creates a task from a Linear ticket.
            NewFromLinear,
            /// Links the focused task to a Linear ticket.
            LinkToLinear,
            /// Removes the Linear link of the focused task.
            UnlinkFromLinear,
            /// Opens the Linear ticket of the focused task in the browser.
            OpenInLinear,
            /// Creates the Linear subtickets the focused task tree lacks.
            CreateMissingSubtickets,
            /// Reads the ticket of the focused task from Linear again.
            RefreshFromLinear,
            /// Shows what the Linear sync suggests for the focused task.
            ShowLinearSuggestions,
            /// Copies the pull request title of the focused task.
            CopyPrTitle,
        ]
    );
}

/// Actions in the `linear` namespace.
pub mod linear {
    use gpui::actions;

    actions!(
        linear,
        [
            /// Asks for the Linear token and stores it.
            SetToken,
            /// Removes the stored Linear token.
            ClearToken,
            /// Writes the state of the Linear sync to a file in the logs directory.
            DumpState,
        ]
    );
}

/// Every action of the sync, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<task::NewFromLinear>(),
        TypeId::of::<task::LinkToLinear>(),
        TypeId::of::<task::UnlinkFromLinear>(),
        TypeId::of::<task::OpenInLinear>(),
        TypeId::of::<task::CreateMissingSubtickets>(),
        TypeId::of::<task::RefreshFromLinear>(),
        TypeId::of::<task::ShowLinearSuggestions>(),
        TypeId::of::<task::CopyPrTitle>(),
        TypeId::of::<linear::SetToken>(),
        TypeId::of::<linear::ClearToken>(),
        TypeId::of::<linear::DumpState>(),
    ]
}

/// Hides the sync's actions from the command palette while `linear.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = LinearSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the sync's commands.
pub struct LinearToast;

/// The message of the last toast of the sync, for tests.
#[cfg(test)]
pub(crate) struct LastToast(pub String);

#[cfg(test)]
impl Global for LastToast {}

/// The toast a command shows while `linear.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "Linear sync is off (linear.enabled)";

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.clone()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LinearToast>(), message),
        cx,
    );
}

/// Registers the handler of `A` that runs `handler` while `linear.enabled` is true and shows the
/// off toast while it is false.
fn register_direct<A: Action>(
    workspace: &mut Workspace,
    handler: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    workspace.register_action(move |workspace, _: &A, window: &mut Window, cx| {
        if !LinearSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        handler(workspace, window, cx);
    });
}

/// Registers the handlers of the sync's actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_direct::<task::NewFromLinear>(workspace, import::new_from_linear);
    register_direct::<task::LinkToLinear>(workspace, link::link_to_linear);
    register_direct::<task::UnlinkFromLinear>(workspace, link::unlink_from_linear);
    register_direct::<task::OpenInLinear>(workspace, link::open_in_linear);
    register_direct::<task::CreateMissingSubtickets>(
        workspace,
        subtickets_actions::create_missing_subtickets,
    );
    register_direct::<task::RefreshFromLinear>(workspace, refresh::refresh_from_linear);
    register_direct::<task::ShowLinearSuggestions>(workspace, suggestions::show_linear_suggestions);
    register_direct::<task::CopyPrTitle>(workspace, conventions::copy_pr_title);
    register_direct::<linear::SetToken>(workspace, token::set_token);
    register_direct::<linear::ClearToken>(workspace, token::clear_token);
    register_direct::<linear::DumpState>(workspace, dump::dump_state);
}
