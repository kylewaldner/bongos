//! Tests of the echo area: messages fade and errors wait for a keystroke, neither takes focus, the
//! log keeps 100, toasts without a click action land in the strip while `messages` is routed, and
//! a prompt in the strip hides the echo area.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::SettingsStore;
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Toast, Workspace};
use zed_actions::minibuffer;

use crate::{
    Candidate, LOG_CAPACITY, Minibuffer, Session, SessionView, Source, error, message,
    minibuffer_for_window, open,
};

const HOST: &str = "test_host";

struct ToastMarker;

fn init(cx: &mut TestAppContext, enabled_for: &[&str]) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(enabled_for.iter().map(|name| name.to_string()).collect());
            });
        });
    });
    app_state
}

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx, enabled_for);
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

fn say(cx: &mut VisualTestContext, text: &'static str) {
    cx.update(|window, cx| message(window, cx, text));
    cx.run_until_parked();
}

fn fail(cx: &mut VisualTestContext, text: &'static str) {
    cx.update(|window, cx| error(window, cx, text));
    cx.run_until_parked();
}

/// The text of the echo area's line, wherever it is on show.
fn line(cx: &mut VisualTestContext) -> Option<String> {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| {
        strip.echo_line().map(|line| line.text.to_string())
    })
}

fn visible(cx: &mut VisualTestContext) -> bool {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| strip.echo_visible())
}

fn texts(cx: &mut VisualTestContext) -> Vec<String> {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    })
}

fn open_session(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, session: Session) {
    workspace.update_in(cx, |workspace, window, cx| {
        open(workspace, session, window, cx)
    });
    cx.run_until_parked();
}

fn session_view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<SessionView> {
    workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx)
        })
        .expect("a session is open")
}

fn show_toast(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, toast: Toast) {
    workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
    cx.run_until_parked();
}

fn toast(text: &'static str) -> Toast {
    Toast::new(NotificationId::unique::<ToastMarker>(), text)
}

fn notifications(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.notification_ids().len())
}

#[gpui::test]
async fn minibuffer_message_fades_after_the_timeout_and_never_moves_focus(cx: &mut TestAppContext) {
    let (_, mut cx) = window(cx, &["messages"]).await;
    let before = cx.update(|window, cx| window.focused(cx));
    assert!(before.is_some());

    say(&mut cx, "saved");
    assert_eq!(line(&mut cx).as_deref(), Some("saved"));
    assert!(visible(&mut cx));
    assert!(cx.debug_bounds("echo-text").is_some(), "the strip draws it");
    assert_eq!(cx.update(|window, cx| window.focused(cx)), before);

    cx.executor().advance_clock(Duration::from_millis(3900));
    cx.run_until_parked();
    assert_eq!(line(&mut cx).as_deref(), Some("saved"));

    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(line(&mut cx), None);
    assert!(cx.debug_bounds("echo-text").is_none());
    assert_eq!(cx.update(|window, cx| window.focused(cx)), before);
    assert_eq!(texts(&mut cx), ["saved"], "the log keeps it");
}

#[gpui::test]
async fn minibuffer_a_newer_message_restarts_the_fade(cx: &mut TestAppContext) {
    let (_, mut cx) = window(cx, &["messages"]).await;
    say(&mut cx, "first");
    cx.executor().advance_clock(Duration::from_millis(3000));
    say(&mut cx, "second");
    cx.executor().advance_clock(Duration::from_millis(3000));
    cx.run_until_parked();
    assert_eq!(line(&mut cx).as_deref(), Some("second"));
    cx.executor().advance_clock(Duration::from_millis(1100));
    cx.run_until_parked();
    assert_eq!(line(&mut cx), None);
}

#[gpui::test]
async fn minibuffer_error_stays_until_a_keystroke(cx: &mut TestAppContext) {
    let (_, mut cx) = window(cx, &["messages"]).await;
    fail(&mut cx, "boom");
    assert_eq!(line(&mut cx).as_deref(), Some("boom"));

    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(
        line(&mut cx).as_deref(),
        Some("boom"),
        "no timeout for an error"
    );

    cx.simulate_keystrokes("f9");
    cx.run_until_parked();
    assert_eq!(line(&mut cx), None);
    assert_eq!(texts(&mut cx), ["boom"]);
}

