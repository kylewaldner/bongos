//! `inbox: dismiss` and groups in a real window over a real inbox directory: `d` archives an item
//! with its flags, bytes and inode kept, `d` on a group archives the members the list showed at
//! the keypress and says how many, RET on a group row expands and collapses it, and nothing is
//! ever deleted.

use super::badge_tests::{Env, Win};
use super::tests::{item, wait_for};
use super::view_tests::{
    cursor_summary, fixed_now, messages, open_view, setup, summaries, unread_flags,
};
use super::*;
use bongos_inbox_model::format;
use chrono::Duration;
use gpui::{Action as _, TestAppContext};
use rows::{Pill, RowKey};

/// An item of group `group` (none when `None`) from `agent/<summary>`, created `minutes_ago`
/// before the tests' clock.
fn grouped(summary: &str, group: Option<&str>, minutes_ago: i64) -> Item {
    let mut item = item(summary);
    item.source = format!("agent/{summary}");
    item.group = group.map(str::to_string);
    item.created = fixed_now() - Duration::minutes(minutes_ago);
    item
}

/// Puts `item` in the inbox under its final name at once: delivered into `new/`, or, with
/// `name`, staged beside the inbox and renamed into `cur/<name>`, as a process that marked it
/// itself would.
fn put(env: &Env, item: &Item, cur_name: Option<&str>) -> PathBuf {
    match cur_name {
        None => maildir::deliver(&env.root, item).unwrap(),
        Some(name) => {
            let staging = env.root.parent().unwrap().join("staging");
            std::fs::create_dir_all(&staging).unwrap();
            let staged = staging.join(name);
            std::fs::write(&staged, format::render(item)).unwrap();
            let path = env.root.join(CUR).join(name);
            std::fs::rename(&staged, &path).unwrap();
            path
        }
    }
}

/// The file names in `dir` of the inbox, sorted.
fn names_in(env: &Env, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(env.root.join(dir))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// How many files `new/`, `cur/` and `archive/` hold together.
fn file_count(env: &Env) -> usize {
    [NEW, CUR, ARCHIVE]
        .iter()
        .map(|dir| names_in(env, dir).len())
        .sum()
}

/// Waits until the snapshot holds `count` items.
async fn wait_items(cx: &mut TestAppContext, env: &Env, count: usize) {
    wait_for(cx, &env.inbox, &format!("{count} items"), |inbox| {
        inbox
            .snapshot()
            .is_some_and(|snapshot| snapshot.len() == count)
    })
    .await;
}

/// Waits until the view has no archive outstanding and `archive/` holds `archived` files,
/// looking again whenever the view or the inbox changes, and fails after the hang guard.
async fn wait_settled(win: &mut Win, view: &Entity<InboxView>, archived: usize, env: &Env) {
    let (sender, mut receiver) = futures::channel::mpsc::unbounded::<()>();
    let _subscriptions = win.cx.update(|_, cx| {
        let on_inbox = sender.clone();
        [
            cx.observe(view, move |_, _| {
                sender.unbounded_send(()).ok();
            }),
            cx.observe(&env.inbox, move |_, _| {
                on_inbox.unbounded_send(()).ok();
            }),
        ]
    });
    let mut guard = win.cx.executor().timer(super::tests::HANG_GUARD).fuse();
    loop {
        let done = names_in(env, ARCHIVE).len() == archived
            && view.read_with(&win.cx, |view, _| !view.is_dismissing());
        if done {
            win.cx.run_until_parked();
            return;
        }
        futures::select_biased! {
            _ = receiver.next() => {}
            _ = guard => panic!("timed out waiting for {archived} archived"),
        }
    }
}

fn row_keys(win: &Win, view: &Entity<InboxView>) -> Vec<RowKey> {
    view.read_with(&win.cx, |view, _| {
        view.rows().iter().map(|row| row.key.clone()).collect()
    })
}

#[gpui::test]
async fn d_moves_the_item_to_archive_with_flags_bytes_and_inode_kept(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    let name = "1800000000.flagged.host:2,FS";
    let path = put(&env, &grouped("flagged", None, 5), Some(name));
    put(&env, &grouped("other", None, 10), None);
    wait_items(cx, &env, 2).await;
    env.wait_unread(cx, 1).await;
    let bytes = std::fs::read(&path).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let count = file_count(&env);
    let view = open_view(&mut win);
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("flagged"));

    win.cx.dispatch_action(Dismiss);
    wait_items(cx, &env, 1).await;
    wait_settled(&mut win, &view, 1, &env).await;

    let archived = env.root.join(ARCHIVE).join(name);
    assert_eq!(names_in(&env, ARCHIVE), [name], "flags kept");
    assert_eq!(std::fs::read(&archived).unwrap(), bytes, "bytes unchanged");
    assert_eq!(std::fs::metadata(&archived).unwrap().ino(), inode);
    assert!(!path.exists());
    assert_eq!(file_count(&env), count, "nothing deleted");
    assert_eq!(summaries(&win, &view), ["other"]);
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("other"));
    // A single item says nothing; the badge is unchanged (the item was read).
    assert!(messages(&mut win).is_empty());
    assert_eq!(win.text().as_deref(), Some("1"));
}

