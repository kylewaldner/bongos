//! The `task` actions of task checks, hidden from the command palette while
//! `task_checks.enabled` is false, and their handlers. While it is false each handler only says
//! that task checks are off. While it is true, run checks, run checks clean, run check, cancel
//! checks and dump checks act on a task (the task at point, else the focused one, else a pick)
//! through the checks engine; they start no agent and call no model. The others still say that
//! they are not available yet.
//!
//! Run checks, run checks clean, run check and cancel checks reach the engine through
//! [`checks_access`]: the engine of this process while it holds the state writer, else the
//! process that holds it, through the registered forwarder; then nothing starts here.

use std::any::TypeId;

use anyhow::Result;
use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{App, Context, Entity, Task, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::TaskChecksSettings;
use crate::command_logic::{NO_TASK_MESSAGE, Target, choose_target, report_message, write_dump};
use crate::engine::{ChecksEngine, RunChecksRequest, plan_rows};
use crate::host::{ChecksAccess, ChecksCommand, ChecksReply, checks_access};

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

/// The toast a command shows while `task_checks.enabled` is true and it is not built yet.
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

/// What a command that acts on a task does once it has the task's id.
type OnTask = fn(String, &mut Workspace, &mut Window, &mut Context<Workspace>);

/// Registers the handler of `A`: the off toast while `task_checks.enabled` is false, else `act`
/// on the task the command is for.
fn register_task_action<A: gpui::Action>(
    workspace: &mut Workspace,
    placeholder: &'static str,
    act: OnTask,
) {
    workspace.register_action(
        move |workspace, _: &A, window: &mut Window, cx: &mut Context<Workspace>| {
            if !TaskChecksSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE.to_string(), cx);
                return;
            }
            on_task(workspace, window, cx, placeholder, act);
        },
    );
}

/// Calls `act` with the task at point, else asks which task among those with checks.
fn on_task(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    placeholder: &'static str,
    act: OnTask,
) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let at_point = bongos_work::task_at_point(workspace, window, cx);
    let with_checks = match at_point {
        Some(_) => Vec::new(),
        None => tasks_with_checks(&engine, cx),
    };
    match choose_target(at_point, with_checks) {
        Target::Task(id) => act(id, workspace, window, cx),
        Target::NoTasks => show_toast(workspace, NO_TASK_MESSAGE.to_string(), cx),
        Target::Pick(ids) => bongos_work::with_task(
            workspace,
            window,
            cx,
            placeholder,
            NO_TASK_MESSAGE,
            move |task, _| {
                task.id()
                    .is_some_and(|id| ids.iter().any(|kept| kept == id))
            },
            Box::new(move |id, workspace, window, cx| act(id, workspace, window, cx)),
        ),
    }
}

/// The ids of the tasks of the Work tree that have checks to run.
fn tasks_with_checks(engine: &Entity<ChecksEngine>, cx: &App) -> Vec<String> {
    let Some(tree) = bongos_work::WorkStore::global(cx).and_then(|store| store.read(cx).snapshot())
    else {
        return Vec::new();
    };
    let engine = engine.read(cx);
    tree.tasks()
        .into_iter()
        .filter_map(|task| task.id())
        .filter(|id| engine.has_checks(id, cx))
        .map(str::to_string)
        .collect()
}

/// How messages name the task `task_id`.
fn task_name(task_id: &str, cx: &App) -> String {
    ChecksEngine::global(cx)
        .and_then(|engine| engine.read(cx).task(task_id, cx))
        .map_or_else(|| task_id.to_string(), |task| task.name)
}

/// What a command says when the process that holds the writer answered.
pub(crate) fn reply_message(name: &str, reply: &Result<ChecksReply>) -> String {
    match reply {
        Ok(ChecksReply::Started) => format!("Running the checks of {name}"),
        Ok(ChecksReply::Refused { reason }) => reason.clone(),
        Ok(ChecksReply::Cancelled { stopped: true }) => format!("Stopping the checks of {name}"),
        Ok(ChecksReply::Cancelled { stopped: false }) => format!("{name} has no checks running"),
        Ok(ChecksReply::Hold { .. }) => format!("Unexpected answer about the checks of {name}"),
        Err(error) => format!("Could not reach the checks of {name}: {error:#}"),
    }
}

/// Toasts the answer of the process that holds the writer once it comes.
fn toast_reply(name: String, reply: Task<Result<ChecksReply>>, cx: &mut Context<Workspace>) {
    cx.spawn(async move |workspace, cx| {
        let reply = reply.await;
        workspace
            .update(cx, |workspace, cx| {
                show_toast(workspace, reply_message(&name, &reply), cx)
            })
            .ok();
    })
    .detach();
}