#[gpui::test]
async fn minibuffer_the_101st_message_drops_the_oldest(cx: &mut TestAppContext) {
    let (_, mut cx) = window(cx, &["messages"]).await;
    for index in 0..=LOG_CAPACITY {
        let text: &'static str = Box::leak(format!("m{index}").into_boxed_str());
        cx.update(|window, cx| message(window, cx, text));
    }
    cx.run_until_parked();
    let log = texts(&mut cx);
    assert_eq!(log.len(), LOG_CAPACITY);
    assert_eq!(
        log.first().map(String::as_str),
        Some("m100"),
        "newest first"
    );
    assert_eq!(log.last().map(String::as_str), Some("m1"));
    assert!(!log.contains(&"m0".to_string()));
}

#[gpui::test]
async fn minibuffer_toast_without_an_action_shows_in_the_strip(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["messages"]).await;
    show_toast(&workspace, &mut cx, toast("Copied"));
    assert_eq!(notifications(&workspace, &mut cx), 0);
    assert_eq!(line(&mut cx).as_deref(), Some("Copied"));
    assert!(visible(&mut cx));

    cx.executor().advance_clock(Duration::from_millis(4100));
    cx.run_until_parked();
    assert_eq!(line(&mut cx), None);
}

#[gpui::test]
async fn minibuffer_toast_with_an_action_stays_a_notification(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["messages"]).await;
    show_toast(
        &workspace,
        &mut cx,
        toast("Open it").on_click("Open", |_, _| {}),
    );
    assert_eq!(notifications(&workspace, &mut cx), 1);
    assert_eq!(line(&mut cx), None);
}

#[gpui::test]
async fn minibuffer_unrouted_messages_leave_toasts_and_the_echo_area_alone(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &["command_palette"]).await;
    show_toast(&workspace, &mut cx, toast("Copied"));
    assert_eq!(notifications(&workspace, &mut cx), 1);
    assert_eq!(line(&mut cx), None);

    say(&mut cx, "hello");
    fail(&mut cx, "boom");
    assert_eq!(line(&mut cx), None);
    assert!(texts(&mut cx).is_empty());
}

#[gpui::test]
async fn minibuffer_picker_with_no_actions_says_so_in_the_echo_area(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST, "messages"]).await;
    open_session(
        &workspace,
        &mut cx,
        Session::completing_read(HOST, "Pick: ")
            .source(Source::Fixed(vec![Candidate::text("alpha")])),
    );
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    assert_eq!(texts(&mut cx), ["no actions"]);
    assert!(!visible(&mut cx), "the session hides the echo area");

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(visible(&mut cx), "it shows once the session closes");
    assert_eq!(line(&mut cx).as_deref(), Some("no actions"));
}

#[gpui::test]
async fn minibuffer_a_message_during_a_session_waits_until_it_closes(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST, "messages"]).await;
    open_session(
        &workspace,
        &mut cx,
        Session::completing_read(HOST, "Pick: ")
            .source(Source::Fixed(vec![Candidate::text("alpha")])),
    );
    say(&mut cx, "later");
    assert!(!visible(&mut cx));

    // Its time does not run while the session hides it.
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert_eq!(line(&mut cx).as_deref(), Some("later"));

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(visible(&mut cx));
    cx.executor().advance_clock(Duration::from_millis(4100));
    cx.run_until_parked();
    assert_eq!(line(&mut cx), None);
}

#[gpui::test]
async fn minibuffer_show_messages_lists_the_log_newest_first(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["messages"]).await;
    say(&mut cx, "one");
    fail(&mut cx, "two");
    say(&mut cx, "three");

    cx.dispatch_action(minibuffer::ShowMessages);
    cx.run_until_parked();
    let view = session_view(&workspace, &mut cx);
    let shown = view.read_with(&cx, |view, cx| view.picker.read(cx).delegate.shown_texts());
    assert_eq!(shown, ["three", "two", "one"]);
    assert!(workspace.read_with(&cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    }));
}

#[gpui::test]
async fn minibuffer_toasts_are_notifications_while_nothing_is_routed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[]).await;
    show_toast(&workspace, &mut cx, toast("Copied"));
    assert_eq!(notifications(&workspace, &mut cx), 1);
}
