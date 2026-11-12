//! Tests of `workflow: validate`, `workflow: new` and `workflow: edit definition` through
//! dispatching the actions: on while `workflows.enabled`, and nothing read or written while off.

use std::path::{Path, PathBuf};

use bongos_workflow_model::LayerDirs;
use bongos_workflow_model::layers::DEFINITION_FILE;
use editor::Editor;
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use settings::SettingsStore;
use tempfile::TempDir;
use workspace::Workspace;

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::commands::VALIDATE_TITLE;
use crate::engine::WorkflowEngine;

const BROKEN: &str = "schema = 1\n\
name = \"broken\"\n\
start = \"only\"\n\
\n\
[nodes.only]\n\
kind = \"program\"\n\
use = \"wait\"\n\
routes = [{ to = \"nowhere\" }]\n\
\n\
[ends.done]\n\
kind = \"success\"\n";

/// The workspace of `workspace_with_actions` over a temporary profile layer that holds `broken`.
struct Env {
    workspace: Entity<Workspace>,
    app_state: std::sync::Arc<workspace::AppState>,
    _temp: TempDir,
    profile: PathBuf,
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

async fn env(cx: &mut TestAppContext, enabled: bool) -> (Env, &mut VisualTestContext) {
    cx.update(editor::init);
    let (workspace, app_state, cx) = crate::actions_tests::workspace_with_actions(cx).await;
    if enabled {
        set_enabled(cx, true);
    }
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    std::fs::create_dir_all(profile.join("broken")).unwrap();
    std::fs::write(profile.join("broken").join(DEFINITION_FILE), BROKEN).unwrap();
    cx.update(|_, cx| {
        cx.set_global(WorkflowEngine::new(LayerDirs {
            profile: Some(profile.clone()),
            ..LayerDirs::default()
        }))
    });
    (
        Env {
            workspace,
            app_state,
            _temp: temp,
            profile,
        },
        cx,
    )
}

async fn seed(env: &Env, file: &Path, contents: &[u8]) {
    let fs = env.app_state.fs.clone();
    fs.create_dir(file.parent().unwrap()).await.unwrap();
    fs.as_fake().insert_file(file, contents.to_vec()).await;
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn active_editor(env: &Env, cx: &mut VisualTestContext) -> Option<Entity<Editor>> {
    env.workspace
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
}

/// Every path under `dir`, sorted.
fn tree(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for item in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            out.push(item.path());
            if item.path().is_dir() {
                walk(&item.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

#[gpui::test]
async fn validate_opens_the_diagnostics_in_a_read_only_buffer(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    dispatch("workflow::Validate", cx);

    let editor = active_editor(&env, cx).expect("the diagnostics buffer is open");
    editor.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx));
        assert_eq!(editor.buffer().read(cx).title(cx), VALIDATE_TITLE);
        let text = editor.text(cx);
        assert!(text.contains("broken (profile)"), "{text}");
        assert!(text.contains("error: [only] line 8: "), "{text}");
        assert!(text.contains("nowhere"), "{text}");
    });
}

#[gpui::test]
async fn new_writes_the_template_and_opens_it(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    // The editor reads through the workspace's file system, which is a fake one: put the file the
    // command is about to write there too.
    let file = env.profile.join("new-workflow").join(DEFINITION_FILE);
    seed(&env, &file, b"schema = 1\n").await;
    dispatch("workflow::New", cx);
    // The list of templates is open with "Minimal" first; confirming it writes the minimal one.
    dispatch("menu::Confirm", cx);

    let written = std::fs::read_to_string(&file).expect("the template is written");
    assert!(written.contains("name = \"new-workflow\""), "{written}");
    assert!(env.profile.join("new-workflow").join("work.md").is_file());
    let editor = active_editor(&env, cx).expect("the new definition is open");
    editor.read_with(cx, |editor, cx| {
        let path = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file().map(|file| file.full_path(cx)));
        assert!(
            path.is_some_and(|path| path.ends_with(DEFINITION_FILE)),
            "opened workflow.toml"
        );
    });

