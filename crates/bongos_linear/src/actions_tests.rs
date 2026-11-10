//! Tests of the `task` and `linear` actions: hidden and toasting while `linear.enabled` is false,
//! and never reaching a handler then.

use std::sync::Arc;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::{LinearSettings, LinearToast, action_types};

const NAMES: &[&str] = &[
    "task::NewFromLinear",
    "task::LinkToLinear",
    "task::UnlinkFromLinear",
    "task::OpenInLinear",
    "task::CreateMissingSubtickets",
    "task::RefreshFromLinear",
    "task::ShowLinearSuggestions",
    "task::CopyPrTitle",
    "linear::SetToken",
    "linear::ClearToken",
    "linear::DumpState",
];

pub(crate) fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
}

pub(crate) fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.linear.get_or_insert_default().enabled = Some(enabled);
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
fn default_settings_are_off(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| assert!(!LinearSettings::get_global(cx).enabled));
}

#[gpui::test]
async fn actions_are_hidden_while_off_and_shown_while_on(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(crate::init_palette);

    assert_eq!(NAMES.len(), action_types().len(), "every action is listed");
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden by default");
    }

    set_enabled(cx, true);
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows while on");
    }

    set_enabled(cx, false);
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden again");
    }
}

pub(crate) async fn workspace_with_actions(
    cx: &mut TestAppContext,
) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        crate::init(cx);
        // No test may reach the application's credential store.
        crate::token::install_credentials_provider(
            Arc::new(crate::token_tests::FakeCredentials::default()),
            cx,
        );
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
    (workspace, cx)
}

pub(crate) fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

pub(crate) fn toast_shown(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<LinearToast>())
    })
}

pub(crate) fn set_enabled_in(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.linear.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn dispatch_by_name_shows_the_off_toast(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    // The default settings leave the sync off.
    for name in NAMES {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<LinearToast>(), cx)
        });
        assert!(!toast_shown(&workspace, cx));
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
        cx.update(|_, cx| {
            assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}");
        });
    }
}

#[test]
fn the_off_toast_names_the_setting() {
    assert_eq!(OFF_MESSAGE, "Linear sync is off (linear.enabled)");
}