#[gpui::test]
async fn d_on_a_group_of_nine_archives_nine_and_the_badge_drops_by_their_unread(
    cx: &mut TestAppContext,
) {
    let (env, mut win) = setup(cx).await;
    // Nine members, four of them read; the group's newest member is the newest item.
    for index in 0..9 {
        let item = grouped(&format!("member {index}"), Some("batch"), 2 + index);
        if index % 2 == 1 {
            put(&env, &item, Some(&format!("18000001{index:02}.m.host:2,S")));
        } else {
            put(&env, &item, None);
        }
    }
    put(&env, &grouped("alone", None, 30), None);
    wait_items(cx, &env, 10).await;
    env.wait_unread(cx, 6).await;
    win.cx.run_until_parked();
    assert_eq!(win.text().as_deref(), Some("6"));
    let count = file_count(&env);
    let view = open_view(&mut win);
    assert_eq!(
        row_keys(&win, &view)[0],
        RowKey::Group("batch".into()),
        "the group row is first"
    );

    win.cx.dispatch_action(Dismiss);
    wait_items(cx, &env, 1).await;
    wait_settled(&mut win, &view, 9, &env).await;

    assert_eq!(names_in(&env, ARCHIVE).len(), 9);
    assert_eq!(
        names_in(&env, ARCHIVE)
            .iter()
            .filter(|name| name.ends_with(":2,S"))
            .count(),
        4,
        "read members stay read in the archive"
    );
    assert!(names_in(&env, CUR).is_empty());
    assert_eq!(names_in(&env, NEW).len(), 1);
    assert_eq!(file_count(&env), count, "nothing deleted");
    env.wait_unread(cx, 1).await;
    win.cx.run_until_parked();
    assert_eq!(
        win.text().as_deref(),
        Some("1"),
        "the badge dropped by five"
    );
    assert_eq!(summaries(&win, &view), ["alone"]);
    assert_eq!(messages(&mut win), ["9 archived"]);
    env.inbox.read_with(cx, |inbox, _| {
        assert_eq!(inbox.stats().operations, 9, "one operation per member");
    });
}

#[gpui::test]
async fn a_group_member_delivered_during_the_dismiss_is_not_archived(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    for index in 0..3 {
        put(
            &env,
            &grouped(&format!("member {index}"), Some("batch"), 5 + index),
            None,
        );
    }
    wait_items(cx, &env, 3).await;
    let view = open_view(&mut win);
    assert_eq!(row_keys(&win, &view), [RowKey::Group("batch".into())]);
    let count = file_count(&env);

    // The key is handled, but no operation has run yet when the new member arrives.
    win.cx
        .update(|window, cx| window.dispatch_action(Dismiss.boxed_clone(), cx));
    let late = put(&env, &grouped("late member", Some("batch"), 1), None);
    wait_items(cx, &env, 1).await;
    wait_settled(&mut win, &view, 3, &env).await;

    assert_eq!(names_in(&env, ARCHIVE).len(), 3);
    assert!(late.is_file(), "the late member stays in new/");
    assert_eq!(file_count(&env), count + 1, "nothing deleted");
    assert_eq!(summaries(&win, &view), ["late member"]);
    assert_eq!(unread_flags(&win, &view), [true]);
    assert_eq!(messages(&mut win), ["3 archived"]);
    env.wait_unread(cx, 1).await;
}

