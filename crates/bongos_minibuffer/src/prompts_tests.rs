//! Tests of the prompts built on sessions: the string read, the read-key, the yes/no question and
//! the strict `yes or no` question, with real key dispatch where a key is the answer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use db::kvp::KeyValueStore;
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, KeyDownEvent, Keystroke, Modifiers,
    TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::{
    HistoryStore, Outcome, ReadKey, ReadKeyView, Session, SessionHandle, SessionView, open,
    read_key, strict_yes_or_no, yes_or_no,
};

const HOST: &str = "test_host";

type Log = Rc<RefCell<Vec<String>>>;

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

/// A string session that logs its answer and its cancel.
fn string_session(log: &Log) -> Session {
    let confirmed = log.clone();
    let cancelled = log.clone();
    Session::string(HOST, "Name: ")
        .on_confirm(move |outcome, _, _| {
            confirmed.borrow_mut().push(match outcome {
                Outcome::Input(input) => format!("input:{input}"),
                Outcome::Candidate(_) => "candidate".to_string(),
            })
        })
        .on_cancel(move |_, _| cancelled.borrow_mut().push("cancelled".to_string()))
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

fn session_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx)
    })
}

fn input(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = session_view(workspace, cx).expect("a session is open");
    view.read_with(cx, |view, cx| view.picker.read(cx).query(cx))
}

fn key_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<ReadKeyView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ReadKeyView>(cx)
    })
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

/// Opens a read-key over `y n ! q` that logs the key that answered, and `default` if given.
fn open_read_key(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    log: &Log,
    default: Option<char>,
) {
    let answered = log.clone();
    let cancelled = log.clone();
    let mut prompt = ReadKey::new(HOST, "Save? ")
        .key('y', "yes")
        .key('n', "no")
        .key('!', "all")
        .key('q', "quit")
        .on_answer(move |key, _, _| answered.borrow_mut().push(format!("answer:{key}")))
        .on_cancel(move |_, _| cancelled.borrow_mut().push("cancelled".to_string()));
    if let Some(default) = default {
        prompt = prompt.default(default);
    }
    workspace.update_in(cx, |workspace, window, cx| {
        read_key(workspace, prompt, window, cx)
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn minibuffer_string_returns_the_edited_text_or_the_default(cx: &mut TestAppContext) {
    let store = HistoryStore::new(KeyValueStore::open_test_db("minibuffer_string_text").await);
    cx.update(|cx| store.install(cx));
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_session(&workspace, cx, string_session(&log));
    assert!(in_strip(&workspace, cx));
    let view = session_view(&workspace, cx).unwrap();
    view.read_with(cx, |view, cx| {
        let delegate = &view.picker.read(cx).delegate;
        assert_eq!(picker::PickerDelegate::match_count(delegate), 0);
        assert!(delegate.shown_texts().is_empty());
    });
    cx.simulate_input("hello");
    cx.simulate_input(" there!");
    assert_eq!(input(&workspace, cx), "hello there!");
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["input:hello there!"]);
    assert!(session_view(&workspace, cx).is_none());

    open_session(&workspace, cx, string_session(&log).default("fallback"));
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow()[1], "input:fallback");

    open_session(&workspace, cx, string_session(&log).default("fallback"));
    cx.simulate_input("typed");
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow()[2], "input:typed", "typing beats the default");

    open_session(&workspace, cx, string_session(&log));
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(log.borrow()[3], "cancelled");
}

#[gpui::test]
async fn minibuffer_string_on_input_sees_each_edit_and_set_prompt_relabels(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;
    let inputs = log.clone();
    open_session(
        &workspace,
        cx,
        string_session(&log).on_input(move |text, handle, window, cx| {
            inputs.borrow_mut().push(text.to_string());
            handle.set_prompt(format!("Name ({} chars): ", text.len()), window, cx);
        }),
    );
    cx.simulate_input("abc");
    assert_eq!(*log.borrow(), ["a", "ab", "abc"]);
    let view = session_view(&workspace, cx).unwrap();
    view.read_with(cx, |view, _| {
        assert_eq!(*view.core.prompt.borrow(), "Name (3 chars): ");
    });
}

