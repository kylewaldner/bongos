//! The handlers of `workflow: validate`, `workflow: new` and `workflow: edit definition`, and of
//! the run commands (`run`, `pause`, `halt`, `resume`, `retry step`, `retry step fresh`,
//! `restart`, `cancel`, `approve`, `revise`, `copy resume command`). While `workflows.enabled` is
//! false each only shows the off toast; no engine exists and nothing is read. File work runs on a
//! background task and the result comes back to the workspace. The run commands act on the active
//! run of the task at point and are thin: the decisions are the engine's.

use bongos_workflow_model::definition::{FieldType, ItemType};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{EndState, Lifecycle as RunLifecycle};
use editor::{Editor, MultiBuffer};
use gpui::{Action, App, AppContext as _, ClipboardItem, Context, WeakEntity, Window};
use language::Capability;
use serde_json::{Number, Value};
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, OpenOptions, OpenVisible, Workspace};

use crate::WorkflowsSettings;
use crate::actions::{OFF_MESSAGE, show_toast, workflow};
use crate::definitions::DefinitionError;
use crate::engine::{EditTarget, WorkflowEngine};
use crate::lifecycle::{ClearedRun, GateField, Refusal, RunId, RunStatus};
use crate::remote::{InstalledWorkflowRemote, WorkflowPayload, WorkflowRemote, WorkflowVerb};
use crate::shipped::TEMPLATES;
use crate::worktrees::{
    ClearPlan, ClearReport, FinishedRun, Git, GitError, apply_clear, plan_clear,
};
use settings::Settings as _;

/// The first choice of `workflow: new`: the minimal template.
pub const MINIMAL_CHOICE: &str = "Minimal";
/// The title of the diagnostics buffer of `workflow: validate`.
pub const VALIDATE_TITLE: &str = "Workflow diagnostics";

pub(crate) fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &workflow::Validate, window, cx| {
            if !WorkflowsSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE.to_string(), cx);
                return;
            }
            let Some(engine) = WorkflowEngine::ensure(cx) else {
                return;
            };
            // The project layer of the task at point is read from its verified commit.
            let task = task_at_point(workspace, window, cx);
            let task_dirs = match engine.dirs_for(task.as_ref(), cx) {
                Ok(task_dirs) => task_dirs,
                Err(refusal) => {
                    show_toast(workspace, refusal.to_string(), cx);
                    return;
                }
            };
            let context = WorkflowsSettings::get_global(cx).validation_context();
            run(
                workspace,
                window,
                cx,
                move |engine| engine.validate_in(&task_dirs.dirs, None, &context),
                |report, workspace, window, cx| {
                    open_report(
                        VALIDATE_TITLE.to_string(),
                        report.render(),
                        workspace,
                        window,
                        cx,
                    )
                },
            )
        })
        .register_action(|workspace, _: &workflow::New, window, cx| {
            if !WorkflowsSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE.to_string(), cx);
                return;
            }
            // The minimal template first, then a copy of each shipped definition to edit.
            let mut choices = vec![format!("{MINIMAL_CHOICE}: one agent node")];
            choices.extend(
                TEMPLATES
                    .iter()
                    .map(|template| format!("{}: {}", template.name, template.description)),
            );
            bongos_work::pick(
                workspace,
                "New workflow from",
                choices,
                0,
                Box::new(|index, workspace, window, cx| {
                    let template = index.checked_sub(1).and_then(|index| TEMPLATES.get(index));
                    run(
                        workspace,
                        window,
                        cx,
                        move |engine| engine.new_definition(None, template),
                        |created, workspace, window, cx| {
                            show_toast(
                                workspace,
                                format!("Created workflow '{}'", created.name),
                                cx,
                            );
                            open_file(created.file, workspace, window, cx);
                        },
                    )
                }),
                window,
                cx,
            );
        })
        .register_action(|workspace, _: &workflow::EditDefinition, window, cx| {
            if !WorkflowsSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE.to_string(), cx);
                return;
            }
            let Some(engine) = WorkflowEngine::ensure(cx) else {
                return;
            };
            // The task at point says which project's workflows come first; the first
            // definition by name until the run picker exists.
            let task = task_at_point(workspace, window, cx);
            edit_definition(&engine, task.as_ref(), workspace, window, cx);
        });
    register_run_commands(workspace);
}

