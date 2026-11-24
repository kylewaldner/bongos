//! The workflow actions in the whole app: with `workflows.enabled` set to false a run
//! started the way `task: mode` starts it, by the name `workflow::Run`, finds a handler and says
//! that workflows are off, instead of "Workflows arrive with feature 20".

use gpui::{BorrowAppContext as _, Focusable as _, TestAppContext};
use serde_json::json;
use util::path;
use workspace::MultiWorkspace;
use workspace::notifications::NotificationId;

use super::tests::init_test;

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn run_by_name_is_handled_and_says_workflows_are_off(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    set_enabled(cx, false);
    cx.update(bongos_workflow::init);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "a": null }))
        .await;
    cx.update(|cx| {
        workspace::open_paths(
            &[path!("/root/a").into()],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();

    let multi_workspace = cx.windows()[0].downcast::<MultiWorkspace>().unwrap();
    let workspace = multi_workspace
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let id = NotificationId::unique::<bongos_workflow::WorkflowToast>();
    assert!(!workspace.read_with(cx, |workspace, _| {
        workspace.notification_ids().contains(&id)
    }));

    // The two checks `task: mode` makes before it would show "Workflows arrive with feature 20".
    let available = multi_workspace
        .update(cx, |_, window, cx| {
            let action = cx.build_action("workflow::Run", None).unwrap();
            let available = window.is_action_available(action.as_ref(), cx);
            window.dispatch_action(action, cx);
            available
        })
        .unwrap();
    cx.run_until_parked();
    assert!(available, "workflow::Run has a handler");
    assert!(workspace.read_with(cx, |workspace, _| {
        workspace.notification_ids().contains(&id)
    }));
}

#[gpui::test]
async fn the_runs_panel_is_absent_while_workflows_are_off_and_follows_the_setting(
    cx: &mut TestAppContext,
) {
    let app_state = init_test(cx);
    set_enabled(cx, false);
    cx.update(bongos_workflow::init);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "a": null }))
        .await;
    cx.update(|cx| {
        workspace::open_paths(
            &[path!("/root/a").into()],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();
    let multi_workspace = cx.windows()[0].downcast::<MultiWorkspace>().unwrap();
    let workspace = multi_workspace
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    multi_workspace
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                bongos_workflow::init_panel(workspace, window, cx)
            })
        })
        .unwrap();
    cx.run_until_parked();
    let has_panel = |cx: &mut TestAppContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<bongos_workflow::RunsPanel>(cx).is_some()
        })
    };
    assert!(!has_panel(cx), "absent while workflows.enabled is off");

    set_enabled(cx, true);
    assert!(has_panel(cx), "added when workflows are turned on");
    set_enabled(cx, false);
    assert!(!has_panel(cx), "removed when workflows are turned off");
}

/// What a workflow action answers while workflows are on and there is no task and no run.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// A workflow toast.
    Toast,
    /// A picker (a modal, or the minibuffer).
    Picker,
    /// An item opened in the active pane.
    Item,
    /// The Runs panel takes focus.
    Panel,
}

/// Every `workflow::*` action with its answer. A new action fails
/// `every_workflow_action_is_covered` until it is listed here.
const ANSWERS: &[(&str, Answer)] = &[
    // No task at point.
    ("workflow::Run", Answer::Toast),
    // No run to act on.
    ("workflow::Pause", Answer::Toast),
    ("workflow::Halt", Answer::Toast),
    ("workflow::Resume", Answer::Toast),
    ("workflow::RetryStep", Answer::Toast),
    ("workflow::RetryStepFresh", Answer::Toast),
    ("workflow::Restart", Answer::Toast),
    ("workflow::Cancel", Answer::Toast),
    ("workflow::Approve", Answer::Toast),
    ("workflow::Revise", Answer::Toast),
    ("workflow::CancelGate", Answer::Toast),
    ("workflow::OpenLogs", Answer::Toast),
    ("workflow::ShowRun", Answer::Toast),
    ("workflow::OpenRunWorktree", Answer::Toast),
    ("workflow::ShowRunDiff", Answer::Toast),
    ("workflow::CopyResumeCommand", Answer::Toast),
    ("workflow::DumpRunView", Answer::Toast),
    // No run view open.
    ("workflow::ToggleGraph", Answer::Toast),
    // "No finished runs to clear".
    ("workflow::ClearFinished", Answer::Toast),
    // "Wrote <path>" for the engine dump.
    ("workflow::DumpEngine", Answer::Toast),
    ("workflow::ShowRuns", Answer::Panel),
    ("workflow::Validate", Answer::Item),
    ("workflow::New", Answer::Picker),
    ("workflow::EditDefinition", Answer::Item),
];

