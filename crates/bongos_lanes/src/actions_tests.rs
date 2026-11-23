//! Tests of the `lanes` settings and actions: on by default, hidden and toasting while off.

use std::rc::Rc;
use std::time::Duration;

use bongos_lanes_model::{Author, LaneId, TaskId};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::actions::{LastChoices, LastToast, PendingAdopt};
use crate::dispatcher_tests::adopt_tests::TreeHarness;
use crate::{
    DROP_CHOICES, ENGINE_HOST_MESSAGE, Lanes, LanesHostSettings, LanesOnInterruption,
    LanesOnLastWindowClose, LanesSettings, LanesToast, OFF_MESSAGE, ONE_PROJECT_MESSAGE,
    WindowLanes, action_types,
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
    "task::Run",
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
fn lanes_settings_are_on_by_default(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        assert_eq!(
            *LanesSettings::get_global(cx),
            LanesSettings {
                enabled: true,
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
        assert!(settings.enabled);
    });
}

#[gpui::test]
fn lanes_actions_are_shown_by_default_and_hidden_while_off(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(crate::init_palette);

    assert_eq!(NAMES.len(), action_types().len(), "every action is listed");
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows by default");
    }
    cx.update(|cx| set_enabled(cx, false));
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden while off");
    }
    cx.update(|cx| set_enabled(cx, true));
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows again while on");
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
    cx.update(|_, cx| set_enabled(cx, false));
    assert_eq!(toasts(&workspace, cx), vec![OFF_MESSAGE; NAMES.len()]);

    // While on, the commands of the engine host say this window does not reach it; accept graph,
    // show and dry run read the project here and need the one project repository open in it.
    cx.update(|_, cx| set_enabled(cx, true));
    let expected: Vec<&str> = NAMES
        .iter()
        .map(|name| match *name {
            "lanes::AcceptGraph" | "lanes::Show" | "lanes::DryRun" => ONE_PROJECT_MESSAGE,
            _ => ENGINE_HOST_MESSAGE,
        })
        .collect();
    assert_eq!(toasts(&workspace, cx), expected);
}

/// A window whose `lanes: adopt` reaches the lanes of `harness`, with lanes on.
async fn adopt_window<'a>(
    harness: &TreeHarness,
    cx: &'a mut TestAppContext,
) -> (gpui::Entity<Workspace>, &'a mut VisualTestContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    let lanes = Lanes::new(harness.lanes.parts.clone());
    cx.update(|_, cx| {
        editor::init(cx);
        set_enabled(cx, true);
        cx.set_global(WindowLanes(Some(Rc::new(lanes))));
    });
    (workspace, cx)
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

/// The choices of the list `lanes: adopt` opened last.
fn choices(cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| cx.global::<LastChoices>().0.clone())
}

fn last_toast(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.try_global::<LastToast>().map(|toast| toast.0.clone()))
}

/// Types `text` into the open prompt and confirms it.
fn answer(text: &str, cx: &mut VisualTestContext) {
    if !text.is_empty() {
        cx.simulate_input(text);
        cx.run_until_parked();
    }
    dispatch("menu::Confirm", cx);
}

/// Waits for the adopt the action started and the toast or prompt it shows.
async fn adopt_done(cx: &mut VisualTestContext) {
    let pending = cx.update(|_, cx| {
        cx.try_global::<PendingAdopt>()
            .is_some()
            .then(|| cx.global_mut::<PendingAdopt>().0.take())
            .flatten()
    });
    pending.expect("an adopt started").await;
    cx.run_until_parked();
}

// `lanes: adopt` asks for a stopped or halted task, then offers only the free lanes, refuses an
// empty reason at its prompt and adopts as the person at the keyboard.
#[gpui::test]
async fn lanes_adopt_asks_task_free_lane_and_reason_then_adopts(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    let (_workspace, cx) = adopt_window(&harness, cx).await;

    dispatch("lanes::Adopt", cx);
    assert_eq!(choices(cx), ["a"]);
    answer("", cx);
    // build-1 is halted and holds a: only build-2 is free.
    assert_eq!(choices(cx), ["build-2"]);
    answer("", cx);

    let before = harness.state();
    answer("", cx);
    assert_eq!(last_toast(cx), None, "an empty reason is not confirmed");
    assert!(cx.update(|_, cx| cx.try_global::<PendingAdopt>().is_none()));
    assert_eq!(harness.state(), before);

    answer("build-1's drive is slow", cx);
    adopt_done(cx).await;
    assert_eq!(
        last_toast(cx).as_deref(),
        Some("a adopted by build-2 from build-1")
    );
    assert_eq!(harness.cancelled(), [failed]);
    let state = harness.state();
    let owner = &state.owners[&TaskId::new("a")];
    assert_eq!(owner.lane, Some(LaneId::new("build", 2)));
    assert!(matches!(owner.by, Author::Person(_)), "{:?}", owner.by);
    let history = harness.history("a");
    let last = history.last().unwrap();
    assert!(
        last.starts_with("adopted by build-2 from build-1: build-1's drive is slow ("),
        "{last}"
    );
}

// With the halted job's tree gone, the adopt shows the refusal and asks to confirm dropping the
// work; keeping it changes nothing, and confirming adopts with the drop recorded.
#[gpui::test]
async fn lanes_adopt_of_an_unreadable_tree_asks_to_confirm_the_drop(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    std::fs::remove_dir_all(harness.lane_tip(1)).unwrap();
    let (_workspace, cx) = adopt_window(&harness, cx).await;
    let before = harness.state();

    let ask_drop = |cx: &mut VisualTestContext| {
        dispatch("lanes::Adopt", cx);
        answer("", cx);
        answer("", cx);
        answer("drive gone", cx);
    };
    ask_drop(cx);
    adopt_done(cx).await;
    assert_eq!(choices(cx), DROP_CHOICES);
    assert_eq!(last_toast(cx), None);
    // The first choice keeps the work: nothing is cancelled or moved.
    answer("", cx);
    assert!(cx.update(|_, cx| cx.global::<PendingAdopt>().0.is_none()));
    assert!(harness.cancelled().is_empty());
    assert_eq!(harness.state(), before);

    ask_drop(cx);
    adopt_done(cx).await;
    assert_eq!(choices(cx), DROP_CHOICES);
    answer("Drop", cx);
    adopt_done(cx).await;
    assert_eq!(
        last_toast(cx).as_deref(),
        Some("a adopted by build-2 from build-1; the unreadable old work was dropped")
    );
    assert_eq!(harness.cancelled(), [failed]);
    let history = harness.history("a");
    let last = history.last().unwrap();
    assert!(
        last.starts_with("adopted by build-2 from build-1: drive gone (")
            && last.ends_with("); unreadable old work dropped"),
        "{last}"
    );
}

#[test]
fn task_run_goes_to_the_group_the_leaf_or_the_project() {
    use crate::actions::{RunTarget, run_target};
    use bongos_work_model::fixtures;
    let tree = fixtures::tree(vec![fixtures::group(
        "phase",
        vec![fixtures::pr_ship("feature")],
    )]);
    let phase = fixtures::id_of(&tree, "phase");
    let feature = fixtures::id_of(&tree, "feature");
    assert_eq!(
        run_target(Some(&tree), Some(&phase)),
        RunTarget::Group(phase.clone())
    );
    assert_eq!(run_target(Some(&tree), Some(&feature)), RunTarget::Leaf);
    assert_eq!(run_target(Some(&tree), None), RunTarget::Project);
    assert_eq!(run_target(Some(&tree), Some("gone")), RunTarget::Project);
    assert_eq!(run_target(None, Some(&phase)), RunTarget::Project);
}
