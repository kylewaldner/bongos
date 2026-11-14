//! Tests of the native completing read: what a session gets for only a host, a list and a
//! handler, replacing, streaming, previews, history, typing into a large list, and that every
//! handler runs once.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bongos_minibuffer_model::{Cancel, Narrower, count_text, texts};
use db::kvp::KeyValueStore;
use futures::channel::mpsc;
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
    WindowHandle, actions,
};
use picker::PickerDelegate as _;
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::{
    Candidate, HistoryStore, Outcome, Preview, Session, SessionHandle, SessionView, Source, open,
};

const HOST: &str = "test_host";

#[derive(Default)]
struct Log {
    confirmed: Vec<String>,
    cancelled: usize,
    actions: Vec<String>,
    previews: Vec<String>,
    inputs: Vec<String>,
}

type Shared = Rc<RefCell<Log>>;

fn text_of(value: &Arc<dyn std::any::Any + Send + Sync>) -> String {
    value.downcast_ref::<String>().cloned().unwrap_or_default()
}

/// A session over `source` that logs its handlers into `log`.
fn session(log: &Shared, source: Source) -> Session {
    let confirmed = log.clone();
    let cancelled = log.clone();
    Session::completing_read(HOST, "Pick: ")
        .source(source)
        .on_confirm(move |outcome, _, _| {
            confirmed.borrow_mut().confirmed.push(match outcome {
                Outcome::Candidate(value) => text_of(&value),
                Outcome::Input(input) => format!("input:{input}"),
            })
        })
        .on_cancel(move |_, _| cancelled.borrow_mut().cancelled += 1)
}

fn fixed(items: &[&str]) -> Source {
    Source::Fixed(items.iter().map(|item| Candidate::text(*item)).collect())
}

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

fn open_session(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    session: Session,
) -> SessionHandle {
    let handle = workspace.update_in(cx, |workspace, window, cx| {
        open(workspace, session, window, cx)
    });
    cx.run_until_parked();
    handle
}

fn view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx)
    })
}

fn shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    let view = view(workspace, cx).expect("a session is open");
    view.read_with(cx, |view, cx| view.picker.read(cx).delegate.shown_texts())
}

fn count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    let view = view(workspace, cx).expect("a session is open");
    view.read_with(cx, |view, cx| {
        let delegate = &view.picker.read(cx).delegate;
        let selected = delegate.selected_index() + 1;
        Some(count_text(
            selected.min(delegate.match_count()),
            delegate.match_count(),
        ))
    })
}

fn input(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = view(workspace, cx).expect("a session is open");
    view.read_with(cx, |view, cx| view.picker.read(cx).query(cx))
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

#[gpui::test]
async fn minibuffer_session_of_a_host_a_list_and_confirm_gets_the_rest(cx: &mut TestAppContext) {
    let store = HistoryStore::new(KeyValueStore::open_test_db("minibuffer_session_rest").await);
    cx.update(|cx| store.clone().install(cx));
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;

    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha", "beta", "gamma", "alphabet"])),
    );
    assert!(
        in_strip(&workspace, cx),
        "a routed host is drawn in the strip"
    );
    assert_eq!(count(&workspace, cx).as_deref(), Some("1/4"));

    cx.simulate_input("alp");
    let mut matches = shown(&workspace, cx);
    matches.sort();
    assert_eq!(matches, ["alpha", "alphabet"]);
    assert_eq!(count(&workspace, cx).as_deref(), Some("1/2"));

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed.len(), 1);
    assert!(log.borrow().confirmed[0].starts_with("alpha"));
    assert_eq!(log.borrow().cancelled, 0);
    assert!(view(&workspace, cx).is_none(), "the session closed");
    assert_eq!(
        store.ring(HOST).entries(),
        ["alp"],
        "the host name is the history name by default"
    );

    // The next session of the host walks the history.
    open_session(&workspace, cx, session(&log, fixed(&["alpha", "beta"])));
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "alp");

    // And C-g cancels.
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(view(&workspace, cx).is_none());
    assert_eq!(log.borrow().cancelled, 1);
    assert_eq!(log.borrow().confirmed.len(), 1);
}

