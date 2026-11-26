//! The inbox view in a real window over a real inbox directory: `inbox: open` keeps one view per
//! window, the cursor moves without renaming anything, `inbox: open item` and `inbox: mark unread`
//! rename the item and move the badge, the cursor stays on its item across rescans, and
//! `inbox: dump view` writes the count and the rows.

use super::badge_tests::{Env, Win, env, window};
use super::tests::{item, set_enabled, wait_for};
use super::*;
use chrono::{DateTime, Duration, TimeZone as _, Utc};
use gpui::{Focusable as _, TestAppContext};
use rows::RowKey;
use workspace::item::Item as _;
use workspace::item::test::TestItem;

/// The time the tests' clock reads; items are delivered at offsets before it.
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
}

/// Delivers an item from source `agent/<summary>` (so no two group), created `minutes_ago`
/// before [`fixed_now`]. Returns its path in `new/`.
fn deliver(env: &Env, summary: &str, minutes_ago: i64) -> PathBuf {
    let mut item = item(summary);
    item.source = format!("agent/{summary}");
    item.created = fixed_now() - Duration::minutes(minutes_ago);
    maildir::deliver(&env.root, &item).unwrap()
}

/// An enabled inbox with a fixed clock and one window.
async fn setup(cx: &mut TestAppContext) -> (Env, Win) {
    let env = env(cx).await;
    cx.update(|cx| set_clock(fixed_now, cx));
    let win = window(cx, &env, false).await;
    (env, win)
}

fn views(win: &Win) -> Vec<Entity<InboxView>> {
    win.workspace.read_with(&win.cx, |workspace, cx| {
        workspace.items_of_type::<InboxView>(cx).collect()
    })
}

fn is_focused(win: &mut Win, view: &Entity<InboxView>) -> bool {
    win.cx
        .update(|window, cx| view.focus_handle(cx).contains_focused(window, cx))
}

/// Opens the view and returns it.
fn open_view(win: &mut Win) -> Entity<InboxView> {
    win.cx.dispatch_action(Open);
    win.cx.run_until_parked();
    let views = views(win);
    assert_eq!(views.len(), 1);
    views[0].clone()
}

fn summaries(win: &Win, view: &Entity<InboxView>) -> Vec<String> {
    view.read_with(&win.cx, |view, _| {
        view.rows().iter().map(|row| row.summary.clone()).collect()
    })
}

fn cursor_summary(win: &Win, view: &Entity<InboxView>) -> Option<String> {
    view.read_with(&win.cx, |view, _| {
        view.selected().map(|row| row.summary.clone())
    })
}

/// Every file name in `new/` and `cur/`, sorted.
fn names(env: &Env) -> Vec<String> {
    let mut names = Vec::new();
    for dir in [NEW, CUR] {
        for entry in std::fs::read_dir(env.root.join(dir)).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            names.push(format!("{dir}/{name}"));
        }
    }
    names.sort();
    names
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn messages(win: &mut Win) -> Vec<String> {
    let window: gpui::AnyWindowHandle = win.handle.into();
    win.cx.update(|_, cx| {
        bongos_minibuffer::minibuffer_for_window(window, cx)
            .expect("the window has a strip")
            .read(cx)
            .messages()
            .iter()
            .map(|message| message.text.to_string())
            .collect()
    })
}

#[gpui::test]
async fn a_second_open_focuses_the_same_view(cx: &mut TestAppContext) {
    let (_env, mut win) = setup(cx).await;
    let view = open_view(&mut win);
    assert!(is_focused(&mut win, &view));
    let title = view.read_with(&win.cx, |view, cx| view.tab_content_text(0, cx));
    assert_eq!(title.as_ref(), "Inbox");

    // Something else takes the focus, then the inbox is opened again.
    win.workspace
        .update_in(&mut win.cx, |workspace, window, cx| {
            let other = cx.new(TestItem::new);
            workspace.add_item_to_active_pane(Box::new(other), None, true, window, cx);
        });
    win.cx.run_until_parked();
    assert!(!is_focused(&mut win, &view));

    let again = open_view(&mut win);
    assert_eq!(again, view, "one view per window");
    assert!(is_focused(&mut win, &view));
    let active = win.workspace.read_with(&win.cx, |workspace, cx| {
        workspace.active_item_as::<InboxView>(cx)
    });
    assert_eq!(active, Some(view));
}

#[gpui::test]
async fn the_actions_are_hidden_from_the_palette_and_open_says_so_while_off(
    cx: &mut TestAppContext,
) {
    let (env, mut win) = setup(cx).await;
    let hidden = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx).unwrap();
            [
                filter.is_hidden(&Open),
                filter.is_hidden(&Next),
                filter.is_hidden(&Previous),
                filter.is_hidden(&Refresh),
                filter.is_hidden(&OpenItem),
                filter.is_hidden(&MarkUnread),
            ]
        })
    };
    assert_eq!(hidden(cx), [false; 6]);

    set_enabled(cx, false);
    wait_for(cx, &env.inbox, "the inbox to go off", |inbox| {
        !inbox.is_enabled()
    })
    .await;
    assert_eq!(hidden(cx), [true; 6]);
    win.cx.dispatch_action(Open);
    win.cx.run_until_parked();
    assert!(views(&win).is_empty());
    assert_eq!(messages(&mut win), [OFF_MESSAGE]);
    assert_eq!(win.notifications(), 0);
    assert!(!win.has_modal());
}