/// The task at point, for the commands that read a project's workflows.
fn task_at_point(
    workspace: &Workspace,
    window: &Window,
    cx: &gpui::App,
) -> Option<bongos_work_model::store::Task> {
    let id = bongos_work::task_at_point(workspace, window, cx)?;
    let tree = bongos_work::WorkStore::global(cx)?.read(cx).snapshot()?;
    tree.task_by_id(&id).cloned()
}

/// `workflow: edit definition` for the first definition by name of the layers `task` reads: a
/// definition of a project repository opens in a new working copy, any other in an editor.
pub(crate) fn edit_definition(
    engine: &WorkflowEngine,
    task: Option<&bongos_work_model::store::Task>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match engine.edit_target(None, task, cx) {
        Ok(EditTarget::File(path)) => open_file(path, workspace, window, cx),
        Ok(EditTarget::WorkingCopy { repo, folder }) => {
            bongos_work::edit_in_working_copy(repo, Some(folder), workspace, window, cx)
        }
        Err(message) => show_toast(workspace, message, cx),
    }
}

/// The toast when a run command finds no run to act on.
pub const NO_RUN: &str = "No workflow run here; select a task with an active run";
/// The toast when `workflow: run` finds no task.
pub const NO_TASK: &str = "Select a leaf task to run its workflow";