#[gpui::test]
async fn minibuffer_session_is_centered_when_its_host_is_not_routed(cx: &mut TestAppContext) {
    for routes in [&[][..], &["other"][..]] {
        let (workspace, mut cx) = window(cx, routes).await;
        let log = Shared::default();
        let cx = &mut cx;
        open_session(&workspace, cx, session(&log, fixed(&["one", "two"])));
        assert!(view(&workspace, cx).is_some());
        assert!(!in_strip(&workspace, cx), "an unrouted host is centered");
        cx.simulate_input("tw");
        assert_eq!(shown(&workspace, cx), ["two"]);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(log.borrow().confirmed, ["two"]);
        assert!(view(&workspace, cx).is_none());
    }
}

#[gpui::test]
async fn minibuffer_session_ends_as_cancelled_when_the_strip_goes_away(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    open_session(&workspace, cx, session(&log, fixed(&["one"])));
    // The strip goes away while the session is open: the session never refuses to go.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_bottom_item(None, window, cx)
    });
    cx.run_until_parked();
    assert!(view(&workspace, cx).is_none(), "no session is left undrawn");
    assert_eq!(log.borrow().cancelled, 1);
    assert!(log.borrow().confirmed.is_empty());
}

#[gpui::test]
async fn minibuffer_replacing_a_session_cancels_the_first_once(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let first = Shared::default();
    let second = Shared::default();
    let cx = &mut cx;

    open_session(&workspace, cx, session(&first, fixed(&["one"])));
    let first_view = view(&workspace, cx).unwrap();
    open_session(&workspace, cx, session(&second, fixed(&["two"])));

    assert_eq!(first.borrow().cancelled, 1);
    assert_eq!(second.borrow().cancelled, 0);
    let second_view = view(&workspace, cx).expect("the second session opened");
    assert_ne!(first_view.entity_id(), second_view.entity_id());
    assert_eq!(shown(&workspace, cx), ["two"]);

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(first.borrow().cancelled, 1, "the first never runs twice");
    assert_eq!(second.borrow().cancelled, 1);
    assert!(first.borrow().confirmed.is_empty() && second.borrow().confirmed.is_empty());
}

type Calls = Rc<RefCell<Vec<(String, Cancel, mpsc::Sender<Vec<Candidate>>)>>>;

fn streaming(calls: &Calls) -> Source {
    let calls = calls.clone();
    Source::Dynamic(Box::new(move |query, cancel, _| {
        let (sender, receiver) = mpsc::channel(16);
        calls.borrow_mut().push((query, cancel, sender));
        receiver
    }))
}

fn batch(items: &[&str]) -> Vec<Candidate> {
    items.iter().map(|item| Candidate::text(*item)).collect()
}

#[gpui::test]
async fn minibuffer_stream_drops_a_late_batch_for_an_old_query(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let calls = Calls::default();
    let cx = &mut cx;
    open_session(&workspace, cx, session(&log, streaming(&calls)));

    cx.simulate_input("a");
    cx.simulate_input("b");
    let queries: Vec<String> = calls.borrow().iter().map(|call| call.0.clone()).collect();
    assert_eq!(queries, ["", "a", "ab"]);
    assert!(
        calls.borrow()[..2].iter().all(|call| call.1.is_cancelled()),
        "typing cancels the source of the previous query"
    );
    assert!(!calls.borrow()[2].1.is_cancelled());

    // The batch for "a" arrives after "ab" was typed: it is never shown.
    calls.borrow_mut()[1].2.try_send(batch(&["a-late"])).ok();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), Vec::<String>::new());

    calls.borrow_mut()[2]
        .2
        .try_send(batch(&["ab-1", "ab-2"]))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["ab-1", "ab-2"]);
    calls.borrow_mut()[2].2.try_send(batch(&["ab-3"])).unwrap();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["ab-1", "ab-2", "ab-3"]);

    // Confirming does not wait for the stream to end.
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed, ["ab-1"]);
}

#[gpui::test]
async fn minibuffer_preview_gets_highlight_then_cancelled(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let previews = log.clone();
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["a", "b", "c"])).on_preview(move |event, _, _| {
            previews.borrow_mut().previews.push(match event {
                Preview::Highlight(value) => format!("highlight:{}", text_of(&value)),
                Preview::Confirmed => "confirmed".to_string(),
                Preview::Cancelled => "cancelled".to_string(),
            })
        }),
    );
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(
        log.borrow().previews,
        ["highlight:a", "highlight:b", "cancelled"]
    );
    assert_eq!(log.borrow().cancelled, 1);
}