#[gpui::test]
async fn a_stale_row_shows_stale_and_dismisses(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    let mut stale = grouped("points at a moved file", None, 5);
    stale.target = Some(
        env.root
            .parent()
            .unwrap()
            .join("moved.rs")
            .display()
            .to_string(),
    );
    put(&env, &stale, None);
    put(&env, &grouped("other", None, 10), None);
    wait_items(cx, &env, 2).await;
    let count = file_count(&env);
    let view = open_view(&mut win);
    let (pill, text) = view.read_with(&win.cx, |view, cx| {
        (view.rows()[0].pill, view.dump_text(cx))
    });
    assert_eq!(pill, Some(Pill::Stale));
    assert!(
        text.contains(">* points at a moved file [stale] 5m"),
        "{text}"
    );

    win.cx.dispatch_action(Dismiss);
    wait_items(cx, &env, 1).await;
    wait_settled(&mut win, &view, 1, &env).await;
    assert_eq!(names_in(&env, ARCHIVE).len(), 1);
    assert_eq!(summaries(&win, &view), ["other"]);
    assert_eq!(file_count(&env), count, "nothing deleted");
}

#[gpui::test]
async fn ret_on_a_group_row_expands_and_collapses_it(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    put(&env, &grouped("newest", Some("batch"), 1), None);
    put(&env, &grouped("older", Some("batch"), 3), None);
    put(&env, &grouped("alone", None, 10), None);
    wait_items(cx, &env, 3).await;
    let view = open_view(&mut win);
    let group = RowKey::Group("batch".into());
    let keys = row_keys(&win, &view);
    assert_eq!(keys[0], group);
    assert!(matches!(keys[1], RowKey::Item(_)), "{keys:?}");

    win.cx.dispatch_action(OpenItem);
    win.cx.run_until_parked();
    assert_eq!(
        summaries(&win, &view),
        ["newest", "newest", "older", "alone"]
    );
    let members = view.read_with(&win.cx, |view, _| {
        view.rows().iter().map(|row| row.member).collect::<Vec<_>>()
    });
    assert_eq!(members, [false, true, true, false]);
    assert!(view.read_with(&win.cx, |view, _| view.is_expanded("batch")));
    assert_eq!(row_keys(&win, &view)[0], group, "the cursor stays");
    assert_eq!(view.read_with(&win.cx, |view, _| view.cursor()), Some(0));
    // Expanding reads nothing.
    assert_eq!(unread_flags(&win, &view), [true, true, true, true]);
    env.inbox.read_with(cx, |inbox, _| {
        assert_eq!(inbox.stats().operations, 0);
    });

    // A member row is an item row: `d` archives that member only.
    win.cx.dispatch_action(Next);
    win.cx.dispatch_action(Next);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("older"));
    win.cx.dispatch_action(Dismiss);
    wait_items(cx, &env, 2).await;
    wait_settled(&mut win, &view, 1, &env).await;
    assert_eq!(summaries(&win, &view), ["newest", "alone"]);

    // Two items again make a group, which comes back collapsed.
    put(&env, &grouped("second", Some("batch"), 2), None);
    wait_items(cx, &env, 3).await;
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["newest", "alone"]);
    win.cx.dispatch_action(Previous);
    win.cx.dispatch_action(Previous);
    win.cx.dispatch_action(OpenItem);
    win.cx.run_until_parked();
    assert_eq!(
        summaries(&win, &view),
        ["newest", "newest", "second", "alone"]
    );
    win.cx.dispatch_action(OpenItem);
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["newest", "alone"], "collapsed");
}

#[gpui::test]
async fn n_and_p_in_the_modal_pass_over_group_rows(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    put(&env, &grouped("first", None, 1), None);
    put(&env, &grouped("grouped a", Some("batch"), 2), None);
    put(&env, &grouped("grouped b", Some("batch"), 3), None);
    put(&env, &grouped("last", None, 4), None);
    wait_items(cx, &env, 4).await;
    let view = open_view(&mut win);
    view.update(&mut win.cx, |view, cx| view.set_layout(Layout::Sidebar, cx));
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["first", "grouped a", "last"]);

    win.cx.dispatch_action(OpenItem);
    win.cx.run_until_parked();
    assert!(win.has_modal());
    win.cx.dispatch_action(Next);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("last"));
    // At the end it stays.
    win.cx.dispatch_action(Next);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("last"));
    win.cx.dispatch_action(Previous);
    win.cx.run_until_parked();
    assert_eq!(cursor_summary(&win, &view).as_deref(), Some("first"));
    assert!(win.has_modal());
    env.wait_unread(cx, 2).await;
}
