//! The inbox badge in real windows over a real inbox directory: it follows the unread count, all
//! windows agree, deliveries raise no notification, toast or modal, and at a narrow width it
//! overlaps nothing and stays inside the window.

use super::tests::{item, key_of, outcome_of, real_inbox, set_enabled, wait_for};
use super::*;
use crate::badge::badge_label;
use gpui::{
    AnyWindowHandle, Bounds, Focusable as _, Render, TestAppContext, UpdateGlobal as _,
    VisualTestContext, WindowHandle, px, size,
};
use project::Project;
use tempfile::TempDir;
use ui::{Label, prelude::*};
use workspace::{AppState, ItemHandle, MultiWorkspace, StatusItemView, Workspace};

#[test]
fn the_label_is_the_count_up_to_99_and_then_99_plus() {
    assert_eq!(badge_label(1), "1");
    assert_eq!(badge_label(3), "3");
    assert_eq!(badge_label(99), "99");
    assert_eq!(badge_label(100), "99+");
    assert_eq!(badge_label(12_345), "99+");
}

#[test]
fn archive_without_a_free_name_fails_but_a_vanished_file_is_a_move() {
    let exists = io::Error::from(io::ErrorKind::AlreadyExists);
    let missing = io::Error::from(io::ErrorKind::NotFound);
    assert!(matches!(
        outcome_of_error(Operation::Archive, &exists),
        OpOutcome::Failed(_)
    ));
    assert_eq!(
        outcome_of_error(Operation::MarkRead, &exists),
        OpOutcome::Moved
    );
    for operation in [
        Operation::MarkRead,
        Operation::MarkUnread,
        Operation::Archive,
    ] {
        assert_eq!(outcome_of_error(operation, &missing), OpOutcome::Moved);
    }
}

/// A left status item with a long label that shrinks and clips like the task path item does.
struct LongItem;

impl Render for LongItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .debug_selector(|| "long-item".to_string())
            .child(Label::new(
                "usage-events > backfill > very-long-phase-name > another-long-task-name \
                 [implement the backfill of the usage events table] pr-ship",
            ))
    }
}

impl StatusItemView for LongItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

/// A right status item added after the badge.
struct OtherRightItem;

impl Render for OtherRightItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "other-right-item".to_string())
            .child(Label::new("Rust"))
    }
}

impl StatusItemView for OtherRightItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

struct Env {
    app_state: Arc<AppState>,
    inbox: Entity<InboxState>,
    root: PathBuf,
    _temp: TempDir,
}

/// An enabled inbox over a temporary directory, scanned once.
async fn env(cx: &mut TestAppContext) -> Env {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        InboxSettings::register(cx);
        init_palette(cx);
        register_open(cx);
        app_state
    });
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec!["messages".to_string()]);
            });
        });
    });
    set_enabled(cx, true);
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("inbox");
    let inbox = real_inbox(cx, &root);
    cx.update(|cx| cx.set_global(Inbox(inbox.clone())));
    wait_for(cx, &inbox, "the first scan", |inbox| {
        inbox.snapshot().is_some()
    })
    .await;
    Env {
        app_state,
        inbox,
        root,
        _temp: temp,
    }
}

impl Env {
    fn deliver(&self, summary: &str) {
        maildir::deliver(&self.root, &item(summary)).unwrap();
    }

    async fn wait_unread(&self, cx: &mut TestAppContext, count: usize) {
        wait_for(cx, &self.inbox, &format!("{count} unread"), |inbox| {
            inbox
                .snapshot()
                .is_some_and(|snapshot| snapshot.unread_count() == count)
        })
        .await;
    }
}

struct Win {
    handle: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    badge: Entity<InboxBadge>,
    cx: VisualTestContext,
}

/// A window whose status bar has the badge first among the right items, then another right item.
/// With `long`, a long left item comes first.
async fn window(cx: &mut TestAppContext, env: &Env, long: bool) -> Win {
    let project = Project::test(env.app_state.fs.clone(), [], cx).await;
    let handle: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = handle
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let mut vcx = VisualTestContext::from_window(handle.into(), cx);
    let badge = vcx.update(|_, cx| cx.new(InboxBadge::new));
    workspace.update_in(&mut vcx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
        let long_item = cx.new(|_| LongItem);
        let other = cx.new(|_| OtherRightItem);
        workspace.status_bar().update(cx, |status_bar, cx| {
            if long {
                status_bar.add_left_item(long_item, window, cx);
            }
            status_bar.add_right_item(badge.clone(), window, cx);
            status_bar.add_right_item(other, window, cx);
        });
    });
    vcx.run_until_parked();
    Win {
        handle,
        workspace,
        badge,
        cx: vcx,
    }
}

impl Win {
    fn text(&self) -> Option<String> {
        self.badge.read_with(&self.cx, |badge, cx| badge.text(cx))
    }

    fn notifications(&self) -> usize {
        self.workspace
            .read_with(&self.cx, |workspace, _| workspace.notification_ids().len())
    }

    fn has_modal(&mut self) -> bool {
        self.workspace
            .update_in(&mut self.cx, |workspace, window, cx| {
                workspace.has_active_modal(window, cx)
            })
    }
}