fn register_run_commands(workspace: &mut Workspace) {
    on::<workflow::Run>(workspace, |engine, workspace, window, cx| {
        let Some(task_id) = bongos_work::task_at_point(workspace, window, cx) else {
            show_toast(workspace, NO_TASK.to_string(), cx);
            return;
        };
        crate::picker::run_picker(engine, task_id, workspace, window, cx);
    });
    on_run::<workflow::Pause>(workspace, |engine, run, workspace, _, cx| {
        let paused = engine.pause(&run, cx);
        report(
            workspace,
            paused.map(|()| "Pausing: running steps finish first"),
            cx,
        );
    });
    on_run::<workflow::Halt>(workspace, |engine, run, workspace, window, cx| {
        if let Err(refusal) = engine.halt(&run, cx) {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
        let Some(worktrees) = engine.halt_worktrees(&run) else {
            return;
        };
        let mut message = format!("Halted; opened {}", worktrees.run.display());
        if !worktrees.branches.is_empty() {
            let branches: Vec<String> = worktrees
                .branches
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            message.push_str(&format!(". Branch worktrees: {}", branches.join(", ")));
        }
        show_toast(workspace, message, cx);
        open_worktree(worktrees.run, window, cx);
    });
    on_run::<workflow::Resume>(workspace, |engine, run, workspace, _, cx| {
        let resumed = engine.resume(&run, cx);
        report(workspace, resumed.map(|()| "Workflow run resumed"), cx);
    });
    on_run::<workflow::RetryStep>(workspace, |engine, run, workspace, _, cx| {
        // An explicit node comes with the run picker; several failed steps are refused.
        let retried = engine.retry(&run, None, false, cx);
        report(workspace, retried.map(|()| "Retrying the failed step"), cx);
    });
    on_run::<workflow::RetryStepFresh>(workspace, |engine, run, workspace, _, cx| {
        let retried = engine.retry(&run, None, true, cx);
        report(
            workspace,
            retried.map(|()| "Retrying the failed step with the definition read again"),
            cx,
        );
    });
    on_run::<workflow::Restart>(workspace, |engine, run, workspace, _, cx| {
        let restarted = engine.restart(&run, cx);
        report(workspace, restarted.map(|_| "Workflow run restarted"), cx);
    });
    on_run::<workflow::Cancel>(workspace, |engine, run, workspace, _, cx| {
        let cancelled = engine.cancel(&run, cx);
        report(workspace, cancelled.map(|()| "Workflow run cancelled"), cx);
    });
    on_run::<workflow::AllowUnsandboxedOnce>(workspace, |_, run, workspace, window, cx| {
        answer_consent(workspace, run, true, window, cx);
    });
    on_run::<workflow::RefuseUnsandboxed>(workspace, |_, run, workspace, window, cx| {
        answer_consent(workspace, run, false, window, cx);
    });
    on_run::<workflow::Approve>(workspace, |engine, run, workspace, window, cx| {
        let Some(key) = engine.open_gates(&run).into_iter().next() else {
            show_toast(
                workspace,
                "The run is not waiting at a gate".to_string(),
                cx,
            );
            return;
        };
        let fields = engine.gate_fields(&run, &key);
        if fields.is_empty() {
            let approved = engine.approve(&run, &key, None, cx);
            report(workspace, approved.map(|()| "Approved"), cx);
            return;
        }
        bongos_work::pick(
            workspace,
            EDIT_FIRST,
            approve_choices(&fields),
            0,
            Box::new(move |index, workspace, window, cx| {
                let Some(field) = index.checked_sub(1).and_then(|index| fields.get(index)) else {
                    let approved = engine.approve(&run, &key, None, cx);
                    report(workspace, approved.map(|()| "Approved"), cx);
                    return;
                };
                let field = field.clone();
                let initial = field_text(&field.value);
                bongos_work::ask(
                    workspace,
                    format!("New value of {}", field.name),
                    &initial,
                    false,
                    Box::new(move |text, workspace, _, cx| {
                        match parse_field(&field.field_type, &text) {
                            Ok(value) => {
                                let edit = Some((field.name.clone(), value));
                                let approved = engine.approve(&run, &key, edit, cx);
                                report(workspace, approved.map(|()| "Approved with the edit"), cx);
                            }
                            Err(message) => show_toast(workspace, message, cx),
                        }
                    }),
                    window,
                    cx,
                );
            }),
            window,
            cx,
        );
    });
    on::<workflow::ClearFinished>(workspace, clear_finished);
    on::<workflow::DumpEngine>(workspace, |engine, _, window, cx| {
        let text = engine.dump_text();
        let dir = logs_dir(cx);
        let written = cx.background_spawn(async move { write_dump(&dir, &text) });
        cx.spawn_in(window, async move |workspace, cx| {
            let result = written.await;
            workspace
                .update(cx, |workspace, cx| {
                    let message = match result {
                        Ok(path) => format!("Wrote {}", path.display()),
                        Err(error) => format!("Could not write the engine dump: {error}"),
                    };
                    show_toast(workspace, message, cx)
                })
                .ok();
        })
        .detach();
    });
    on_run::<workflow::Revise>(workspace, |engine, run, workspace, window, cx| {
        let Some(key) = engine.open_gates(&run).into_iter().next() else {
            show_toast(
                workspace,
                "The run is not waiting at a gate".to_string(),
                cx,
            );
            return;
        };
        bongos_work::ask(
            workspace,
            format!("What should {} change", key.node),
            "",
            false,
            Box::new(move |comment, workspace, _, cx| {
                let revised = engine.revise(&run, &key, comment, cx);
                report(workspace, revised.map(|()| "Sent back for revision"), cx);
            }),
            window,
            cx,
        );
    });
    on_run::<workflow::CopyResumeCommand>(workspace, |engine, run, workspace, _, cx| match engine
        .resume_command(&run)
    {
        Ok(command) => {
            cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
            show_toast(workspace, format!("Copied: {command}"), cx);
        }
        Err(refusal) => show_toast(workspace, refusal.to_string(), cx),
    });
}

/// Registers the handler of `A`: the off toast while `workflows.enabled` is false, else `then`
/// with the engine.
pub(crate) fn on<A: Action>(
    workspace: &mut Workspace,
    then: impl Fn(WorkflowEngine, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    workspace.register_action(move |workspace, _: &A, window, cx| {
        if !WorkflowsSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        if route_remote::<A>(workspace, window, cx) {
            return;
        }
        if let Some(engine) = WorkflowEngine::ensure(cx) {
            then(engine, workspace, window, cx);
        }
    });
}

/// The command `A` as it is sent to a writer, and whether it reads the definition again.
fn remote_verb<A: Action>() -> Option<(WorkflowVerb, bool)> {
    let id = std::any::TypeId::of::<A>();
    let is = |other: std::any::TypeId| id == other;
    let verbs = [
        (
            std::any::TypeId::of::<workflow::Run>(),
            WorkflowVerb::Start,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Pause>(),
            WorkflowVerb::Pause,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Halt>(),
            WorkflowVerb::Halt,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Resume>(),
            WorkflowVerb::Resume,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::RetryStep>(),
            WorkflowVerb::Retry,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::RetryStepFresh>(),
            WorkflowVerb::Retry,
            true,
        ),
        (
            std::any::TypeId::of::<workflow::Restart>(),
            WorkflowVerb::Restart,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Cancel>(),
            WorkflowVerb::Cancel,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Approve>(),
            WorkflowVerb::Approve,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::Revise>(),
            WorkflowVerb::Revise,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::AllowUnsandboxedOnce>(),
            WorkflowVerb::AllowUnsandboxedOnce,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::RefuseUnsandboxed>(),
            WorkflowVerb::RefuseUnsandboxed,
            false,
        ),
        (
            std::any::TypeId::of::<workflow::ClearFinished>(),
            WorkflowVerb::ClearFinished,
            false,
        ),
    ];
    verbs
        .into_iter()
        .find(|(action, _, _)| is(*action))
        .map(|(_, verb, fresh)| (verb, fresh))
}

/// Sends the command `A` to the process that holds the engine, when this process does not: a
/// window that attached to another's writer. `false` when the command runs here. A client has no
/// engine to ask for the run's gate or the workflow's picker, so `workflow: run` starts the
/// workflow the todo names, `approve` approves as is and `revise` asks for its comment here.
fn route_remote<A: Action>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some((verb, fresh)) = remote_verb::<A>() else {
        return false;
    };
    let Some(remote) = InstalledWorkflowRemote::get(cx) else {
        return false;
    };
    let task_id = bongos_work::task_at_point(workspace, window, cx);
    if !remote.is_remote(task_id.as_deref(), cx) {
        return false;
    }
    if verb == WorkflowVerb::Start && task_id.is_none() {
        show_toast(workspace, NO_TASK.to_string(), cx);
        return true;
    }
    let payload = WorkflowPayload {
        run: crate::run_view::active_run(workspace, cx),
        task_id,
        fresh,
        ..WorkflowPayload::default()
    };
    if verb == WorkflowVerb::Revise {
        bongos_work::ask(
            workspace,
            "What should the run change".to_string(),
            "",
            false,
            Box::new(move |comment, workspace, window, cx| {
                let payload = WorkflowPayload {
                    comment: Some(comment),
                    ..payload.clone()
                };
                send_remote(remote.clone(), verb, payload, workspace, window, cx);
            }),
            window,
            cx,
        );
    } else {
        send_remote(remote, verb, payload, workspace, window, cx);
    }
    true
}

/// Answers the consent question of `run` as a person: through the writer's remote when another
/// process holds the engine, else on the local engine, with the outcome as a toast. The run is
/// the caller's, not the active item's, so a press on a view that is not in front answers its own
/// run.
pub(crate) fn answer_consent(
    workspace: &mut Workspace,
    run: RunId,
    allowed: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !WorkflowsSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        return;
    }
    if let Some(remote) = InstalledWorkflowRemote::get(cx) {
        let task_id = bongos_work::task_at_point(workspace, window, cx);
        if remote.is_remote(task_id.as_deref(), cx) {
            let verb = if allowed {
                WorkflowVerb::AllowUnsandboxedOnce
            } else {
                WorkflowVerb::RefuseUnsandboxed
            };
            let payload = WorkflowPayload {
                run: Some(run),
                task_id,
                ..WorkflowPayload::default()
            };
            send_remote(remote, verb, payload, workspace, window, cx);
            return;
        }
    }
    let Some(engine) = WorkflowEngine::ensure(cx) else {
        return;
    };
    let answered = engine.answer_unsandboxed(&run, allowed, crate::lifecycle::BY_PERSON, cx);
    let done = if allowed {
        "Allowed this run to go without a sandbox"
    } else {
        "Stopped: nothing ran without a sandbox"
    };
    report(workspace, answered.map(|()| done), cx);
}

