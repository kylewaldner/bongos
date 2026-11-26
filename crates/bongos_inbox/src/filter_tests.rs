//! `inbox: filter` in a real window over a real inbox directory: the `Filter: ` session offers
//! `all` and the kinds and sources of the snapshot, choosing one leaves only its rows and puts it
//! in the tab title, `all` clears it, cancelling changes nothing, and the session is drawn in the
//! minibuffer strip when `inbox` is routed there and centered otherwise.

use super::badge_tests::Win;
use super::tests::{item, wait_for};
use super::view_tests::{fixed_now, open_view, setup, summaries};
use super::*;
use bongos_minibuffer::SessionView;
use chrono::Duration;
use gpui::{BorrowAppContext as _, TestAppContext};
use zed_actions::minibuffer;

/// Delivers an item of `kind` from `source`, created `minutes_ago` before the tests' clock.
fn deliver_as(env: &badge_tests::Env, summary: &str, kind: &str, source: &str, minutes_ago: i64) {
    let mut item = item(summary);
    item.kind = kind.to_string();
    item.source = source.to_string();
    item.created = fixed_now() - Duration::minutes(minutes_ago);
    maildir::deliver(&env.root, &item).unwrap();
}

/// Three items: two decisions (from `agent/a` and `agent/b`) and a note (from `agent/a`).
async fn deliver_three(cx: &mut TestAppContext, env: &badge_tests::Env) {
    deliver_as(env, "first", "decision-default", "agent/a", 1);
    deliver_as(env, "second", "note", "agent/a", 2);
    deliver_as(env, "third", "decision-default", "agent/b", 3);
    wait_for(cx, &env.inbox, "three items", |inbox| {
        inbox.snapshot().is_some_and(|snapshot| snapshot.len() == 3)
    })
    .await;
}

fn route(win: &mut Win, routes: &[&str]) {
    win.cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(routes.iter().map(|name| name.to_string()).collect());
            });
        });
    });
}

fn session_open(win: &mut Win) -> bool {
    win.workspace.read_with(&win.cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn in_strip(win: &mut Win) -> bool {
    win.workspace.read_with(&win.cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

fn title(win: &Win, view: &Entity<InboxView>) -> String {
    view.read_with(&win.cx, |view, cx| view.tab_content_text(0, cx).to_string())
}

/// Opens `Filter: ` and types `text`.
fn ask(win: &mut Win, text: &str) {
    win.cx.dispatch_action(Filter);
    win.cx.run_until_parked();
    assert!(session_open(win), "the Filter: session is open");
    win.cx.simulate_input(text);
}

fn confirm(win: &mut Win) {
    win.cx.dispatch_action(minibuffer::Confirm);
    win.cx.run_until_parked();
    assert!(!session_open(win), "the session ended");
}

#[gpui::test]
async fn the_candidates_are_all_and_the_kinds_and_sources_of_the_snapshot(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    deliver_three(cx, &env).await;
    let view = open_view(&mut win);
    let labels: Vec<String> = view.read_with(&win.cx, |view, cx| {
        view.filter_candidates(cx)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    });
    assert_eq!(
        labels,
        [
            "all",
            "kind: decision-default",
            "kind: note",
            "source: agent/a",
            "source: agent/b",
        ]
    );
}

async fn choosing_a_kind_leaves_only_its_rows(routed: bool, cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    route(&mut win, if routed { &["inbox"] } else { &[] });
    deliver_three(cx, &env).await;
    let view = open_view(&mut win);
    assert_eq!(summaries(&win, &view), ["first", "second", "third"]);
    assert_eq!(title(&win, &view), "Inbox");

    ask(&mut win, "kind: decision-default");
    assert_eq!(
        in_strip(&mut win),
        routed,
        "routed to the strip or centered"
    );
    confirm(&mut win);
    assert_eq!(summaries(&win, &view), ["first", "third"]);
    assert_eq!(title(&win, &view), "Inbox: decision-default");

    // A source filter shows the source.
    ask(&mut win, "source: agent/b");
    confirm(&mut win);
    assert_eq!(summaries(&win, &view), ["third"]);
    assert_eq!(title(&win, &view), "Inbox: agent/b");
}

#[gpui::test]
async fn choosing_a_kind_leaves_only_its_rows_in_the_strip(cx: &mut TestAppContext) {
    choosing_a_kind_leaves_only_its_rows(true, cx).await;
}

#[gpui::test]
async fn choosing_a_kind_leaves_only_its_rows_centered(cx: &mut TestAppContext) {
    choosing_a_kind_leaves_only_its_rows(false, cx).await;
}

#[gpui::test]
async fn cancelling_leaves_the_filter_unchanged(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    route(&mut win, &["inbox"]);
    deliver_three(cx, &env).await;
    let view = open_view(&mut win);

    // No filter before: still none.
    ask(&mut win, "kind: note");
    win.cx.dispatch_action(minibuffer::Cancel);
    win.cx.run_until_parked();
    assert!(!session_open(&mut win));
    assert_eq!(summaries(&win, &view), ["first", "second", "third"]);
    assert_eq!(title(&win, &view), "Inbox");

    // A filter before: still that one.
    ask(&mut win, "kind: decision-default");
    confirm(&mut win);
    ask(&mut win, "kind: note");
    win.cx.dispatch_action(minibuffer::Cancel);
    win.cx.run_until_parked();
    assert_eq!(summaries(&win, &view), ["first", "third"]);
    assert_eq!(title(&win, &view), "Inbox: decision-default");
}

#[gpui::test]
async fn all_clears_the_filter(cx: &mut TestAppContext) {
    let (env, mut win) = setup(cx).await;
    route(&mut win, &["inbox"]);
    deliver_three(cx, &env).await;
    let view = open_view(&mut win);

    ask(&mut win, "kind: note");
    confirm(&mut win);
    assert_eq!(summaries(&win, &view), ["second"]);

    ask(&mut win, "all");
    confirm(&mut win);
    assert_eq!(summaries(&win, &view), ["first", "second", "third"]);
    assert_eq!(title(&win, &view), "Inbox");
}
