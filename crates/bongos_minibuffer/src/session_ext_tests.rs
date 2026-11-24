//! Tests of what a host can do with a session beyond the basics: highlighted and dimmed rows,
//! asking the source again, reading the selection, replacing a fixed list, and hearing about the
//! history entry M-p puts in the input.

use std::cell::RefCell;
use std::rc::Rc;

use bongos_minibuffer_model::Cancel;
use db::kvp::KeyValueStore;
use futures::channel::mpsc;
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use picker::PickerDelegate as _;
use project::Project;
use settings::SettingsStore;
use ui::Color;
use workspace::{MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::{Candidate, HistoryStore, Session, SessionHandle, SessionView, Source, Value, open};

const HOST: &str = "test_host";

fn text_of(value: &Value) -> String {
    value.downcast_ref::<String>().cloned().unwrap_or_default()
}

fn fixed(items: &[&str]) -> Source {
    Source::Fixed(items.iter().map(|item| Candidate::text(*item)).collect())
}

fn session(source: Source) -> Session {
    Session::completing_read(HOST, "Pick: ").source(source)
}

async fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec![HOST.to_string()]);
            });
        });
    });
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

fn view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<SessionView> {
    workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx)
        })
        .expect("a session is open")
}

fn shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    view(workspace, cx).read_with(cx, |view, cx| view.picker.read(cx).delegate.shown_texts())
}

fn input(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    view(workspace, cx).read_with(cx, |view, cx| view.picker.read(cx).query(cx))
}

fn selected_index(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    view(workspace, cx).read_with(cx, |view, cx| {
        view.picker.read(cx).delegate.selected_index()
    })
}

fn row_style(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    ix: usize,
) -> (Vec<usize>, Color) {
    view(workspace, cx)
        .read_with(cx, |view, cx| view.picker.read(cx).delegate.row_style(ix))
        .expect("the row is shown")
}

#[gpui::test]
async fn minibuffer_ext_highlighted_ranges_render_as_highlights(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;
    let source = Source::Fixed(vec![
        Candidate::text("alpha").highlights(vec![0..2, 4..5]),
        Candidate::text("beta"),
        // "é" is two bytes: a range that splits it, or runs past the text, is not drawn.
        Candidate::text("éa").highlights(vec![0..1, 2..3, 3..9]),
    ]);
    open_session(&workspace, cx, session(source));
    assert_eq!(
        row_style(&workspace, cx, 0),
        (vec![0, 1, 4], Color::Default)
    );
    assert_eq!(row_style(&workspace, cx, 1), (vec![], Color::Default));
    assert_eq!(row_style(&workspace, cx, 2), (vec![2], Color::Default));
    // The rows are drawn with those highlights without complaint.
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["alpha", "beta", "éa"]);
}

#[gpui::test]
async fn minibuffer_ext_dimmed_row_is_muted(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;
    let source = Source::Fixed(vec![
        Candidate::text("open"),
        Candidate::text("closed").dimmed().highlights(vec![0..1]),
    ]);
    open_session(&workspace, cx, session(source));
    assert_eq!(row_style(&workspace, cx, 0), (vec![], Color::Default));
    assert_eq!(row_style(&workspace, cx, 1), (vec![0], Color::Muted));
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
async fn minibuffer_ext_refresh_asks_again_and_drops_the_old_pass(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;
    let calls = Calls::default();
    let inputs = Rc::new(RefCell::new(Vec::new()));
    let seen = inputs.clone();
    let handle = open_session(
        &workspace,
        cx,
        session(streaming(&calls))
            .on_input(move |text, _, _, _| seen.borrow_mut().push(text.to_string())),
    );
    cx.simulate_input("a");
    calls.borrow_mut()[1].2.try_send(batch(&["a-1"])).unwrap();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["a-1"]);

    cx.update(|window, cx| handle.refresh(window, cx));
    cx.run_until_parked();
    let queries: Vec<String> = calls.borrow().iter().map(|call| call.0.clone()).collect();
    assert_eq!(
        queries,
        ["", "a", "a"],
        "the source is asked again with the same input"
    );
    assert!(calls.borrow()[1].1.is_cancelled());
    assert!(!calls.borrow()[2].1.is_cancelled());
    assert_eq!(shown(&workspace, cx), Vec::<String>::new());
    assert_eq!(*inputs.borrow(), ["a"], "the input did not change");

    // The previous pass sends a batch late: it is dropped.
    calls.borrow_mut()[1].2.try_send(batch(&["a-late"])).ok();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), Vec::<String>::new());

    calls.borrow_mut()[2].2.try_send(batch(&["a-new"])).unwrap();
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["a-new"]);
}

