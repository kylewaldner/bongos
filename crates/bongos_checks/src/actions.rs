//! The `task` actions of task checks, hidden from the command palette while
//! `task_checks.enabled` is false, and their handlers. While it is false each handler only says
//! that task checks are off. While it is true, run checks, run checks clean, run check, cancel
//! checks and dump checks act on a task (the task at point, else the focused one, else a pick)
//! through the checks engine; they start no agent and call no model. Show checks and show check
//! sources open the details tab at the Checks section, show check output opens the output of a
//! check in a read-only buffer, and edit checks opens the task's checks in a working copy.
//!
//! Run checks, run checks clean, run check and cancel checks reach the engine through
//! [`checks_access`]: the engine of this process while it holds the state writer, else the
//! process that holds it, through the registered forwarder; then nothing starts here.

use std::any::TypeId;

use anyhow::Result;
use bongos_project_repo::{ProjectRepositorySettings, fresh_name};
use bongos_work::model::PersonAtKeyboard;
use bongos_work::model::commands::Context as WorkContext;
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
use crate::edit::{checks_table_row, edit_target};
use crate::engine::{ChecksEngine, Refusal, RunChecksRequest, plan_rows, task_records};
use crate::host::{ChecksAccess, ChecksCommand, ChecksReply, checks_access};
use crate::output_view::{latest_per_check, read_output, view_text};

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

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.clone()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<ChecksToast>(), message),
        cx,
    );
}

/// What a command that acts on a task does once it has the task's id.
type OnTask = fn(String, &mut Workspace, &mut Window, &mut Context<Workspace>);

/// What a command that offers every task says when there is none.
pub(crate) const NO_TASKS_MESSAGE: &str = "There are no tasks to act on";

fn has_tasks(cx: &App) -> bool {
    bongos_work::WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .is_some_and(|tree| !tree.tasks().is_empty())
}

/// Which tasks a command offers when it acts on none at point.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Offer {
    /// The tasks that have checks.
    WithChecks,
    /// Every task: the command can give a task its first checks.
    Any,
}

/// Registers the handler of `A`: the off toast while `task_checks.enabled` is false, else `act`
/// on the task the command is for.
fn register_task_action<A: gpui::Action>(
    workspace: &mut Workspace,
    placeholder: &'static str,
    act: OnTask,
) {
    register_offering::<A>(workspace, placeholder, Offer::WithChecks, act);
}

fn register_offering<A: gpui::Action>(
    workspace: &mut Workspace,
    placeholder: &'static str,
    offer: Offer,
    act: OnTask,
) {
    workspace.register_action(
        move |workspace, _: &A, window: &mut Window, cx: &mut Context<Workspace>| {
            if !TaskChecksSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE.to_string(), cx);
                return;
            }
            on_task(workspace, window, cx, placeholder, offer, act);
        },
    );
}

/// Calls `act` with the task at point, else asks which task among those `offer` names.
fn on_task(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    placeholder: &'static str,
    offer: Offer,
    act: OnTask,
) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let at_point = bongos_work::task_at_point(workspace, window, cx);
    if offer == Offer::Any {
        match at_point {
            Some(id) => act(id, workspace, window, cx),
            None if !has_tasks(cx) => show_toast(workspace, NO_TASKS_MESSAGE.to_string(), cx),
            None => bongos_work::with_task(
                workspace,
                window,
                cx,
                placeholder,
                NO_TASKS_MESSAGE,
                |_, _| true,
                Box::new(move |id, workspace, window, cx| act(id, workspace, window, cx)),
            ),
        }
        return;
    }
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

/// Refuses with a toast when the task has no checks. True when it may go on.
fn has_checks_or_toast(id: &str, workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    let Some(engine) = ChecksEngine::global(cx) else {
        return false;
    };
    let name = task_name(id, cx);
    let task = engine.read(cx).task(id, cx);
    let refusal = match task {
        Some(task) => plan_rows(&task, None).err(),
        None => Some(Refusal::NoChecks { task: name }),
    };
    match refusal {
        Some(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            false
        }
        None => true,
    }
}

/// `task: show checks`: the details of the task, at its Checks section.
fn show_checks(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let focus = bongos_work::SectionFocus {
        title: CHECKS_TITLE.to_string(),
    };
    bongos_work::show_details(&id, Some(focus), workspace, window, cx);
}