/// Starts a run of `request` and toasts how it ended, or why it was refused. Without the state
/// writer the request goes to the process that holds it and only its answer is toasted.
fn start_run(request: RunChecksRequest, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(access) = checks_access(cx) else {
        return;
    };
    let name = task_name(&request.task_id, cx);
    let engine = match access {
        ChecksAccess::Local(engine) => engine,
        forwarded @ ChecksAccess::Forwarded(_) => {
            let command = ChecksCommand::RunChecks {
                task_id: request.task_id,
                checks: request.checks,
                clean: request.clean,
            };
            let reply = forwarded.send(&command, cx);
            toast_reply(name, reply, cx);
            return;
        }
    };
    let report = match engine.update(cx, |engine, cx| engine.start(request, cx)) {
        Ok(report) => report,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    show_toast(workspace, format!("Running the checks of {name}"), cx);
    cx.spawn(async move |workspace, cx| {
        let report = report.await;
        workspace
            .update(cx, |workspace, cx| {
                show_toast(workspace, report_message(&name, &report), cx)
            })
            .ok();
    })
    .detach();
}

pub(crate) fn run_checks(
    id: String,
    workspace: &mut Workspace,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    start_run(RunChecksRequest::person(id), workspace, cx);
}

fn run_checks_clean(
    id: String,
    workspace: &mut Workspace,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let request = RunChecksRequest {
        clean: true,
        ..RunChecksRequest::person(id)
    };
    start_run(request, workspace, cx);
}

/// Asks which check of the task, then runs only that one.
fn run_check(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    // Without the writer only the task's checks are known here; the process that holds the
    // writer checks the rest when the run arrives.
    let rows = if matches!(checks_access(cx), Some(ChecksAccess::Forwarded(_))) {
        match engine.read(cx).task(&id, cx) {
            Some(task) => plan_rows(&task, None),
            None => Err(crate::engine::Refusal::NoChecks { task: id.clone() }),
        }
    } else {
        engine
            .read(cx)
            .preflight(&id, None, cx)
            .map(|preflight| preflight.rows)
    };
    let names = match rows {
        Ok(rows) => rows
            .iter()
            .map(|row| row.decl.name.clone())
            .collect::<Vec<_>>(),
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    let chosen = names.clone();
    bongos_work::pick(
        workspace,
        "Run check",
        names,
        0,
        Box::new(move |index, workspace, _, cx| {
            let request = RunChecksRequest {
                checks: Some(vec![chosen[index].clone()]),
                ..RunChecksRequest::person(id)
            };
            start_run(request, workspace, cx);
        }),
        window,
        cx,
    );
}

pub(crate) fn cancel_checks(
    id: String,
    workspace: &mut Workspace,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(access) = checks_access(cx) else {
        return;
    };
    let name = task_name(&id, cx);
    let engine = match access {
        ChecksAccess::Local(engine) => engine,
        forwarded @ ChecksAccess::Forwarded(_) => {
            let reply = forwarded.send(&ChecksCommand::CancelChecks { task_id: id }, cx);
            toast_reply(name, reply, cx);
            return;
        }
    };
    let message = if engine.update(cx, |engine, _| engine.cancel(&id)) {
        format!("Stopping the checks of {name}")
    } else {
        format!("{name} has no checks running")
    };
    show_toast(workspace, message, cx);
}

fn dump_checks(id: String, workspace: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let message = match engine.read(cx).dump(&id, cx) {
        None => format!("{id} has no checks"),
        Some(text) => match write_dump(paths::logs_dir(), &text) {
            Ok(path) => format!("Wrote {}", path.display()),
            Err(error) => format!("Could not write the checks: {error}"),
        },
    };
    show_toast(workspace, message, cx);
}

/// Registers the handlers of the actions of task checks on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_task_action::<task::RunChecks>(workspace, "Run checks of", run_checks);
    register_task_action::<task::RunChecksClean>(
        workspace,
        "Run checks clean of",
        run_checks_clean,
    );
    register_task_action::<task::RunCheck>(workspace, "Run a check of", run_check);
    register_action::<task::ShowChecks>(workspace);
    register_action::<task::ShowCheckSources>(workspace);
    register_action::<task::ShowCheckOutput>(workspace);
    register_action::<task::EditChecks>(workspace);
    register_task_action::<task::CancelChecks>(workspace, "Stop checks of", cancel_checks);
    register_task_action::<task::DumpChecks>(workspace, "Dump checks of", dump_checks);
}