#[gpui::test]
async fn minibuffer_ext_selected_returns_the_highlighted_value(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;
    let handle = open_session(&workspace, cx, session(fixed(&["a", "b", "c"])));
    let selected = cx.update(|_, cx| handle.selected(cx).map(|value| text_of(&value)));
    assert_eq!(selected.as_deref(), Some("a"));

    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
    let selected = cx.update(|_, cx| handle.selected(cx).map(|value| text_of(&value)));
    assert_eq!(selected.as_deref(), Some("b"));

    cx.simulate_input("zzz");
    let selected = cx.update(|_, cx| handle.selected(cx).map(|value| text_of(&value)));
    assert_eq!(selected, None, "nothing is shown");

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(cx.update(|_, cx| handle.selected(cx)).is_none());
}

#[gpui::test]
async fn minibuffer_ext_set_candidates_keeps_input_and_clamps_selection(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;

    // With an empty input.
    let handle = open_session(
        &workspace,
        cx,
        session(fixed(&["alpha", "beta", "gamma", "delta"])),
    );
    for _ in 0..3 {
        cx.dispatch_action(minibuffer::SelectNext);
    }
    cx.run_until_parked();
    assert_eq!(selected_index(&workspace, cx), 3);
    cx.update(|window, cx| handle.set_candidates(batch(&["one", "two"]), window, cx));
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx), ["one", "two"]);
    assert_eq!(selected_index(&workspace, cx), 1);
    let selected = cx.update(|_, cx| handle.selected(cx).map(|value| text_of(&value)));
    assert_eq!(selected.as_deref(), Some("two"));

    // With a typed input, which narrows the new list.
    cx.update(|window, cx| {
        handle.set_candidates(batch(&["alpha", "beta", "gamma", "delta"]), window, cx)
    });
    cx.simulate_input("a");
    assert_eq!(shown(&workspace, cx).len(), 4);
    for _ in 0..3 {
        cx.dispatch_action(minibuffer::SelectNext);
    }
    cx.run_until_parked();
    assert_eq!(selected_index(&workspace, cx), 3);
    cx.update(|window, cx| handle.set_candidates(batch(&["zeta", "eta", "rho"]), window, cx));
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "a", "the input is kept");
    let mut matches = shown(&workspace, cx);
    matches.sort();
    assert_eq!(matches, ["eta", "zeta"]);
    assert_eq!(selected_index(&workspace, cx), 1, "clamped to the last row");
}

#[gpui::test]
async fn minibuffer_ext_on_history_entry_runs_after_history_prev(cx: &mut TestAppContext) {
    let store =
        HistoryStore::new(KeyValueStore::open_test_db("minibuffer_ext_history_entry").await);
    cx.update(|cx| store.clone().install(cx));
    let (workspace, mut cx) = window(cx).await;
    let cx = &mut cx;

    open_session(&workspace, cx, session(fixed(&["alpha", "beta"])));
    cx.simulate_input("alp");
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(store.ring(HOST).entries(), ["alp"]);

    let entries = Rc::new(RefCell::new(Vec::new()));
    let seen = entries.clone();
    let weak = workspace.downgrade();
    open_session(
        &workspace,
        cx,
        session(fixed(&["alpha", "beta"])).on_history_entry(move |entry, _, _, cx| {
            // What the input holds when the handler runs.
            let input = weak
                .upgrade()
                .and_then(|workspace| workspace.read(cx).active_modal::<SessionView>(cx))
                .map(|view| view.read(cx).picker.read(cx).query(cx));
            seen.borrow_mut().push((entry.to_string(), input));
        }),
    );
    cx.simulate_input("b");
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "alp");
    assert_eq!(
        *entries.borrow(),
        [("alp".to_string(), Some("alp".to_string()))],
        "it runs with the entry, once the entry is in the input"
    );

    // Going back to the typed input is not a history entry.
    cx.dispatch_action(minibuffer::HistoryNext);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "b");
    assert_eq!(entries.borrow().len(), 1);
}