#[gpui::test]
async fn moving_the_cursor_renames_nothing(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    deliver(&env, "older", 10);
    deliver(&env, "newer", 5);
    env.wait_unread(cx, 2).await;
    let view = open_view(&mut win);
    assert_eq!(summaries(&win, &view), ["newer", "older"]);
    let before = names(&env);

    for action in [true, true, false, true, false, false] {
        if action {
            win.cx.dispatch_action(Next);
        } else {
            win.cx.dispatch_action(Previous);
        }
    }
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("newer"));
    win.cx.dispatch_action(Next);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("older"));

    assert_eq!(names(&env), before);
    env.inbox.read_with(cx, |inbox, _| {
        assert_eq!(inbox.stats().operations, 0);
        assert_eq!(inbox.snapshot().unwrap().unread_count(), 2);
    });
}

#[gpui::test]
async fn open_item_marks_read_and_mark_unread_marks_unread(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    deliver(&env, "older", 10);
    let newer = deliver(&env, "newer", 5);
    env.wait_unread(cx, 2).await;
    win.cx.run_until_parked();
    assert_eq!(win.text().as_deref(), Some("2"));
    let view = open_view(&mut win);
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("newer"));

    win.cx.dispatch_action(OpenItem);
    env.wait_unread(cx, 1).await;
    win.cx.run_until_parked();
    let unique = file_name(&newer);
    let read = env.root.join(CUR).join(format!("{unique}:2,S"));
    assert!(!newer.exists());
    assert!(read.is_file());
    assert_eq!(win.text().as_deref(), Some("1"));
    let (unread, cursor) = view.read_with(&win.cx, |view, _| {
        (view.rows()[0].unread, view.selected().unwrap().key.clone())
    });
    assert!(!unread, "the row turned read");
    assert_eq!(cursor, RowKey::Item(unique.clone()), "the cursor stayed");

    // On a read item, open item changes nothing.
    let operations = env.inbox.read_with(cx, |inbox, _| inbox.stats().operations);
    win.cx.dispatch_action(OpenItem);
    win.cx.run_until_parked();
    assert_eq!(
        env.inbox.read_with(cx, |inbox, _| inbox.stats().operations),
        operations
    );
    assert!(read.is_file());

    win.cx.dispatch_action(MarkUnread);
    env.wait_unread(cx, 2).await;
    win.cx.run_until_parked();
    assert!(!read.exists());
    assert!(env.root.join(CUR).join(format!("{unique}:2,")).is_file());
    assert_eq!(win.text().as_deref(), Some("2"));
    assert!(view.read_with(&win.cx, |view, _| view.rows()[0].unread));
}

#[gpui::test]
async fn the_cursor_survives_a_rescan_and_moves_on_when_its_item_goes(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    deliver(&env, "a", 10);
    let b = deliver(&env, "b", 20);
    deliver(&env, "c", 30);
    env.wait_unread(cx, 3).await;
    let view = open_view(&mut win);
    win.cx.dispatch_action(Next);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("b"));

    // A newer item arrives above: the cursor stays on b.
    deliver(&env, "d", 1);
    env.wait_unread(cx, 4).await;
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["d", "a", "b", "c"]);
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("b"));

    // `inbox: refresh` scans again and the cursor stays.
    let scans = env.inbox.read_with(cx, |inbox, _| inbox.stats().scans);
    win.cx.dispatch_action(Refresh);
    wait_for(cx, &env.inbox, "the refresh scan", |inbox| {
        inbox.stats().scans > scans
    })
    .await;
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("b"));

    // b goes away by hand: the cursor moves to the row after it.
    std::fs::remove_file(&b).unwrap();
    env.wait_unread(cx, 3).await;
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["d", "a", "c"]);
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("c"));
}

#[cfg(debug_assertions)]
#[gpui::test]
async fn dump_view_writes_the_unread_count_and_the_rows(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    let logs = tempfile::TempDir::new().unwrap();
    cx.update(|cx| cx.set_global(DumpDir(logs.path().to_path_buf())));
    let dumped = || std::fs::read_to_string(logs.path().join(DUMP_FILE)).unwrap();
    deliver(&env, "first", 3);
    deliver(&env, "second", 120);
    env.wait_unread(cx, 2).await;
    let task = env.inbox.update(cx, |inbox, cx| {
        let entry = inbox
            .snapshot()
            .unwrap()
            .items()
            .iter()
            .find(|(_, result)| {
                result
                    .as_ref()
                    .as_ref()
                    .is_ok_and(|item| item.summary == "second")
            })
            .map(|(entry, _)| (entry.unique.clone(), entry.inode))
            .unwrap();
        inbox.mark_read(&entry.0, entry.1, cx)
    });
    super::tests::outcome_of(cx, task).await;
    env.wait_unread(cx, 1).await;

    // No view open: the snapshot's rows, no cursor.
    win.cx.dispatch_action(DumpView);
    win.cx.run_until_parked();
    assert!(views(&win).is_empty());
    assert_eq!(
        dumped(),
        "unread: 1\n \
         * first 3m | agent · note · first\n   \
         second 2h | agent · note · second\n"
    );

    // With the view open: its rows and cursor.
    open_view(&mut win);
    win.cx.dispatch_action(Next);
    win.cx.dispatch_action(DumpView);
    win.cx.run_until_parked();
    assert_eq!(
        dumped(),
        "unread: 1\n \
         * first 3m | agent · note · first\n>  \
         second 2h | agent · note · second\n"
    );

    set_enabled(cx, false);
    wait_for(cx, &env.inbox, "the inbox to go off", |inbox| {
        !inbox.is_enabled()
    })
    .await;
    win.cx.dispatch_action(DumpView);
    win.cx.run_until_parked();
    assert_eq!(dumped(), "unread: off\n");
}