/// A temporary directory for the engine roots and the data directory, removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Points the engine and the logs at `scratch`, so no test writes the real data directory. Call
/// before anything reads the data directory: each test runs in its own process.
fn isolate(scratch: &Scratch, cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(bongos_workflow::EngineRoots {
            data_dir: scratch.0.join("data"),
            home: scratch.0.join("home"),
        });
    });
}

async fn open_workspace(
    app_state: &std::sync::Arc<workspace::AppState>,
    cx: &mut TestAppContext,
) -> (
    gpui::WindowHandle<MultiWorkspace>,
    gpui::Entity<workspace::Workspace>,
) {
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "a": "text" }))
        .await;
    cx.update(|cx| {
        workspace::open_paths(
            &[path!("/root/a").into()],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();
    let multi_workspace = cx.windows()[0].downcast::<MultiWorkspace>().unwrap();
    let workspace = multi_workspace
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    (multi_workspace, workspace)
}

#[gpui::test]
async fn every_workflow_action_is_covered(cx: &mut TestAppContext) {
    let scratch = Scratch::new("bongos-workflow-actions-covered");
    paths::set_custom_data_dir(scratch.0.join("app").to_str().unwrap());
    init_test(cx);
    cx.update(bongos_workflow::init);
    let mut registered: Vec<&str> = cx.update(|cx| {
        cx.all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("workflow::"))
            .collect()
    });
    registered.sort_unstable();
    let mut listed: Vec<&str> = ANSWERS.iter().map(|(name, _)| *name).collect();
    listed.sort_unstable();
    assert_eq!(
        registered, listed,
        "the workflow actions and ANSWERS differ"
    );
    assert_eq!(
        bongos_workflow::action_types().len(),
        ANSWERS.len(),
        "the palette list and ANSWERS differ"
    );
}

#[gpui::test]
async fn every_workflow_action_answers_with_no_task_and_no_run(cx: &mut TestAppContext) {
    let scratch = Scratch::new("bongos-workflow-actions-answer");
    paths::set_custom_data_dir(scratch.0.join("app").to_str().unwrap());
    let app_state = init_test(cx);
    set_enabled(cx, true);
    isolate(&scratch, cx);
    cx.update(bongos_workflow::init);
    let (multi_workspace, workspace) = open_workspace(&app_state, cx).await;
    multi_workspace
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                bongos_workflow::init_panel(workspace, window, cx)
            })
        })
        .unwrap();
    cx.run_until_parked();
    let id = NotificationId::unique::<bongos_workflow::WorkflowToast>();

    for (name, answer) in ANSWERS {
        let available = multi_workspace
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.dismiss_notification(&id, cx);
                    workspace.hide_modal(window, cx);
                });
                let action = cx.build_action(name, None).unwrap();
                let available = window.is_action_available(action.as_ref(), cx);
                window.dispatch_action(action, cx);
                available
            })
            .unwrap();
        cx.run_until_parked();
        assert!(available, "{name} has a handler");
        let (toast, modal, item, panel) = multi_workspace
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    (
                        workspace.notification_ids().contains(&id),
                        workspace.has_active_modal(window, cx),
                        workspace
                            .active_item(cx)
                            .map(|item| item.tab_content_text(0, cx)),
                        workspace
                            .panel::<bongos_workflow::RunsPanel>(cx)
                            .map(|panel| panel.focus_handle(cx).contains_focused(window, cx)),
                    )
                })
            })
            .unwrap();
        match answer {
            Answer::Toast => assert!(toast, "{name} shows a toast"),
            Answer::Picker => assert!(modal, "{name} opens a picker"),
            Answer::Item => assert!(
                item.is_some_and(|title| title != "a"),
                "{name} opens an item"
            ),
            Answer::Panel => assert_eq!(panel, Some(true), "{name} focuses the Runs panel"),
        }
    }
}

