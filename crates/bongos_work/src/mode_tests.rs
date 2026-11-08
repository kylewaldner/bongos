//! `task: mode`: chat and run are dispatched by name when something handles `chat::New` and
//! `workflow::Run`, and otherwise say which feature provides them. The tests stand in for
//! features 04 and 20 with actions of the same names defined here.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use db::AppDatabase;
use gpui::{TestAppContext, VisualTestContext};

use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::modes::{CHAT_MISSING, RUN_MISSING};
use crate::registry::{ActiveRun, LiveModes, LiveModesRegistry, LiveModesSource};
use crate::{Focus, focus};

mod fake {
    use gpui::actions;

    actions!(chat, [New]);
    actions!(workflow, [Run]);
}

#[derive(Default)]
struct Calls {
    chats: AtomicUsize,
    runs: AtomicUsize,
}

/// Gives the harness a [`Focus`] on the task `id`.
fn focus_on(harness: &Harness, id: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.set_global(AppDatabase::test_new());
        focus::init(cx);
        let focus = Focus::global(cx).unwrap();
        focus.update(cx, |focus, cx| focus.set(Some(id.to_string()), cx));
    });
    harness.clear_toasts(cx);
}

/// Handles the fake `chat::New` and `workflow::Run` in the workspace, counting the calls.
fn handle_fakes(harness: &Harness, calls: &Arc<Calls>, cx: &mut VisualTestContext) {
    let calls = calls.clone();
    harness.workspace.update(cx, |workspace, cx| {
        let chats = calls.clone();
        let runs = calls.clone();
        workspace
            .register_action(move |_, _: &fake::New, _, _| {
                chats.chats.fetch_add(1, Ordering::SeqCst);
            })
            .register_action(move |_, _: &fake::Run, _, _| {
                runs.runs.fetch_add(1, Ordering::SeqCst);
            });
        cx.notify();
    });
    cx.run_until_parked();
}

struct Running(RefCell<Option<ActiveRun>>);

impl LiveModesSource for Running {
    fn live_modes(&self, task_id: &str, _: &gpui::App) -> LiveModes {
        LiveModes {
            open_chats: 0,
            run: self.0.borrow().clone().filter(|_| task_id == "l1"),
        }
    }
}

#[test]
fn the_missing_messages_name_the_features() {
    assert_eq!(CHAT_MISSING, "Chat sessions arrive with feature 04");
    assert_eq!(RUN_MISSING, "Workflows arrive with feature 20");
}

#[gpui::test]
async fn chat_and_run_toast_when_nothing_handles_them(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    focus_on(&harness, "l1", cx);
    let calls = Arc::new(Calls::default());

    for mode in ["Chat:", "Run:"] {
        harness.clear_toasts(cx);
        harness.run(cx, "task::Mode", &[Input::Pick(mode)]).await;
        assert!(harness.has_toast(cx), "{mode} showed no toast");
        assert!(harness.picker(cx).is_none());
    }
    assert_eq!(calls.chats.load(Ordering::SeqCst), 0);
    assert_eq!(calls.runs.load(Ordering::SeqCst), 0);
}

#[gpui::test]
async fn chat_and_run_dispatch_by_name_when_handled(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    focus_on(&harness, "l1", cx);
    let calls = Arc::new(Calls::default());
    handle_fakes(&harness, &calls, cx);

    harness.run(cx, "task::Mode", &[Input::Pick("Chat:")]).await;
    assert_eq!(calls.chats.load(Ordering::SeqCst), 1);
    assert!(!harness.has_toast(cx));

    harness.run(cx, "task::Mode", &[Input::Pick("Run:")]).await;
    assert_eq!(calls.runs.load(Ordering::SeqCst), 1);
    assert!(!harness.has_toast(cx));

    // A leaf with an active run gets no second one: a toast with the chip, nothing dispatched.
    let running = Rc::new(Running(RefCell::new(Some(ActiveRun {
        workflow: "ship".to_string(),
        status: "running".to_string(),
        step: "build".to_string(),
        past_runs: 0,
    }))));
    cx.update(|_, cx| LiveModesRegistry::add_source("runs", running, cx));
    harness.run(cx, "task::Mode", &[Input::Pick("Run:")]).await;
    assert_eq!(calls.runs.load(Ordering::SeqCst), 1);
    assert!(harness.has_toast(cx));
    // Chat is not held back by a run.
    harness.clear_toasts(cx);
    harness.run(cx, "task::Mode", &[Input::Pick("Chat:")]).await;
    assert_eq!(calls.chats.load(Ordering::SeqCst), 2);

    // A group has no run.
    focus_on_group(&harness, cx);
    harness.run(cx, "task::Mode", &[Input::Pick("Run:")]).await;
    assert_eq!(calls.runs.load(Ordering::SeqCst), 1);
    assert!(harness.has_toast(cx));
}