#[gpui::test]
async fn minibuffer_string_m_p_and_m_n_walk_a_named_history(cx: &mut TestAppContext) {
    let store = HistoryStore::new(KeyValueStore::open_test_db("minibuffer_string_history").await);
    cx.update(|cx| store.clone().install(cx));
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    for (typed, name) in [("one", "names"), ("two", "names"), ("elsewhere", "other")] {
        open_session(&workspace, cx, string_session(&log).history(name));
        cx.simulate_input(typed);
        cx.dispatch_action(minibuffer::Confirm);
        cx.run_until_parked();
    }
    assert_eq!(store.ring("names").entries(), ["two", "one"]);

    open_session(&workspace, cx, string_session(&log).history("names"));
    cx.simulate_input("draft");
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "two");
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "one");
    cx.dispatch_action(minibuffer::HistoryNext);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "two");
    cx.dispatch_action(minibuffer::HistoryNext);
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "draft", "the typed text comes back");
}

#[gpui::test]
async fn minibuffer_read_key_answers_with_one_key_and_no_ret(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_read_key(&workspace, cx, &log, Some('n'));
    assert!(in_strip(&workspace, cx));
    let view = key_view(&workspace, cx).unwrap();
    view.read_with(cx, |view, _| assert_eq!(view.legend(), "y n ! q"));
    cx.simulate_keystrokes("x");
    assert!(key_view(&workspace, cx).is_some(), "x is not declared");
    cx.simulate_keystrokes("!");
    assert_eq!(*log.borrow(), ["answer:!"], "! is declared");
    assert!(key_view(&workspace, cx).is_none());

    open_read_key(&workspace, cx, &log, None);
    cx.simulate_keystrokes("y");
    cx.run_until_parked();
    assert_eq!(log.borrow()[1], "answer:y");
    assert!(key_view(&workspace, cx).is_none());
}

#[gpui::test]
async fn minibuffer_read_key_ret_and_c_g_answer_the_default(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_read_key(&workspace, cx, &log, Some('q'));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    open_read_key(&workspace, cx, &log, Some('n'));
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["answer:q", "answer:n"]);

    // With no default RET waits and C-g cancels; the minibuffer's own cancel does the same.
    open_read_key(&workspace, cx, &log, None);
    cx.simulate_keystrokes("enter");
    assert!(key_view(&workspace, cx).is_some());
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert_eq!(log.borrow()[2], "cancelled");

    // The cancel action answers the default, once.
    open_read_key(&workspace, cx, &log, Some('n'));
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(log.borrow()[3], "answer:n");
    assert_eq!(log.borrow().len(), 4);
}

/// Opens a read-key that any single printable character answers, logging what happened.
fn open_any_char(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, log: &Log) {
    let answered = log.clone();
    let cancelled = log.clone();
    let prompt = ReadKey::new(HOST, "Zap to char: ")
        .any_char()
        .on_answer(move |key, _, _| answered.borrow_mut().push(format!("answer:{key}")))
        .on_cancel(move |_, _| cancelled.borrow_mut().push("cancelled".to_string()));
    workspace.update_in(cx, |workspace, window, cx| {
        read_key(workspace, prompt, window, cx)
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn minibuffer_read_key_any_char_answers_with_undeclared_characters(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_any_char(&workspace, cx, &log);
    cx.simulate_keystrokes(")");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["answer:)"]);
    assert!(key_view(&workspace, cx).is_none());

    open_any_char(&workspace, cx, &log);
    cx.simulate_keystrokes("shift-z");
    cx.run_until_parked();
    assert_eq!(log.borrow()[1], "answer:Z");

    open_any_char(&workspace, cx, &log);
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(log.borrow()[2], "answer:x");
    assert_eq!(log.borrow().len(), 3);
}

#[gpui::test]
async fn minibuffer_read_key_any_char_uppercases_a_shifted_key_without_a_typed_character(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    for key_char in [None, Some(String::new())] {
        open_any_char(&workspace, cx, &log);
        cx.simulate_event(KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::shift(),
                key: "z".to_string(),
                key_char,
            },
            is_held: false,
            prefer_character_input: false,
        });
        cx.run_until_parked();
    }
    assert_eq!(*log.borrow(), ["answer:Z", "answer:Z"]);
}

