//! Tests of the pending-key hint and the prefix echo: the hint shows the pending keys after
//! `minibuffer.key_hint_delay` and clears when the sequence ends, the continuations list when
//! `which_key.enabled`, the prefix echo shows at once, and the which-key modal stays closed
//! while `key_hint` is routed. Nothing happens while `key_hint` is not routed.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::Project;
use settings::SettingsStore;
use which_key::WhichKeyModal;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::{KEY_HINT, Minibuffer, minibuffer_for_window, set_prefix_echo};

/// The default of `minibuffer.key_hint_delay`.
const DELAY: Duration = Duration::from_millis(500);

fn init(cx: &mut TestAppContext, enabled_for: &[&str], which_key: bool) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        which_key::init(cx);
        crate::init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-x ctrl-f", minibuffer::Focus, None),
            KeyBinding::new("ctrl-x 4 b", minibuffer::Cancel, None),
            KeyBinding::new("ctrl-x 4 f", minibuffer::ShowMessages, None),
        ]);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(enabled_for.iter().map(|name| name.to_string()).collect());
                content.which_key.get_or_insert_default().enabled = Some(which_key);
            });
        });
    });
    app_state
}

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
    which_key: bool,
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx, enabled_for, which_key);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

fn strip(cx: &mut VisualTestContext) -> Entity<Minibuffer> {
    cx.update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip")
}

fn hint(cx: &mut VisualTestContext) -> Option<String> {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| {
        strip.hint_text().map(|text| text.to_string())
    })
}

fn rows(cx: &mut VisualTestContext) -> Vec<(String, String)> {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| {
        strip
            .hint_rows()
            .iter()
            .map(|(keys, action)| (keys.to_string(), action.to_string()))
            .collect()
    })
}

fn wait(cx: &mut VisualTestContext, duration: Duration) {
    cx.executor().advance_clock(duration);
    cx.run_until_parked();
}

fn which_key_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update(cx, |workspace, cx| {
        workspace.active_modal::<WhichKeyModal>(cx).is_some()
    })
}

#[gpui::test]
async fn pending_keys_show_after_the_delay(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    assert_eq!(hint(cx), None);
    wait(cx, DELAY - Duration::from_millis(100));
    assert_eq!(hint(cx), None);
    wait(cx, Duration::from_millis(200));
    assert_eq!(hint(cx).as_deref(), Some("C-x -"));
    assert!(strip(cx).read_with(cx, |strip, _| strip.hint_visible()));
    assert!(rows(cx).is_empty(), "no list while which_key is off");
}

#[gpui::test]
async fn the_hint_grows_with_the_sequence(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY);
    cx.simulate_keystrokes("4");
    assert_eq!(hint(cx).as_deref(), Some("C-x 4 -"));
}

#[gpui::test]
async fn completing_the_sequence_clears_the_hint(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY);
    assert_eq!(hint(cx).as_deref(), Some("C-x -"));
    cx.simulate_keystrokes("ctrl-f");
    assert_eq!(hint(cx), None);
}

#[gpui::test]
async fn cancelling_the_sequence_clears_the_hint(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY);
    assert_eq!(hint(cx).as_deref(), Some("C-x -"));
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(hint(cx), None);
}

#[gpui::test]
async fn a_sequence_that_ends_before_the_delay_never_shows(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x ctrl-g");
    wait(cx, DELAY * 2);
    assert_eq!(hint(cx), None);
}

#[gpui::test]
async fn continuations_list_when_which_key_is_enabled(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], true).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY);
    assert_eq!(hint(cx).as_deref(), Some("C-x -"));
    let rows = rows(cx);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().any(|(_, action)| action == "+2 keybinds"));
    cx.simulate_keystrokes("ctrl-f");
    assert!(self::rows(cx).is_empty());
}

#[gpui::test]
async fn prefix_echo_shows_at_once_and_clears(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &[KEY_HINT], false).await;
    let cx = &mut cx;
    cx.update(|window, cx| set_prefix_echo(window, cx, Some("C-u 4 -".to_string())));
    cx.run_until_parked();
    assert_eq!(hint(cx).as_deref(), Some("C-u 4 -"));
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY);
    assert_eq!(hint(cx).as_deref(), Some("C-u 4 C-x -"));
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(hint(cx).as_deref(), Some("C-u 4 -"));
    cx.update(|window, cx| set_prefix_echo(window, cx, None));
    cx.run_until_parked();
    assert_eq!(hint(cx), None);
}

#[gpui::test]
async fn nothing_shows_while_key_hint_is_not_routed(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx, &["messages"], true).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, DELAY * 2);
    assert_eq!(hint(cx), None);
    cx.update(|window, cx| set_prefix_echo(window, cx, Some("C-u 4 -".to_string())));
    cx.run_until_parked();
    assert_eq!(hint(cx), None);
}

#[gpui::test]
async fn which_key_modal_stays_closed_while_key_hint_is_routed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[KEY_HINT], true).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, Duration::from_millis(2000));
    assert!(!which_key_open(&workspace, cx));
    assert_eq!(hint(cx).as_deref(), Some("C-x -"));
}

#[gpui::test]
async fn which_key_modal_opens_while_key_hint_is_not_routed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["messages"], true).await;
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-x");
    wait(cx, Duration::from_millis(2000));
    assert!(which_key_open(&workspace, cx));
}
