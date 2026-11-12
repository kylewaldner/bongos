//! The `workflow` actions, hidden from the command palette while `workflows.enabled` is false, and
//! their handlers. While it is false each handler only says that workflows are off.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::WorkflowsSettings;

/// Actions in the `workflow` namespace.
pub mod workflow {
    use gpui::actions;

    actions!(
        workflow,
        [
            /// Starts a workflow run on the task at point.
            Run,
            /// Pauses the run after its running nodes finish.
            Pause,
            /// Stops the run now, keeping its record.
            Halt,
            /// Resumes a paused or halted run.
            Resume,
            /// Retries the failed step of the run.
            RetryStep,
            /// Retries the failed step with the definition read again.
            RetryStepFresh,
            /// Starts the run again from its first step.
            Restart,
            /// Ends the run as cancelled.
            Cancel,
            /// Approves the open gate of the run.
            Approve,
            /// Sends the open gate back with notes.
            Revise,
            /// Opens the logs of the run.
            OpenLogs,
            /// Shows the runs of every task.
            ShowRuns,
            /// Shows the run of the task at point.
            ShowRun,
            /// Opens the worktree of the run.
            OpenRunWorktree,
            /// Shows the diff of the run.
            ShowRunDiff,
            /// Removes the records of finished runs.
            ClearFinished,
            /// Checks a workflow definition.
            Validate,
            /// Creates a workflow definition from a template.
            New,
            /// Opens the definition of a workflow.
            EditDefinition,
            /// Copies the command that resumes the session of a node.
            CopyResumeCommand,
            /// Toggles the graph in the run view.
            ToggleGraph,
            /// Writes the text form of the run view to the logs directory.
            DumpRunView,
            /// Writes the state of the engine to the logs directory.
            DumpEngine,
        ]
    );
}

/// Every action of workflows, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<workflow::Run>(),
        TypeId::of::<workflow::Pause>(),
        TypeId::of::<workflow::Halt>(),
        TypeId::of::<workflow::Resume>(),
        TypeId::of::<workflow::RetryStep>(),
        TypeId::of::<workflow::RetryStepFresh>(),
        TypeId::of::<workflow::Restart>(),
        TypeId::of::<workflow::Cancel>(),
        TypeId::of::<workflow::Approve>(),
        TypeId::of::<workflow::Revise>(),
        TypeId::of::<workflow::OpenLogs>(),
        TypeId::of::<workflow::ShowRuns>(),
        TypeId::of::<workflow::ShowRun>(),
        TypeId::of::<workflow::OpenRunWorktree>(),
        TypeId::of::<workflow::ShowRunDiff>(),
        TypeId::of::<workflow::ClearFinished>(),
        TypeId::of::<workflow::Validate>(),
        TypeId::of::<workflow::New>(),
        TypeId::of::<workflow::EditDefinition>(),
        TypeId::of::<workflow::CopyResumeCommand>(),
        TypeId::of::<workflow::ToggleGraph>(),
        TypeId::of::<workflow::DumpRunView>(),
        TypeId::of::<workflow::DumpEngine>(),
    ]
}

/// Hides the workflow actions from the command palette while `workflows.enabled` is false and
/// shows them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = WorkflowsSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the workflow commands.
pub struct WorkflowToast;

/// The message of the last toast of workflows, for tests.
#[cfg(test)]
pub(crate) struct LastToast(pub String);

#[cfg(test)]
impl Global for LastToast {}

/// The toast a command shows while `workflows.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "Workflows are off (workflows.enabled)";

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.clone()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<WorkflowToast>(), message),
        cx,
    );
}

/// Registers the handler of `A`: it shows the off toast while `workflows.enabled` is false. The
/// engine's handlers replace the body when it is true in later steps (`validate`, `new`, `edit
/// definition` and the run commands have theirs in `commands`).
fn register_action<A: Action>(workspace: &mut Workspace) {
    workspace.register_action(move |workspace, _: &A, _: &mut Window, cx| {
        if !WorkflowsSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        }
    });
}

/// Registers the handlers of the workflow actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_action::<workflow::ShowRuns>(workspace);
    register_action::<workflow::ClearFinished>(workspace);
    register_action::<workflow::DumpEngine>(workspace);
    crate::commands::register(workspace);
    crate::run_view::register(workspace);
}
