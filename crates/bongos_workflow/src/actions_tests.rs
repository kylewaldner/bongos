//! Tests of the `workflow` actions: shown by default, hidden and toasting while `workflows.enabled`
//! is set to false, and touching nothing then.

use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::{WorkflowToast, WorkflowsSettings, action_types};
use command_palette_hooks::CommandPaletteFilter;

const NAMES: &[&str] = &[
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
    "workflow::OpenLogs",
    "workflow::ShowRuns",
    "workflow::ShowRun",
    "workflow::OpenRunWorktree",
    "workflow::ShowRunDiff",
    "workflow::ClearFinished",
    "workflow::Validate",
    "workflow::New",
    "workflow::EditDefinition",
    "workflow::CopyResumeCommand",
    "workflow::ToggleGraph",
    "workflow::DumpRunView",
    "workflow::DumpEngine",
];

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        // Not `crate::init`: with workflows on it would write the shipped definitions into the
        // real data directory, and these tests need the setting, not the engine.
        WorkflowsSettings::register(cx);
    });
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

fn palette_hidden(name: &str, cx: &mut TestAppContext) -> bool {
    cx.update(|cx| {
        let action = cx
            .build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"));
        CommandPaletteFilter::try_global(cx)
            .expect("the palette filter exists")
            .is_hidden(&*action)
    })
}

#[gpui::test]
fn default_settings_are_on(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        assert!(WorkflowsSettings::get_global(cx).enabled);
        assert_eq!(
            *WorkflowsSettings::get_global(cx),
            WorkflowsSettings::default(),
            "the struct default mirrors default.json"
        );
    });
}

#[gpui::test]
async fn actions_are_shown_by_default_and_hidden_while_set_off(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(crate::init_palette);

    assert_eq!(NAMES.len(), action_types().len(), "every action is listed");
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows by default");
    }
    set_enabled(cx, false);
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden while set off");
    }
    set_enabled(cx, true);
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows again");
    }
}

/// A workspace with the actions registered and `workflows.enabled` set to false before the init,
/// so no engine exists; a test that wants workflows on calls `set_enabled(cx, true)`.
pub(crate) async fn workspace_with_actions(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<Workspace>,
    std::sync::Arc<AppState>,
    &mut VisualTestContext,
) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(false);
            });
        });
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update_in(cx, |workspace, window, cx| {
        crate::register(workspace);
        window.focus(&gpui::Focusable::focus_handle(workspace, cx), cx)
    });
    cx.run_until_parked();
    (workspace, app_state, cx)
}

fn toast_shown(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<WorkflowToast>())
    })
}

#[gpui::test]
async fn dispatch_by_name_shows_the_off_toast_and_touches_nothing(cx: &mut TestAppContext) {
    let (workspace, app_state, cx) = workspace_with_actions(cx).await;
    let fake_fs = app_state.fs.as_fake();
    let before = fake_fs.paths(true);
    for name in NAMES {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<WorkflowToast>(), cx)
        });
        assert!(!toast_shown(&workspace, cx));
        let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
        cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}"));
    }
    assert_eq!(fake_fs.paths(true), before, "nothing was written");
}

#[test]
fn the_off_toast_names_the_setting() {
    assert_eq!(OFF_MESSAGE, "Workflows are off (workflows.enabled)");
}
