//! The `task` and `proposal` actions of the task builder, hidden from the command palette while
//! `task_builder.enabled` is false, and their handlers. While it is false each handler only says
//! that the task builder is off. While it is true `task: plan` and `task: new without plan` work
//! (see [`crate::plan`]) and the others do nothing yet: their commands arrive in later steps. The
//! keys are bound in the Emacs keymaps, not here.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::{TaskBuilderSettings, plan};

/// New actions in the `task` namespace, shared with the work tree's and Zed's task runner; no
/// name collides.
pub mod task {
    use gpui::actions;

    actions!(
        task,
        [
            /// Starts or shows the plan chat of a task.
            Plan,
            /// Writes the next proposal file of a task and asks the chat to edit it.
            Propose,
            /// Shows the preview of the proposal of a task.
            ShowProposal,
            /// Opens the proposal file of a task.
            EditProposal,
            /// Applies the proposal of a task.
            ApplyProposal,
            /// Rejects the proposal of a task.
            RejectProposal,
            /// Creates a task without the plan todo.
            NewWithoutPlan,
            /// Turns the automatic apply of proposals on or off for a project.
            ToggleAutoApply,
        ]
    );
}

/// Actions in the `proposal` namespace: the keys of the proposal view and the debug dumps.
pub mod proposal {
    use gpui::actions;

    actions!(
        proposal,
        [
            /// Opens the task under the cursor in the proposal view.
            Descend,
            /// Goes up to the parent in the proposal view.
            Up,
        ]
    );

    #[cfg(debug_assertions)]
    actions!(
        proposal,
        [
            /// Debug builds only: writes the text of the proposal view to `proposal-view.txt` in
            /// the logs directory.
            DumpView,
            /// Debug builds only: writes the text of the proposal preview to
            /// `proposal-preview.txt` in the logs directory.
            DumpPreview,
        ]
    );
}

/// Every action of the builder, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    #[allow(unused_mut)]
    let mut types = vec![
        TypeId::of::<task::Plan>(),
        TypeId::of::<task::Propose>(),
        TypeId::of::<task::ShowProposal>(),
        TypeId::of::<task::EditProposal>(),
        TypeId::of::<task::ApplyProposal>(),
        TypeId::of::<task::RejectProposal>(),
        TypeId::of::<task::NewWithoutPlan>(),
        TypeId::of::<task::ToggleAutoApply>(),
        TypeId::of::<proposal::Descend>(),
        TypeId::of::<proposal::Up>(),
    ];
    #[cfg(debug_assertions)]
    {
        types.push(TypeId::of::<proposal::DumpView>());
        types.push(TypeId::of::<proposal::DumpPreview>());
    }
    types
}

/// Hides the builder's actions from the command palette while `task_builder.enabled` is false and
/// shows them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = TaskBuilderSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the builder's commands.
pub struct BuilderToast;

/// The toast a command shows while `task_builder.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "The task builder is off (task_builder.enabled)";

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<BuilderToast>(), message),
        cx,
    );
}

/// Registers the handler of `A`: the off toast while `task_builder.enabled` is false, nothing yet
/// while it is true.
fn register_action<A: Action>(workspace: &mut Workspace) {
    register_with::<A>(workspace, |_, _, _| {});
}

/// Registers the handler of `A`: the off toast while `task_builder.enabled` is false, `run` while
/// it is true.
fn register_with<A: Action>(
    workspace: &mut Workspace,
    run: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    workspace.register_action(move |workspace, _: &A, window: &mut Window, cx| {
        if TaskBuilderSettings::get_global(cx).enabled {
            run(workspace, window, cx);
        } else {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        }
    });
}

/// Registers the handlers of the builder's actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_with::<task::Plan>(workspace, plan::plan);
    register_action::<task::Propose>(workspace);
    register_action::<task::ShowProposal>(workspace);
    register_action::<task::EditProposal>(workspace);
    register_action::<task::ApplyProposal>(workspace);
    register_action::<task::RejectProposal>(workspace);
    register_with::<task::NewWithoutPlan>(workspace, plan::new_without_plan);
    register_action::<task::ToggleAutoApply>(workspace);
    register_action::<proposal::Descend>(workspace);
    register_action::<proposal::Up>(workspace);
    #[cfg(debug_assertions)]
    {
        register_action::<proposal::DumpView>(workspace);
        register_action::<proposal::DumpPreview>(workspace);
    }
}