/// Whether the window's file system, where `task: think` creates documents, has `path`.
fn has_file(harness: &Harness, path: &std::path::Path, cx: &mut VisualTestContext) -> bool {
    let fs = harness
        .workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.as_fake());
    fs.paths(false).iter().any(|existing| existing == path)
}

fn focus_on_group(harness: &Harness, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        let focus = Focus::global(cx).unwrap();
        focus.update(cx, |focus, cx| focus.set(Some("g2".to_string()), cx));
    });
    harness.clear_toasts(cx);
}

#[gpui::test]
async fn thinking_opens_a_new_document_and_nothing_else(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    focus_on(&harness, "l1", cx);
    let calls = Arc::new(Calls::default());
    handle_fakes(&harness, &calls, cx);
    let writes = harness.writes(cx);

    harness
        .run(cx, "task::Mode", &[Input::Pick("Thinking:")])
        .await;
    let document = harness.task_dir(&["backfill"]).join("untitled-1.md");
    wait_until(cx, "the new document", |cx| {
        has_file(&harness, &document, cx)
    })
    .await;
    assert!(!harness.has_toast(cx));
    assert_eq!(calls.chats.load(Ordering::SeqCst), 0);
    assert_eq!(calls.runs.load(Ordering::SeqCst), 0);
    assert_eq!(harness.writes(cx), writes);
}

#[gpui::test]
async fn without_a_focused_task_the_mode_asks_which(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    // No focus global at all: the picker starts with the tasks, and thinking works on the pick.
    dispatch(cx, "task::Mode");
    assert!(harness.picker(cx).is_some());
    let labels = harness.choices(cx);
    assert!(labels.iter().any(|label| label.contains("Backfill")));
    harness.answer(cx, &Input::Pick("Backfill")).await;
    harness.answer(cx, &Input::Pick("Thinking:")).await;
    let document = harness.task_dir(&["backfill"]).join("untitled-1.md");
    wait_until(cx, "the new document", |cx| {
        has_file(&harness, &document, cx)
    })
    .await;
}

#[gpui::test]
async fn a_mode_on_an_unfocused_task_focuses_it_first(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    // A focus with no task, so the picker asks which task, and chat focuses the pick first.
    cx.update(|_, cx| {
        cx.set_global(AppDatabase::test_new());
        focus::init(cx);
    });
    // The window switches to another workspace, which has no handlers of its own, so the chat
    // action is also handled app-wide to see it run there.
    let calls = Arc::new(Calls::default());
    handle_fakes(&harness, &calls, cx);
    let chats = calls.clone();
    cx.update(|_, cx| {
        cx.on_action(move |_: &fake::New, _| {
            chats.chats.fetch_add(1, Ordering::SeqCst);
        });
    });
    cx.run_until_parked();

    dispatch(cx, "task::Mode");
    harness.answer(cx, &Input::Pick("Backfill")).await;
    harness.answer(cx, &Input::Pick("Chat:")).await;
    wait_until(cx, "the focus", |cx| {
        cx.update(|_, cx| Focus::global(cx).unwrap().read(cx).task_id() == Some("l1"))
    })
    .await;
    wait_until(cx, "chat: new", |_| calls.chats.load(Ordering::SeqCst) == 1).await;
    assert!(harness.picker(cx).is_none());
    assert!(!harness.has_toast(cx));
}
