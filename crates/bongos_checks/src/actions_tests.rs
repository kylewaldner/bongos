//! Tests of the task check actions: hidden and toasting while `task_checks.enabled` is false,
//! shown while it is true.

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::actions::{LastToast, NOT_AVAILABLE_MESSAGE, OFF_MESSAGE};
use crate::command_logic::NO_TASK_MESSAGE;
use crate::{ChecksToast, TaskChecksSettings, action_types};

const NAMES: &[&str] = &[
    "task::RunChecks",
    "task::RunChecksClean",
    "task::RunCheck",
    "task::ShowChecks",
    "task::ShowCheckSources",
    "task::ShowCheckOutput",
    "task::EditChecks",
    "task::CancelChecks",
    "task::DumpChecks",
];

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_checks.get_or_insert_default().enabled = Some(enabled);
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
fn checks_default_settings_are_off_and_say_so(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let settings = TaskChecksSettings::get_global(cx);
        assert!(!settings.enabled);
        assert_eq!(settings.output_retention_days, 30);
        assert_eq!(settings.output_limit_mb, 256);
        assert_eq!(settings.result_file_limit_kb, 256);
        assert_eq!(settings.default_budget_minutes, None);
        assert_eq!(settings.no_verdict_reruns, 2);
        assert_eq!(settings.no_verdict_rerun_minutes, 5);
        assert_eq!(settings.pool_wait_limit_hours, 4);
        assert_eq!(settings.kill_grace_seconds, 5);
        assert_eq!(settings.parallel_parts_max, 4);
        assert_eq!(settings.tree_key_debounce_ms, 2000);
        assert_eq!(settings.live_write_ms, 1000);
        assert_eq!(settings.output_dir, None);
        assert_eq!(settings.scratch_dir, None);
        assert!(settings.scratch_folder().ends_with("checks-scratch"));
    });
}

#[gpui::test]
async fn checks_actions_are_hidden_while_off_and_shown_while_on(cx: &mut TestAppContext) {
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

fn set_enabled_in(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_checks.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn toast_shown(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<ChecksToast>())
    })
}

#[gpui::test]
async fn checks_dispatch_by_name_shows_the_off_toast_while_off(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    set_enabled_in(cx, false);
    for name in NAMES {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<ChecksToast>(), cx)
        });
        assert!(!toast_shown(&workspace, cx));
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
        cx.update(|_, cx| {
            assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}");
        });
    }
}

/// The commands that act on a task's checks; the others are not built yet.
const ACTING: &[&str] = &[
    "task::RunChecks",
    "task::RunChecksClean",
    "task::RunCheck",
    "task::CancelChecks",
    "task::DumpChecks",
];

#[gpui::test]
async fn checks_dispatch_by_name_acts_on_a_task_or_says_it_is_not_available_while_on(
    cx: &mut TestAppContext,
) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    set_enabled_in(cx, true);
    for name in NAMES {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<ChecksToast>(), cx)
        });
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows a toast");
        // With no Work tree there is no task to act on, and nothing runs.
        let expected = if ACTING.contains(name) {
            NO_TASK_MESSAGE
        } else {
            NOT_AVAILABLE_MESSAGE
        };
        cx.update(|_, cx| {
            assert_eq!(cx.global::<LastToast>().0, expected, "{name}");
        });
    }
}

#[test]
fn checks_the_off_toast_names_the_setting() {
    assert_eq!(OFF_MESSAGE, "Task checks are off (task_checks.enabled)");
}