#[gpui::test]
async fn minibuffer_history_survives_a_new_store_over_the_same_db(cx: &mut TestAppContext) {
    let kvp = KeyValueStore::open_test_db("minibuffer_history_survives").await;
    let first = HistoryStore::new(kvp.clone());
    cx.update(|cx| first.clone().install(cx));
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;

    for (typed, name) in [
        ("be", "other"),
        ("al", "other"),
        ("be", "other"),
        ("ga", HOST),
    ] {
        let source = fixed(&["alpha", "beta", "gamma"]);
        open_session(&workspace, cx, session(&log, source).history(name));
        cx.simulate_input(typed);
        cx.dispatch_action(minibuffer::Confirm);
        cx.run_until_parked();
    }

    let second = HistoryStore::new(kvp);
    assert_eq!(second.ring("other").entries(), ["be", "al"]);
    assert_eq!(second.ring(HOST).entries(), ["ga"]);
    assert!(second.ring("never used").is_empty());
}

#[gpui::test]
async fn minibuffer_typing_into_a_large_list_keeps_every_character(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let items: Vec<String> = (0..100_000).map(|ix| format!("item-{ix:06}")).collect();
    let source = Source::Fixed(
        items
            .iter()
            .map(|item| Candidate::text(item.clone()))
            .collect(),
    );
    open_session(&workspace, cx, session(&log, source));
    assert_eq!(count(&workspace, cx).as_deref(), Some("1/100000"));

    let typed = "it 1 2 3 4 5 6 7 8 9 0 1 2 3 4";
    assert_eq!(typed.chars().count(), 30);
    cx.simulate_input(typed);

    assert_eq!(input(&workspace, cx), typed, "no character was dropped");
    let expected = Narrower::new(texts(&items))
        .narrow(typed, &Cancel::new())
        .expect("not cancelled")
        .len();
    let shown_count = view(&workspace, cx)
        .unwrap()
        .read_with(cx, |view, cx| view.picker.read(cx).delegate.match_count());
    assert_eq!(
        shown_count, expected,
        "the list ends with the engine's count"
    );
}

#[gpui::test]
async fn minibuffer_each_handler_runs_once(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;

    // Two answers before anything is processed: only the first counts.
    open_session(&workspace, cx, session(&log, fixed(&["one", "two"])));
    cx.dispatch_action(minibuffer::Confirm);
    cx.dispatch_action(minibuffer::Confirm);
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed, ["one"]);
    assert_eq!(log.borrow().cancelled, 0);

    // A cancel, then a close and another cancel.
    let log = Shared::default();
    let handle = open_session(&workspace, cx, session(&log, fixed(&["one"])));
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    cx.update(|window, cx| handle.close(window, cx));
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(log.borrow().cancelled, 1);
    assert!(log.borrow().confirmed.is_empty());
}

#[gpui::test]
async fn minibuffer_descend_replaces_the_input_instead_of_confirming(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let source = Source::Fixed(vec![
        Candidate::text("src").descend("src/"),
        Candidate::text("readme"),
    ]);
    open_session(&workspace, cx, session(&log, source));
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "src/");
    assert!(log.borrow().confirmed.is_empty());
    assert!(view(&workspace, cx).is_some());
}

#[gpui::test]
async fn minibuffer_free_input_answers_what_was_typed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;

    // RET with nothing that matches.
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha"])).free_input(true),
    );
    cx.simulate_input("zzz");
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed, ["input:zzz"]);

    // C-j answers the input even when something matches; empty input answers the default.
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha"])).free_input(true),
    );
    cx.simulate_input("al");
    cx.dispatch_action(minibuffer::ConfirmInput);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed[1], "input:al");

    open_session(
        &workspace,
        cx,
        session(&log, fixed(&[]))
            .free_input(true)
            .default("fallback"),
    );
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed[2], "input:fallback");

    // Without free input, C-j and a RET on no match do nothing.
    open_session(&workspace, cx, session(&log, fixed(&["alpha"])));
    cx.simulate_input("zzz");
    cx.dispatch_action(minibuffer::ConfirmInput);
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow().confirmed.len(), 3);
    assert!(view(&workspace, cx).is_some());
}