#[gpui::test]
async fn minibuffer_read_key_any_char_cancels_on_c_g_and_escape_and_ignores_modified_keys(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_any_char(&workspace, cx, &log);
    cx.simulate_keystrokes("ctrl-x alt-x enter");
    cx.run_until_parked();
    assert!(
        log.borrow().is_empty(),
        "modified keys and RET are not answers"
    );
    assert!(key_view(&workspace, cx).is_some());
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["cancelled"]);
    assert!(key_view(&workspace, cx).is_none());

    open_any_char(&workspace, cx, &log);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["cancelled", "cancelled"]);
}

#[gpui::test]
async fn minibuffer_yes_or_no_is_a_read_key_over_y_and_n(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;

    for (keys, default) in [
        ("y", false),
        ("n", true),
        ("enter", false),
        ("ctrl-g", false),
    ] {
        let answered = log.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            yes_or_no(
                workspace,
                HOST,
                "Sure? ",
                default,
                move |answer, _, _| answered.borrow_mut().push(answer.to_string()),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let view = key_view(&workspace, cx).unwrap();
        view.read_with(cx, |view, _| assert_eq!(view.legend(), "y n"));
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
    }
    assert_eq!(*log.borrow(), ["true", "false", "false", "false"]);
}

#[gpui::test]
async fn minibuffer_strict_yes_or_no_refuses_y_and_accepts_yes(cx: &mut TestAppContext) {
    let store = HistoryStore::new(KeyValueStore::open_test_db("minibuffer_strict").await);
    cx.update(|cx| store.install(cx));
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let log = Log::default();
    let cx = &mut cx;
    let open_strict = |cx: &mut VisualTestContext| {
        let answered = log.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            strict_yes_or_no(
                workspace,
                HOST,
                "Really? (yes or no) ",
                move |answer, _, _| answered.borrow_mut().push(format!("{answer:?}")),
                window,
                cx,
            )
        });
        cx.run_until_parked();
    };

    open_strict(cx);
    for refused in ["", "y", "ye", "n", "maybe"] {
        cx.simulate_input(refused);
        cx.dispatch_action(minibuffer::Confirm);
        cx.dispatch_action(minibuffer::ConfirmInput);
        cx.run_until_parked();
        assert!(log.borrow().is_empty(), "{refused:?} is refused");
        assert!(session_view(&workspace, cx).is_some());
        let view = session_view(&workspace, cx).unwrap();
        view.read_with(cx, |view, _| {
            assert_eq!(*view.core.status.borrow(), "Please answer yes or no.");
        });
        view.update_in(cx, |view, window, cx| view.set_input("", window, cx));
        cx.run_until_parked();
    }
    cx.simulate_input("yes");
    let view = session_view(&workspace, cx).unwrap();
    view.read_with(cx, |view, _| assert!(view.core.status.borrow().is_empty()));
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["Some(true)"]);

    open_strict(cx);
    cx.simulate_input("NO");
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(log.borrow()[1], "Some(false)");

    open_strict(cx);
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(log.borrow()[2], "None");
}

#[gpui::test]
async fn minibuffer_prompts_are_centered_while_their_host_is_not_routed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[]).await;
    let log = Log::default();
    let cx = &mut cx;

    open_read_key(&workspace, cx, &log, Some('n'));
    assert!(key_view(&workspace, cx).is_some());
    assert!(!in_strip(&workspace, cx), "nothing routes by default");
    cx.simulate_keystrokes("y");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["answer:y"]);

    open_session(&workspace, cx, string_session(&log));
    assert!(session_view(&workspace, cx).is_some());
    assert!(!in_strip(&workspace, cx));
}
