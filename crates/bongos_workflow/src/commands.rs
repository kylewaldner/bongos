//! The handlers of `workflow: validate`, `workflow: new` and `workflow: edit definition`. While
//! `workflows.enabled` is false each only shows the off toast; no engine exists and nothing is
//! read. File work runs on a background task and the result comes back to the workspace.

use bongos_workflow_model::layers::DEFINITION_FILE;
use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, Window};
use language::Capability;
use std::path::PathBuf;
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::WorkflowsSettings;
use crate::actions::{OFF_MESSAGE, show_toast, workflow};
use crate::definitions::DefinitionError;
use crate::engine::WorkflowEngine;
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