/// Loads the keymaps the app loads on Linux with the Emacs base.
fn load_app_keymap(cx: &mut gpui::App) {
    let mut all = Vec::new();
    for (path, source) in [
        (
            "keymaps/default-linux.json",
            settings::KeybindSource::Default,
        ),
        ("keymaps/linux/emacs.json", settings::KeybindSource::Base),
        (
            "keymaps/specific-overrides.json",
            settings::KeybindSource::Default,
        ),
    ] {
        all.extend(
            settings::KeymapFile::load_asset(path, Some(source), cx)
                .unwrap_or_else(|error| panic!("{path}: {error}")),
        );
    }
    cx.bind_keys(all);
}

/// Types `keys` in the focused editor of a workspace with workflows on and the app keymap.
/// Returns how many times `RetryStep` and `RetryStepFresh` reached the editor, and whether a
/// workflow toast appeared.
async fn type_retry_keys(keys: &str, cx: &mut TestAppContext) -> (usize, usize, bool) {
    let scratch = Scratch::new("bongos-workflow-keys");
    paths::set_custom_data_dir(scratch.0.join("app").to_str().unwrap());
    let app_state = init_test(cx);
    set_enabled(cx, true);
    isolate(&scratch, cx);
    cx.update(|cx| {
        bongos_emacs::init(cx);
        load_app_keymap(cx);
        bongos_workflow::init(cx);
    });
    let (multi_workspace, workspace) = open_workspace(&app_state, cx).await;
    let mut cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
    cx.run_until_parked();
    let plain = std::rc::Rc::new(std::cell::Cell::new(0));
    let fresh = std::rc::Rc::new(std::cell::Cell::new(0));
    workspace.update_in(&mut cx, |workspace, window, cx| {
        let editor = workspace
            .active_item_as::<editor::Editor>(cx)
            .expect("the file is open in an editor");
        let (plain, fresh) = (plain.clone(), fresh.clone());
        editor.update(cx, |editor, _| {
            editor
                .register_action(move |_: &bongos_workflow::workflow::RetryStep, _, cx| {
                    plain.set(plain.get() + 1);
                    cx.propagate();
                })
                .detach();
            editor
                .register_action(
                    move |_: &bongos_workflow::workflow::RetryStepFresh, _, cx| {
                        fresh.set(fresh.get() + 1);
                        cx.propagate();
                    },
                )
                .detach();
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
    let id = NotificationId::unique::<bongos_workflow::WorkflowToast>();
    let toast = workspace.read_with(&cx, |workspace, _| {
        workspace.notification_ids().contains(&id)
    });
    (plain.get(), fresh.get(), toast)
}

#[gpui::test]
async fn c_u_c_c_f_t_reaches_retry_step_fresh(cx: &mut TestAppContext) {
    let (plain, fresh, toast) = type_retry_keys("ctrl-u ctrl-c f t", cx).await;
    assert_eq!(
        (plain, fresh),
        (0, 1),
        "C-u C-c f t runs RetryStepFresh only"
    );
    assert!(toast, "the handler of RetryStepFresh answered");
}

#[gpui::test]
async fn plain_c_c_f_t_reaches_retry_step(cx: &mut TestAppContext) {
    let (plain, fresh, toast) = type_retry_keys("ctrl-c f t", cx).await;
    assert_eq!((plain, fresh), (1, 0), "C-c f t runs RetryStep only");
    assert!(toast, "the handler of RetryStep answered");
}