    // The written file validates clean through the same command.
    dispatch("workflow::Validate", cx);
    let editor = active_editor(&env, cx).unwrap();
    let text = editor.read_with(cx, |editor, cx| editor.text(cx));
    assert!(text.contains("new-workflow (profile)"), "{text}");
    assert!(
        text.contains("new-workflow (profile)") && text.contains(": ok"),
        "{text}"
    );
}

#[gpui::test]
async fn edit_definition_opens_the_resolved_file(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    let file = env.profile.join("broken").join(DEFINITION_FILE);
    seed(&env, &file, BROKEN.as_bytes()).await;
    let engine = cx.update(|_, cx| cx.global::<WorkflowEngine>().clone());
    assert_eq!(engine.definition_path(None).unwrap(), file);

    dispatch("workflow::EditDefinition", cx);
    let editor = active_editor(&env, cx).expect("the definition is open");
    editor.read_with(cx, |editor, cx| {
        assert_eq!(editor.text(cx), BROKEN);
        assert!(!editor.read_only(cx));
    });
}

#[gpui::test]
async fn edit_definition_with_no_definition_says_so(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    std::fs::remove_dir_all(env.profile.join("broken")).unwrap();
    dispatch("workflow::EditDefinition", cx);
    assert!(active_editor(&env, cx).is_none());
    cx.update(|_, cx| {
        assert_eq!(
            cx.global::<LastToast>().0,
            "There are no workflow definitions"
        )
    });
}

#[gpui::test]
async fn nothing_is_read_or_written_while_off(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, false).await;
    let before = tree(&env.profile);
    let fake_before = env.app_state.fs.as_fake().paths(true);
    for name in [
        "workflow::Validate",
        "workflow::New",
        "workflow::EditDefinition",
        "workflow::Run",
        "workflow::Pause",
        "workflow::Halt",
        "workflow::Resume",
        "workflow::RetryStep",
        "workflow::RetryStepFresh",
        "workflow::Restart",
        "workflow::Cancel",
        "workflow::Approve",
        "workflow::Revise",
        "workflow::CopyResumeCommand",
    ] {
        dispatch(name, cx);
        cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}"));
        assert!(active_editor(&env, cx).is_none(), "{name} opens nothing");
    }
    assert_eq!(tree(&env.profile), before, "nothing was written");
    assert_eq!(env.app_state.fs.as_fake().paths(true), fake_before);
}

#[gpui::test]
async fn no_engine_exists_while_off_and_one_is_made_on_demand_when_on(cx: &mut TestAppContext) {
    let (workspace, _, cx) = crate::actions_tests::workspace_with_actions(cx).await;
    let _ = workspace;
    cx.update(|_, cx| {
        assert!(cx.try_global::<WorkflowEngine>().is_none(), "none at init");
        assert!(WorkflowEngine::ensure(cx).is_none(), "none while off");
        assert!(cx.try_global::<WorkflowEngine>().is_none());
    });
    for name in [
        "workflow::Validate",
        "workflow::New",
        "workflow::EditDefinition",
    ] {
        dispatch(name, cx);
    }
    cx.update(|_, cx| {
        assert!(
            cx.try_global::<WorkflowEngine>().is_none(),
            "no command made one"
        )
    });

    set_enabled(cx, true);
    cx.update(|_, cx| {
        let engine = WorkflowEngine::ensure(cx).expect("an engine while on");
        let dirs = engine.dirs();
        assert!(
            dirs.profile
                .as_ref()
                .unwrap()
                .ends_with(".bongos/workflows")
        );
        assert!(
            dirs.shipped
                .as_ref()
                .unwrap()
                .ends_with("workflows/shipped/0.0.0")
        );
        assert!(dirs.project.is_none());
        assert!(cx.try_global::<WorkflowEngine>().is_some());
    });
}