#[gpui::test]
async fn the_badge_is_hidden_at_zero_and_shows_three_after_three_deliveries(
    cx: &mut TestAppContext,
) {
    let env = env(cx).await;
    let mut win = window(cx, &env, false).await;
    assert_eq!(win.text(), None);
    assert!(win.cx.debug_bounds("inbox-badge").is_none());

    for summary in ["one", "two", "three"] {
        env.deliver(summary);
    }
    env.wait_unread(cx, 3).await;
    win.cx.run_until_parked();
    assert_eq!(win.text().as_deref(), Some("3"));
    assert!(win.cx.debug_bounds("inbox-badge").is_some());
}

#[gpui::test]
async fn the_badge_says_99_plus_above_99(cx: &mut TestAppContext) {
    let env = env(cx).await;
    let win = window(cx, &env, false).await;
    for number in 0..100 {
        env.deliver(&format!("item {number}"));
    }
    env.wait_unread(cx, 100).await;
    win.cx.run_until_parked();
    assert_eq!(win.text().as_deref(), Some("99+"));
}

#[gpui::test]
async fn the_badge_hides_when_the_inbox_is_turned_off(cx: &mut TestAppContext) {
    let env = env(cx).await;
    let win = window(cx, &env, false).await;
    env.deliver("one");
    env.wait_unread(cx, 1).await;
    assert_eq!(win.text().as_deref(), Some("1"));
    set_enabled(cx, false);
    wait_for(cx, &env.inbox, "the inbox to go off", |inbox| {
        !inbox.is_enabled()
    })
    .await;
    assert_eq!(win.text(), None);
}

#[gpui::test]
async fn two_windows_show_the_same_count_after_a_read_in_one(cx: &mut TestAppContext) {
    let env = env(cx).await;
    let first = window(cx, &env, false).await;
    let second = window(cx, &env, false).await;
    for summary in ["one", "two", "three"] {
        env.deliver(summary);
    }
    env.wait_unread(cx, 3).await;
    first.cx.run_until_parked();
    second.cx.run_until_parked();
    assert_eq!(first.text().as_deref(), Some("3"));
    assert_eq!(second.text().as_deref(), Some("3"));

    let (unique, inode) = env.inbox.read_with(cx, |inbox, _| key_of(inbox, "two"));
    let task = env
        .inbox
        .update(cx, |inbox, cx| inbox.mark_read(&unique, inode, cx));
    assert!(matches!(outcome_of(cx, task).await, OpOutcome::Done(_)));
    env.wait_unread(cx, 2).await;
    first.cx.run_until_parked();
    second.cx.run_until_parked();
    assert_eq!(first.text().as_deref(), Some("2"));
    assert_eq!(second.text().as_deref(), Some("2"));
}

#[gpui::test]
async fn deliveries_raise_no_notification_toast_or_modal(cx: &mut TestAppContext) {
    let env = env(cx).await;
    let mut win = window(cx, &env, false).await;
    let before = win.notifications();
    assert!(!win.has_modal());
    for summary in ["one", "two", "three"] {
        env.deliver(summary);
    }
    env.wait_unread(cx, 3).await;
    win.cx.run_until_parked();
    assert_eq!(win.notifications(), before);
    assert!(!win.has_modal());
}

#[gpui::test]
async fn open_says_not_yet_in_the_echo_area_and_is_hidden_from_the_palette(
    cx: &mut TestAppContext,
) {
    let env = env(cx).await;
    let mut win = window(cx, &env, false).await;
    cx.update(|cx| {
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx).unwrap();
        assert!(filter.is_hidden(&Open));
    });
    win.cx.dispatch_action(Open);
    win.cx.run_until_parked();
    let window: AnyWindowHandle = win.handle.into();
    let messages = win.cx.update(|_, cx| {
        bongos_minibuffer::minibuffer_for_window(window, cx)
            .expect("the window has a strip")
            .read(cx)
            .messages()
    });
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text.as_ref(), NOT_YET);
    assert_eq!(win.notifications(), 0);
    assert!(!win.has_modal());
}

fn intersects(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    a.intersect(&b).size.width > px(0.) && a.intersect(&b).size.height > px(0.)
}

#[gpui::test]
async fn at_480_pixels_the_badge_is_the_rightmost_item_and_overlaps_nothing(
    cx: &mut TestAppContext,
) {
    let env = env(cx).await;
    let mut win = window(cx, &env, true).await;
    win.cx.simulate_resize(size(px(480.), px(320.)));
    env.deliver("one");
    env.wait_unread(cx, 1).await;
    win.cx.update(|window, cx| {
        bongos_minibuffer::message(window, cx, "an echo line");
    });
    win.cx.run_until_parked();

    let window_bounds = Bounds {
        origin: Default::default(),
        size: size(px(480.), px(320.)),
    };
    let badge = win
        .cx
        .debug_bounds("inbox-badge")
        .expect("the badge is drawn");
    let long = win
        .cx
        .debug_bounds("long-item")
        .expect("the task path is drawn");
    let other = win
        .cx
        .debug_bounds("other-right-item")
        .expect("the item is drawn");
    // Added first among the right items, so the rightmost, and not in the left tools.
    assert!(badge.left() >= other.right(), "{badge:?} vs {other:?}");
    assert!(badge.left() >= long.right(), "{badge:?} vs {long:?}");
    assert!(!intersects(badge, long));
    assert!(!intersects(badge, other));
    assert_eq!(window_bounds.intersect(&badge), badge, "inside the window");
    if let Some(echo) = win.cx.debug_bounds("echo-text") {
        assert!(!intersects(badge, echo), "{badge:?} vs {echo:?}");
    }
}