fn send_remote(
    remote: std::rc::Rc<dyn WorkflowRemote>,
    verb: WorkflowVerb,
    payload: WorkflowPayload,
    _workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let sending = remote.send(verb, payload, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match sending.await {
            Ok(message) => message,
            Err(error) => format!("{error:#}"),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// [`on`] for a command on a run: the active run of the task at point, or the only active run.
pub(crate) fn on_run<A: Action>(
    workspace: &mut Workspace,
    then: impl Fn(WorkflowEngine, RunId, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    on::<A>(workspace, move |engine, workspace, window, cx| {
        // A run view in front is the run asked about; else the task at point.
        let in_view = crate::run_view::active_run(workspace, cx);
        let at_point = bongos_work::task_at_point(workspace, window, cx);
        let run = match (in_view, at_point) {
            (Some(run), _) => Some(run),
            (None, Some(task_id)) => engine.active_run_id(&task_id),
            (None, None) => match engine.active_runs().as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            },
        };
        match run {
            Some(run) => then(engine, run, workspace, window, cx),
            None => show_toast(workspace, NO_RUN.to_string(), cx),
        }
    });
}

/// Shows `done` as a toast, or the refusal with its fix.
fn report(workspace: &mut Workspace, result: Result<&str, Refusal>, cx: &mut Context<Workspace>) {
    let message = match result {
        Ok(done) => done.to_string(),
        Err(refusal) => {
            let mut message = refusal.to_string();
            if !refusal.offers.is_empty() {
                message.push_str(" (workflow: resume, workflow: cancel)");
            }
            message
        }
    };
    show_toast(workspace, message, cx);
}

/// The question of `workflow: approve` over a gate with a held result.
pub const EDIT_FIRST: &str = "Edit a field first?";
/// The choice that approves without an edit.
pub const APPROVE_AS_IS: &str = "Approve as is";

/// "Approve as is", then one row per field of the held result.
pub(crate) fn approve_choices(fields: &[GateField]) -> Vec<String> {
    let mut choices = vec![APPROVE_AS_IS.to_string()];
    choices.extend(
        fields
            .iter()
            .map(|field| format!("{}: {}", field.name, field_text(&field.value))),
    );
    choices
}

/// A field value as the line the value prompt starts with.
pub(crate) fn field_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(field_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn parse_number(text: &str) -> Result<Number, String> {
    let text = text.trim();
    if let Ok(whole) = text.parse::<i64>() {
        return Ok(Number::from(whole));
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .ok_or_else(|| format!("'{text}' is not a number"))
}

/// The value a typed line stands for in a field of `field_type`: text and paths and choices are
/// the line itself, a number is parsed, a list is its comma-separated items, each parsed as its
/// item type. A line that does not parse is the error to show.
pub(crate) fn parse_field(field_type: &FieldType, text: &str) -> Result<Value, String> {
    match field_type {
        FieldType::Text | FieldType::Path | FieldType::Choice(_) => {
            Ok(Value::String(text.to_string()))
        }
        FieldType::Number => parse_number(text).map(Value::Number),
        FieldType::Data => Err("this field cannot be edited here".to_string()),
        FieldType::List(item) => text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|part| match item {
                ItemType::Text | ItemType::Path => Ok(Value::String(part.to_string())),
                ItemType::Number => parse_number(part).map(Value::Number),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
    }
}

/// The file `workflow: dump engine` writes in the logs directory.
pub const ENGINE_DUMP_FILE: &str = "workflow-engine.txt";

/// Writes `text` to [`ENGINE_DUMP_FILE`] in `dir` and returns the path.
pub(crate) fn write_dump(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(ENGINE_DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Where the logs go: tests install their own directory.
#[cfg(test)]
pub(crate) struct LogsDir(pub PathBuf);

#[cfg(test)]
impl gpui::Global for LogsDir {}

fn logs_dir(cx: &gpui::App) -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = cx.try_global::<LogsDir>() {
        return dir.0.clone();
    }
    let _ = cx;
    paths::logs_dir().clone()
}

/// The moment `clear finished` measures ages from: tests pin it.
#[cfg(test)]
pub(crate) struct ClearNow(pub std::time::SystemTime);

#[cfg(test)]
impl gpui::Global for ClearNow {}

fn clear_now(cx: &gpui::App) -> std::time::SystemTime {
    #[cfg(test)]
    if let Some(now) = cx.try_global::<ClearNow>() {
        return now.0;
    }
    let _ = cx;
    std::time::SystemTime::now()
}

/// A cleared run as [`plan_clear`] reads it.
pub(crate) fn finished_run(cleared: ClearedRun) -> FinishedRun {
    FinishedRun {
        run_id: cleared.id,
        lifecycle: match cleared.status {
            RunStatus::Succeeded => RunLifecycle::Ended(EndState::Succeeded),
            _ => RunLifecycle::Cancelled,
        },
        finished: cleared.finished,
        worktree: cleared.worktree,
        attempt: cleared.attempt,
        checkout: cleared.checkout,
    }
}

/// The plan without the runs that have nothing to remove and nothing to skip.
pub(crate) fn tidy(mut plan: ClearPlan) -> ClearPlan {
    plan.runs
        .retain(|run| !run.remove.is_empty() || !run.skipped.is_empty());
    plan
}

/// How many worktrees the plan removes and how many it keeps.
pub(crate) fn plan_counts(plan: &ClearPlan) -> (usize, usize) {
    let remove = plan.runs.iter().map(|run| run.remove.len()).sum();
    let skipped = plan.runs.iter().map(|run| run.skipped.len()).sum();
    (remove, skipped)
}

/// The plan of `finished`: their worktrees older than `keep_days` before `now`, dirty ones
/// skipped, and a run whose recorded run directory is not the one the app made under
/// `worktrees_dir` (with `home` for `~`) kept whole. Writes nothing.
pub(crate) async fn plan_removal(
    git: &Git,
    finished: &[FinishedRun],
    worktrees_dir: Option<&Path>,
    home: &Path,
    keep_days: u64,
    now: std::time::SystemTime,
) -> Result<ClearPlan, GitError> {
    plan_clear(git, finished, worktrees_dir, home, keep_days, now)
        .await
        .map(tidy)
}

/// Applies a confirmed plan run by run, each from the main checkout of its task's own repository,
/// which the plan found from the task's worktree. A removal that fails is reported with its item
/// and the rest go on.
pub(crate) async fn apply_plan(git: &Git, plan: &ClearPlan) -> ClearReport {
    let mut report = ClearReport::default();
    for run in &plan.runs {
        match &run.repository {
            Some(root) => {
                let one = ClearPlan {
                    runs: vec![run.clone()],
                    saved: Vec::new(),
                    delete_saved: plan.delete_saved,
                };
                let applied = apply_clear(git, root, &one).await;
                report.removed.extend(applied.removed);
                report.skipped.extend(applied.skipped);
                report.kept_saved.extend(applied.kept_saved);
                report.deleted_saved.extend(applied.deleted_saved);
            }
            None => {
                for item in run.remove.iter().chain(&run.skipped) {
                    let reason = item
                        .reason
                        .clone()
                        .unwrap_or_else(|| "its repository is unknown".to_string());
                    report.skipped.push((item.clone(), reason));
                }
            }
        }
    }
    // The saved work on no item's branch, once; each entry names its own repository.
    let saved = ClearPlan {
        runs: Vec::new(),
        saved: plan.saved.clone(),
        delete_saved: plan.delete_saved,
    };
    let applied = apply_clear(git, Path::new(""), &saved).await;
    report.kept_saved.extend(applied.kept_saved);
    report.deleted_saved.extend(applied.deleted_saved);
    report
}

/// `workflow: clear finished`: the runs that ended succeeded or were cancelled leave the Runs
/// panel at once; their worktrees and branches older than `workflows.keep_finished_days` are
/// listed and removed after one yes-or-no question.
fn clear_finished(
    engine: WorkflowEngine,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let outcome = engine.clear_finished(cx);
    let mut notes = Vec::new();
    if !outcome.cleared.is_empty() {
        notes.push(format!("Cleared {} finished runs", outcome.cleared.len()));
    }
    for (run, error) in &outcome.failed {
        notes.push(format!("Could not clear run {run}: {error}"));
    }
    let finished: Vec<FinishedRun> = engine
        .cleared_runs(cx)
        .into_iter()
        .map(finished_run)
        .collect();
    if finished.is_empty() {
        if notes.is_empty() {
            notes.push("No finished runs to clear".to_string());
        }
        show_toast(workspace, notes.join(". "), cx);
        return;
    }
    let settings = WorkflowsSettings::get_global(cx);
    let keep_days = settings.keep_finished_days;
    let worktrees_dir = settings.worktrees_dir.clone();
    let home = paths::home_dir().clone();
    let now = clear_now(cx);
    let planned = cx.background_spawn(async move {
        plan_removal(
            &Git::new("git"),
            &finished,
            worktrees_dir.as_deref(),
            &home,
            keep_days,
            now,
        )
        .await
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let planned = planned.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                offer_removal(notes, planned, workspace, window, cx)
            })
            .ok();
    })
    .detach();
}

/// The second half of `workflow: clear finished`: shows the plan and asks once.
pub(crate) fn offer_removal(
    mut notes: Vec<String>,
    planned: Result<ClearPlan, GitError>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            notes.push(format!("Could not list the worktrees: {error}"));
            show_toast(workspace, notes.join(". "), cx);
            return;
        }
    };
    let (remove, skipped) = plan_counts(&plan);
    let saved = plan.saved_count();
    let weak = cx.entity().downgrade();
    if remove == 0 {
        if skipped > 0 || saved > 0 {
            notes.push(format!("Kept:\n{}", plan.summary()));
        } else if notes.is_empty() {
            notes.push("No finished runs to clear".to_string());
        }
        show_toast(workspace, notes.join(". "), cx);
        if saved > 0 {
            ask_delete_saved(plan, saved, workspace, window, cx);
        }
        return;
    }
    log::info!("workflows: clear finished would\n{}", plan.summary());
    notes.push(plan.summary());
    show_toast(workspace, notes.join("\n"), cx);
    bongos_minibuffer::yes_or_no(
        workspace,
        bongos_work::MINIBUFFER_NAME,
        format!(
            "Remove {remove} worktrees and branches of finished runs ({skipped} kept)? (y or n) "
        ),
        false,
        move |yes, window, cx| {
            if !yes {
                return;
            }
            if saved == 0 {
                apply_and_report(plan, weak, cx);
                return;
            }
            // The next question opens once this one has closed.
            window.defer(cx, move |window, cx| {
                weak.update(cx, |workspace, cx| {
                    ask_delete_saved(plan, saved, workspace, window, cx)
                })
                .ok();
            });
        },
        window,
        cx,
    );
}

/// Asks whether to delete the `saved` branches and refs holding saved work, `no` by default. The
/// plan is applied either way, with `delete_saved` set to the answer.
fn ask_delete_saved(
    plan: ClearPlan,
    saved: usize,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    bongos_minibuffer::yes_or_no(
        workspace,
        bongos_work::MINIBUFFER_NAME,
        format!("Also delete the {saved} branches and refs holding saved work? (y or n) "),
        false,
        move |yes, _, cx| {
            let mut plan = plan;
            plan.delete_saved = yes;
            apply_and_report(plan, weak, cx);
        },
        window,
        cx,
    );
}

/// Applies `plan` in the background and shows its report.
fn apply_and_report(plan: ClearPlan, weak: WeakEntity<Workspace>, cx: &mut App) {
    let applied = cx.background_spawn(async move { apply_plan(&Git::new("git"), &plan).await });
    cx.spawn(async move |cx| {
        let report = applied.await;
        weak.update(cx, |workspace, cx| {
            show_toast(workspace, report.to_string(), cx)
        })
        .ok();
    })
    .detach();
}

/// Opens `path` as a workspace of this window and activates it.
pub(crate) fn open_worktree(path: PathBuf, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    cx.spawn_in(window, async move |_, cx| {
        let opening = handle.update(cx, |multi, window, cx| {
            multi.open_project(vec![path], OpenMode::Activate, window, cx)
        });
        if let Ok(opening) = opening {
            opening.await.log_err();
        }
    })
    .detach();
}

/// Runs `job` on the engine in the background and hands its result to `done` on the workspace;
/// an error is shown as a toast. While workflows are off it shows the off toast instead.
fn run<T: Send + 'static>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    job: impl FnOnce(WorkflowEngine) -> Result<T, DefinitionError> + Send + 'static,
    done: impl FnOnce(T, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    if !WorkflowsSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        return;
    }
    let Some(engine) = WorkflowEngine::ensure(cx) else {
        return;
    };
    let result = cx.background_spawn(async move { job(engine) });
    cx.spawn_in(window, async move |workspace, cx| {
        let result = result.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(value) => done(value, workspace, window, cx),
                Err(error) => show_toast(workspace, error.to_string(), cx),
            })
            .ok();
    })
    .detach();
}

/// Opens `path` in an editor in the active pane.
fn open_file(
    path: PathBuf,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    debug_assert!(path.ends_with(DEFINITION_FILE));
    let open = workspace.open_abs_path(
        path,
        OpenOptions {
            visible: Some(OpenVisible::None),
            ..OpenOptions::default()
        },
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        if let Err(error) = open.await {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!("Could not open the definition: {error:#}"),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
}

/// Opens `text` in a read-only buffer titled `title` in the active pane. The buffer is in memory
/// only: it has no path, so opening it creates no file and closing it asks nothing.
pub(crate) fn open_report(
    title: String,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the diagnostics".to_string(), cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
            .ok();
    })
    .detach();
}
