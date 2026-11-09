//! The `task` and `linear` actions of the Linear sync, hidden from the command palette while
//! `linear.enabled` is false, and their handlers. While it is false each handler only says that
//! the sync is off. While it is true each handler passes its [`Command`] to the installed
//! [`LinearRequests`]; none is installed yet, so nothing happens. The keys are bound in the Emacs
//! keymaps, not here.

use std::any::TypeId;
use std::sync::Arc;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action, App, Context, Global, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::LinearSettings;

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
        ]
    );
}

/// What a handler asks the Linear sync to do while `linear.enabled` is true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewFromLinear,
    LinkToLinear,
    UnlinkFromLinear,
    OpenInLinear,
    CreateMissingSubtickets,
    RefreshFromLinear,
    ShowLinearSuggestions,
    CopyPrTitle,
    SetToken,
    ClearToken,
}

/// The seam between the actions and the sync: the one place an enabled handler goes through.
/// Nothing installs it yet, so an enabled handler does nothing; a test installs a counter to prove
/// that a disabled handler never gets here.
pub trait LinearRequests: Send + Sync {
    fn run(&self, command: Command, cx: &mut App);
}

struct InstalledRequests(Arc<dyn LinearRequests>);

impl Global for InstalledRequests {}

/// Installs the sink of the enabled handlers, replacing any earlier one.
pub fn install_requests(requests: Arc<dyn LinearRequests>, cx: &mut App) {
    cx.set_global(InstalledRequests(requests));
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

fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.clone()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LinearToast>(), message),
        cx,
    );
}

/// Registers the handler of `A`: the off toast while `linear.enabled` is false, `command` handed
/// to the installed [`LinearRequests`] while it is true.
fn register_with<A: Action>(workspace: &mut Workspace, command: Command) {
    workspace.register_action(move |workspace, _: &A, _: &mut Window, cx| {
        if !LinearSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        if let Some(requests) = cx.try_global::<InstalledRequests>().map(|r| r.0.clone()) {
            requests.run(command, cx);
        }
    });
}

/// Registers the handlers of the sync's actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_with::<task::NewFromLinear>(workspace, Command::NewFromLinear);
    register_with::<task::LinkToLinear>(workspace, Command::LinkToLinear);
    register_with::<task::UnlinkFromLinear>(workspace, Command::UnlinkFromLinear);
    register_with::<task::OpenInLinear>(workspace, Command::OpenInLinear);
    register_with::<task::CreateMissingSubtickets>(workspace, Command::CreateMissingSubtickets);
    register_with::<task::RefreshFromLinear>(workspace, Command::RefreshFromLinear);
    register_with::<task::ShowLinearSuggestions>(workspace, Command::ShowLinearSuggestions);
    register_with::<task::CopyPrTitle>(workspace, Command::CopyPrTitle);
    register_with::<linear::SetToken>(workspace, Command::SetToken);
    register_with::<linear::ClearToken>(workspace, Command::ClearToken);
}