#[gpui::test]
async fn minibuffer_menu_action_answers_with_the_selected_candidate(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let actions = log.clone();
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha", "beta"])).action(
            "Open in split",
            's',
            move |value, _, _| actions.borrow_mut().actions.push(text_of(&value)),
        ),
    );
    cx.simulate_input("be");
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    cx.simulate_keystrokes("s");
    cx.run_until_parked();
    assert_eq!(log.borrow().actions, ["beta"]);
    assert!(log.borrow().confirmed.is_empty());
    assert_eq!(log.borrow().cancelled, 0);
    assert!(view(&workspace, cx).is_none());
}

#[gpui::test]
async fn minibuffer_on_input_and_handle_drive_the_session(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let inputs = log.clone();
    let handle = open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha", "beta"])).on_input(move |text, handle, window, cx| {
            inputs.borrow_mut().inputs.push(text.to_string());
            handle.set_prompt("Failing: ", window, cx);
            handle.set_status("1/2", cx);
        }),
    );
    cx.simulate_input("al");
    assert_eq!(log.borrow().inputs, ["a", "al"]);
    let view = view(&workspace, cx).unwrap();
    view.read_with(cx, |view, _| {
        assert_eq!(*view.core.status.borrow(), "1/2");
        assert_eq!(*view.core.prompt.borrow(), "Failing: ");
    });

    cx.update(|window, cx| handle.set_input("be", window, cx));
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["beta"]);
}

actions!(test_host, [Ping]);

#[gpui::test]
async fn minibuffer_host_key_context_sits_under_minibuffer(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    let pings = Rc::new(RefCell::new(0));
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new(
            "ctrl-t",
            Ping,
            Some("Minibuffer > TestHost > Editor"),
        )]);
        let pings = pings.clone();
        cx.on_action(move |_: &Ping, _| *pings.borrow_mut() += 1);
    });
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["one"])).key_context("TestHost"),
    );
    cx.simulate_keystrokes("ctrl-t");
    assert_eq!(*pings.borrow(), 1);
}

fn detail_text(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    let view = view(workspace, cx).expect("a session is open");
    view.read_with(cx, |view, cx| {
        view.picker
            .read(cx)
            .delegate
            .detail_text(cx)
            .map(|text| text.to_string())
    })
}

fn detail_drawn(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("minibuffer-detail").is_some()
}

fn detailed(log: &Shared) -> Session {
    session(log, fixed(&["alpha", "beta"]))
        .detail(|value, _| Some(format!("about {}\n\nsecond line", text_of(value)).into()))
}

#[gpui::test]
async fn minibuffer_detail_shows_the_selected_candidate_in_both_presentations(
    cx: &mut TestAppContext,
) {
    for enabled_for in [&[HOST][..], &[][..]] {
        let (workspace, mut cx) = window(cx, enabled_for).await;
        let log = Shared::default();
        let cx = &mut cx;
        open_session(&workspace, cx, detailed(&log));
        assert_eq!(in_strip(&workspace, cx), !enabled_for.is_empty());
        assert_eq!(
            detail_text(&workspace, cx).as_deref(),
            Some("about alpha\n\nsecond line")
        );
        assert!(detail_drawn(cx));
    }
}

#[gpui::test]
async fn minibuffer_detail_follows_the_selection(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    open_session(&workspace, cx, detailed(&log));
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
    assert_eq!(
        detail_text(&workspace, cx).as_deref(),
        Some("about beta\n\nsecond line")
    );
    assert!(detail_drawn(cx));
    cx.simulate_input("al");
    cx.run_until_parked();
    assert_eq!(
        detail_text(&workspace, cx).as_deref(),
        Some("about alpha\n\nsecond line")
    );
}

#[gpui::test]
async fn minibuffer_detail_is_absent_when_the_handler_returns_none(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    open_session(
        &workspace,
        cx,
        session(&log, fixed(&["alpha", "beta"])).detail(|_, _| None),
    );
    assert_eq!(detail_text(&workspace, cx), None);
    assert!(!detail_drawn(cx));
}

#[gpui::test]
async fn minibuffer_detail_is_absent_without_a_handler(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Shared::default();
    let cx = &mut cx;
    open_session(&workspace, cx, session(&log, fixed(&["alpha", "beta"])));
    assert_eq!(detail_text(&workspace, cx), None);
    assert!(!detail_drawn(cx));
}
