//! Tests of `pick` and `ask` routed to the minibuffer strip (`work` in `minibuffer.enabled_for`):
//! a chain of both, where each continuation runs once its prompt has closed, a cancel that runs
//! nothing, a refused empty line, and the modals they stay when `work` is not routed.

use std::cell::RefCell;
use std::rc::Rc;

use bongos_minibuffer::{HistoryStore, SessionView};
use db::kvp::KeyValueStore;
use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::modals::{self, ChoicePicker, TextPrompt};

type Log = Rc<RefCell<Vec<String>>>;

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
    db_name: &'static str,
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        app_state
    });
    let store = HistoryStore::new(KeyValueStore::open_test_db(db_name).await);
    cx.update(|cx| {
        store.install(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(enabled_for.iter().map(|name| name.to_string()).collect());
            });
        });
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
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

fn session_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

fn confirm(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

/// Opens a pick of `alpha` and `beta` whose continuation logs the index and then asks for a name.
fn open_chain(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, log: &Log) {
    let picked = log.clone();
    workspace.update_in(cx, |workspace, window, cx| {
        modals::pick(
            workspace,
            "Pick one",
            vec!["alpha".into(), "beta".into()],
            0,
            Box::new(move |index, workspace, window, cx| {
                picked.borrow_mut().push(format!("picked:{index}"));
                let asked = picked;
                modals::ask(
                    workspace,
                    "Name",
                    "",
                    false,
                    Box::new(move |text, _, _, _| asked.borrow_mut().push(format!("asked:{text}"))),
                    window,
                    cx,
                );
            }),
            window,
            cx,
        );
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn routed_pick_then_ask_run_after_each_closes(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["work"], "work_modal_route_chain").await;
    let cx = &mut cx;
    let log: Log = Log::default();

    open_chain(&workspace, cx, &log);
    assert!(session_open(&workspace, cx) && in_strip(&workspace, cx));
    assert!(log.borrow().is_empty());

    cx.dispatch_action(minibuffer::SelectNext);
    confirm(cx);
    assert_eq!(*log.borrow(), ["picked:1"]);
    assert!(
        session_open(&workspace, cx) && in_strip(&workspace, cx),
        "the ask of the continuation is open in the strip"
    );

    cx.simulate_input("  hello  ");
    confirm(cx);
    assert_eq!(*log.borrow(), ["picked:1", "asked:hello"]);
    assert!(!session_open(&workspace, cx));
    assert!(!in_strip(&workspace, cx));
}

#[gpui::test]
async fn routed_cancel_runs_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["work"], "work_modal_route_cancel").await;
    let cx = &mut cx;
    let log: Log = Log::default();

    open_chain(&workspace, cx, &log);
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(log.borrow().is_empty());
    assert!(!session_open(&workspace, cx));

    open_chain(&workspace, cx, &log);
    confirm(cx);
    assert_eq!(*log.borrow(), ["picked:0"]);
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["picked:0"], "the cancelled ask ran nothing");
    assert!(!session_open(&workspace, cx));
}

#[gpui::test]
async fn routed_ask_refuses_an_empty_line_unless_allowed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["work"], "work_modal_route_empty").await;
    let cx = &mut cx;
    let log: Log = Log::default();

    for (allow_empty, initial) in [(false, ""), (true, ""), (false, "start")] {
        let asked = log.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            modals::ask(
                workspace,
                "Title",
                initial,
                allow_empty,
                Box::new(move |text, _, _, _| asked.borrow_mut().push(format!("asked:{text}"))),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        confirm(cx);
        if allow_empty || !initial.is_empty() {
            assert!(!session_open(&workspace, cx));
        } else {
            assert!(session_open(&workspace, cx), "an empty line is refused");
            cx.dispatch_action(minibuffer::Cancel);
            cx.run_until_parked();
        }
    }
    assert_eq!(*log.borrow(), ["asked:", "asked:start"]);
}

#[gpui::test]
async fn unrouted_pick_and_ask_stay_modals(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[], "work_modal_route_off").await;
    let cx = &mut cx;
    let log: Log = Log::default();

    open_chain(&workspace, cx, &log);
    workspace.read_with(cx, |workspace, cx| {
        assert!(workspace.active_modal::<SessionView>(cx).is_none());
        assert!(workspace.active_modal::<ChoicePicker>(cx).is_some());
    });
    assert!(!in_strip(&workspace, cx));

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["picked:0"]);
    workspace.read_with(cx, |workspace, cx| {
        assert!(workspace.active_modal::<TextPrompt>(cx).is_some());
    });
}
