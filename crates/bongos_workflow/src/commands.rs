//! The handlers of `workflow: validate`, `workflow: new` and `workflow: edit definition`, and of
//! the run commands (`run`, `pause`, `halt`, `resume`, `retry step`, `retry step fresh`,
//! `restart`, `cancel`, `approve`, `revise`, `copy resume command`). While `workflows.enabled` is
//! false each only shows the off toast; no engine exists and nothing is read. File work runs on a
//! background task and the result comes back to the workspace. The run commands act on the active
//! run of the task at point and are thin: the decisions are the engine's.

use bongos_workflow_model::layers::DEFINITION_FILE;
use editor::{Editor, MultiBuffer};
use gpui::{Action, AppContext as _, ClipboardItem, Context, Window};
use language::Capability;
use std::path::PathBuf;
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, OpenOptions, OpenVisible, Workspace};

use crate::WorkflowsSettings;
use crate::actions::{OFF_MESSAGE, show_toast, workflow};
use crate::definitions::DefinitionError;
use crate::engine::WorkflowEngine;
use crate::lifecycle::{Refusal, RunId, StartRequest};
use settings::Settings as _;

/// The title of the diagnostics buffer of `workflow: validate`.
pub const VALIDATE_TITLE: &str = "Workflow diagnostics";

pub(crate) fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &workflow::Validate, window, cx| {
            run(
                workspace,
                window,
                cx,
                |engine| engine.validate(None),
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
            run(
                workspace,
                window,
                cx,
                |engine| engine.new_definition(None),
                |created, workspace, window, cx| {
                    show_toast(
                        workspace,
                        format!("Created workflow '{}'", created.name),
                        cx,
                    );
                    open_file(created.file, workspace, window, cx);
                },
            )
        })
        .register_action(|workspace, _: &workflow::EditDefinition, window, cx| {
            // The first definition by name until the picker of step 17.
            run(
                workspace,
                window,
                cx,
                |engine| engine.definition_path(None),
                |path, workspace, window, cx| open_file(path, workspace, window, cx),
            )
        });
    register_run_commands(workspace);
}

/// The toast when a run command finds no run to act on.
pub const NO_RUN: &str = "No workflow run here; select a task with an active run";
/// The toast when `workflow: run` finds no task.
pub const NO_TASK: &str = "Select a leaf task to run its workflow";

fn register_run_commands(workspace: &mut Workspace) {
    on::<workflow::Run>(workspace, |engine, workspace, window, cx| {
        // The todo's named workflow until the picker of step 17.
        let Some(task_id) = bongos_work::task_at_point(workspace, window, cx) else {
            show_toast(workspace, NO_TASK.to_string(), cx);
            return;
        };
        let started = engine.start(StartRequest::new(task_id), cx);
        report(workspace, started.map(|_| "Workflow run started"), cx);
    });
    on_run::<workflow::Pause>(workspace, |engine, run, workspace, _, cx| {
        let paused = engine.pause(&run, cx);
        report(workspace, paused.map(|()| "Pausing: running steps finish first"), cx);
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
        // An explicit node comes with the picker of step 17; several failed steps are refused.
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
    on_run::<workflow::Approve>(workspace, |engine, run, workspace, _, cx| {
        let Some(key) = engine.open_gates(&run).into_iter().next() else {
            show_toast(workspace, "The run is not waiting at a gate".to_string(), cx);
            return;
        };
        let approved = engine.approve(&run, &key, None, cx);
        report(workspace, approved.map(|()| "Approved"), cx);
    });
    on_run::<workflow::Revise>(workspace, |engine, run, workspace, window, cx| {
        let Some(key) = engine.open_gates(&run).into_iter().next() else {
            show_toast(workspace, "The run is not waiting at a gate".to_string(), cx);
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
    on_run::<workflow::CopyResumeCommand>(workspace, |engine, run, workspace, _, cx| {
        match engine.resume_command(&run) {
            Ok(command) => {
                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                show_toast(workspace, format!("Copied: {command}"), cx);
            }
            Err(refusal) => show_toast(workspace, refusal.to_string(), cx),
        }
    });
}

/// Registers the handler of `A`: the off toast while `workflows.enabled` is false, else `then`
/// with the engine.
fn on<A: Action>(
    workspace: &mut Workspace,
    then: impl Fn(WorkflowEngine, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    workspace.register_action(move |workspace, _: &A, window, cx| {
        if !WorkflowsSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        if let Some(engine) = WorkflowEngine::ensure(cx) {
            then(engine, workspace, window, cx);
        }
    });
}

/// [`on`] for a command on a run: the active run of the task at point, or the only active run.
fn on_run<A: Action>(
    workspace: &mut Workspace,
    then: impl Fn(WorkflowEngine, RunId, &mut Workspace, &mut Window, &mut Context<Workspace>)
    + 'static,
) {
    on::<A>(workspace, move |engine, workspace, window, cx| {
        let at_point = bongos_work::task_at_point(workspace, window, cx);
        let run = match at_point {
            Some(task_id) => engine.active_run_id(&task_id),
            None => match engine.active_runs().as_slice() {
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
fn report(
    workspace: &mut Workspace,
    result: Result<&str, Refusal>,
    cx: &mut Context<Workspace>,
) {
    let message = match result {
        Ok(done) => done.to_string(),
        Err(refusal) => refusal.to_string(),
    };
    show_toast(workspace, message, cx);
}

/// Opens `path` as a workspace of this window and activates it (03 D21's way of focusing).
fn open_worktree(path: PathBuf, window: &mut Window, cx: &mut Context<Workspace>) {
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
fn open_report(
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