/// `task: show check sources`: the same, with the effective set's derivation under it.
fn show_check_sources(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let focus = bongos_work::SectionFocus {
        title: CHECKS_TITLE.to_string(),
    };
    bongos_work::show_details_with_sources(&id, Some(focus), workspace, window, cx);
}

/// The title of the Checks section.
const CHECKS_TITLE: &str = "Checks";

/// `task: show check output`: the output of a check, asked for when the task has several that
/// ran.
fn show_check_output(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !has_checks_or_toast(&id, workspace, cx) {
        return;
    }
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let records = engine.read(cx).records(&id, cx);
    let checks: Vec<String> = latest_per_check(&records)
        .into_iter()
        .map(|record| record.check.clone())
        .collect();
    match checks.len() {
        0 => {
            let name = task_name(&id, cx);
            show_toast(workspace, format!("No check of {name} has run yet"), cx);
        }
        1 => open_output_of(&id, &checks[0], workspace, window, cx),
        _ => {
            let chosen = checks.clone();
            bongos_work::pick(
                workspace,
                "Check output",
                checks,
                0,
                Box::new(move |index, workspace, window, cx| {
                    open_output_of(&id, &chosen[index], workspace, window, cx)
                }),
                window,
                cx,
            );
        }
    }
}

/// Opens the output of the latest record of `check` of the task in a read-only buffer, which
/// follows the files while the check runs.
pub(crate) fn open_output_of(
    task_id: &str,
    check: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let Some(task) = engine.read(cx).task(task_id, cx) else {
        return;
    };
    let records = task_records(&task);
    let latest = latest_per_check(&records)
        .into_iter()
        .find(|record| record.check == check)
        .cloned();
    let Some(record) = latest else {
        let message = format!("{check} of {} has not run yet", task.name);
        show_toast(workspace, message, cx);
        return;
    };
    let text = view_text(&record, read_output(&record).as_deref());
    let title = format!("Output: {check} of {}", task.name);
    let (task_id, record_id) = (task_id.to_string(), record.id);
    let follow = move |cx: &App| {
        let records = task_records(&task);
        let Some(record) = records.iter().find(|record| record.id == record_id) else {
            return (String::new(), true);
        };
        let running =
            ChecksEngine::global(cx).is_some_and(|engine| engine.read(cx).is_running(&task_id));
        let text = view_text(record, read_output(record).as_deref());
        (text, record.is_finished() && !running)
    };
    bongos_work::open_following_text(workspace, title, text, follow, window, cx);
}

/// `task: edit checks`: the file of the task's checks in a new working copy, with the cursor at
/// its `checks` table.
fn edit_checks(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(engine) = ChecksEngine::global(cx) else {
        return;
    };
    let Some(task) = engine.read(cx).task(&id, cx) else {
        let name = task_name(&id, cx);
        show_toast(
            workspace,
            format!("{name} is not in a project with definitions"),
            cx,
        );
        return;
    };
    let folder = bongos_work::WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .and_then(|tree| tree.task_by_id(&id)?.definition.clone());
    let relative = edit_target(folder.as_deref(), None);
    let prefix = ProjectRepositorySettings::get_global(cx)
        .branch_prefix
        .clone();
    let author = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx)).author;
    let created =
        bongos_work::working_copy_to_edit(task.project, fresh_name(), None, author, prefix, cx);
    cx.spawn_in(window, async move |workspace, cx| match created.await {
        Ok(definitions) => {
            let path = definitions.join(&relative);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let row = checks_table_row(&text);
            workspace
                .update_in(cx, |workspace, window, cx| {
                    bongos_work::open_file_at_line(path, row, workspace, window, cx)
                })
                .ok();
        }
        Err(error) => {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!("Could not edit the checks: {error:#}"),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
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
    // Show checks and show check sources open the details tab, whose header says "no checks".
    register_offering::<task::ShowChecks>(workspace, "Show checks of", Offer::Any, show_checks);
    register_offering::<task::ShowCheckSources>(
        workspace,
        "Show check sources of",
        Offer::Any,
        show_check_sources,
    );
    register_task_action::<task::ShowCheckOutput>(
        workspace,
        "Show check output of",
        show_check_output,
    );
    register_offering::<task::EditChecks>(workspace, "Edit checks of", Offer::Any, edit_checks);
    register_task_action::<task::CancelChecks>(workspace, "Stop checks of", cancel_checks);
    register_task_action::<task::DumpChecks>(workspace, "Dump checks of", dump_checks);
}
