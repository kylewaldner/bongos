//! Tests of the `lanes` settings and actions: off by default, hidden and toasting while off.

use std::time::Duration;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::actions::LastToast;
use crate::{
    ENGINE_HOST_MESSAGE, LanesHostSettings, LanesOnInterruption, LanesOnLastWindowClose,
    LanesSettings, LanesToast, OFF_MESSAGE, action_types,
};

const NAMES: &[&str] = &[
    "lanes::Show",
    "lanes::Start",
    "lanes::Stop",
    "lanes::Pause",
    "lanes::Resume",
    "lanes::DryRun",
    "lanes::Pick",
    "lanes::Adopt",
    "lanes::AcceptGraph",
    "lanes::Dump",
];

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
}

fn set_enabled(cx: &mut gpui::App, enabled: bool) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.lanes.get_or_insert_default().enabled = Some(enabled);
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
fn lanes_settings_are_off_by_default(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        assert_eq!(
            *LanesSettings::get_global(cx),
            LanesSettings {
                enabled: false,
                reevaluate_interval: Duration::from_secs(60),
                exit_when_idle: false,
                on_interruption: LanesOnInterruption::Ask,
                on_last_window_close: LanesOnLastWindowClose::Ask,
                desktop_notifications: true,
                dry_run_limit: 12,
                staleness_timeout: Duration::from_secs(60),
                save_max_file_bytes: 10_485_760,
                host: LanesHostSettings {
                    idle_exit: Duration::from_secs(300),
                    start_timeout: Duration::from_secs(15),
                    handover_timeout: Duration::from_secs(1800),
                    log_max_bytes: 1_048_576,
                },
            }
        );
    });
}

#[gpui::test]
fn lanes_user_settings_override_the_defaults(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    r#"{ "lanes": { "on_interruption": "resume", "on_last_window_close": "stop",
                         "dry_run_limit": 3, "host": { "idle_exit_seconds": 5 } } }"#,
                    cx,
                )
                .result()
                .unwrap();
        });
        let settings = LanesSettings::get_global(cx);
        assert_eq!(settings.on_interruption, LanesOnInterruption::Resume);
        assert_eq!(settings.on_last_window_close, LanesOnLastWindowClose::Stop);
        assert_eq!(settings.dry_run_limit, 3);
        assert_eq!(settings.host.idle_exit, Duration::from_secs(5));
        assert_eq!(
            settings.host.start_timeout,
            Duration::from_secs(15),
            "an unset key keeps its default"
        );
        assert!(!settings.enabled);
    });
}

#[gpui::test]
fn lanes_actions_are_hidden_while_off_and_shown_while_on(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(crate::init_palette);

    assert_eq!(NAMES.len(), action_types().len(), "every action is listed");
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden by default");
    }
    cx.update(|cx| set_enabled(cx, true));
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows while on");
    }
    cx.update(|cx| set_enabled(cx, false));
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden again while off");
    }
}

async fn workspace_with_actions(
    cx: &mut TestAppContext,
) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
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
    (workspace, cx)
}

/// Dispatches each action by name and returns the toast each one showed.
fn toasts(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    let id = NotificationId::unique::<LanesToast>();
    let mut shown = Vec::new();
    for name in NAMES {
        workspace.update(cx, |workspace, cx| workspace.dismiss_notification(&id, cx));
        let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
        assert!(
            workspace.read_with(cx, |workspace, _| workspace
                .notification_ids()
                .contains(&id)),
            "{name} shows a toast"
        );
        shown.push(cx.update(|_, cx| cx.global::<LastToast>().0.clone()));
    }
    shown
}

#[gpui::test]
async fn lanes_dispatch_by_name_shows_the_off_toast_while_off(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    assert_eq!(OFF_MESSAGE, "Lanes are off (lanes.enabled)");
    assert_eq!(toasts(&workspace, cx), vec![OFF_MESSAGE; NAMES.len()]);

    cx.update(|_, cx| set_enabled(cx, true));
    assert_eq!(
        toasts(&workspace, cx),
        vec![ENGINE_HOST_MESSAGE; NAMES.len()]
    );
}
