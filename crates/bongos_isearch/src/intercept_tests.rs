//! Tests of the keystroke interceptor: the key rule as a table over a synthetic keymap and the
//! session's real context stack, and the implicit exit end to end: the key that ends the session
//! runs exactly once, in the origin editor, from the match, and the text never changes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use editor::Editor;
use gpui::{
    Focusable as _, KeyBinding, KeyContext, Keymap, Keystroke, Subscription, TestAppContext,
    VisualTestContext, actions,
};
use settings::KeymapFile;
use workspace::{SaveAll, Workspace};

use crate::actions::{Backward, Forward, ForwardRegexp};
use crate::intercept::{Decision, decide};
use crate::session::{TEXT_HISTORY, test_session};
use crate::session_tests::{
    cursor, input, keys, mark, prompt, ring, selections, session_open, set_selections, start, text,
    type_slowly, workspace_text,
};

actions!(intercept_tests, [AltSX, Palette, Counted, XSave]);

/// Four lines with an `x` each, at 2, 6, 10 and 14.
const XS: &str = "a x\nb x\nc x\nd x\n";

fn key(text: &str) -> Keystroke {
    Keystroke::parse(text).unwrap().with_simulated_ime()
}

fn keystrokes(text: &str) -> Vec<Keystroke> {
    text.split(' ').map(key).collect()
}

/// The rows of a base keymap the session's keys are decided against: an editor motion, an
/// editor prefix, the regexp starter above `workspace::SaveAll`, an editor `alt-s x` binding
/// that shares the prefix of the session's `alt-s` rows and a workspace `alt-x`.
fn base_rows() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-f", editor::actions::MoveRight, Some("Editor")),
        KeyBinding::new("ctrl-x ctrl-s", XSave, Some("Editor")),
        KeyBinding::new(
            "ctrl-alt-s",
            SaveAll { save_intent: None },
            Some("Workspace"),
        ),
        KeyBinding::new("ctrl-alt-s", ForwardRegexp, Some("Editor")),
        KeyBinding::new("alt-s x", AltSX, Some("Editor")),
        KeyBinding::new("alt-x", Palette, Some("Workspace")),
    ]
}

/// Records the action of every handled keystroke, by name.
fn record_actions(cx: &mut VisualTestContext) -> (Rc<RefCell<Vec<String>>>, Subscription) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let subscription = cx.update(|_, cx| {
        let log = log.clone();
        cx.observe_keystrokes(move |event, _, _| {
            if let Some(action) = &event.action {
                log.borrow_mut().push(action.name().to_string());
            }
        })
    });
    (log, subscription)
}

/// The context stack of the session's input, as a key typed there sees it.
fn session_stack(cx: &mut VisualTestContext) -> Vec<KeyContext> {
    let stack = Rc::new(RefCell::new(Vec::new()));
    let subscription = cx.update(|_, cx| {
        let stack = stack.clone();
        cx.intercept_keystrokes(move |event, _, _| {
            *stack.borrow_mut() = event.context_stack.clone();
        })
    });
    keys("q", cx);
    drop(subscription);
    keys("backspace", cx);
    stack.take()
}

async fn the_key_rule_follows_the_first_binding(routed: bool, cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = workspace_text(XS, routed, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    let stack = session_stack(cx);
    assert!(
        stack.iter().any(|context| context.contains("Isearch")),
        "{stack:?}"
    );
    assert_eq!(input(&workspace, cx), "");

    let overrides = cx.update(|_, cx| {
        KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx).unwrap()
    });
    let keymap = Keymap::new(base_rows().into_iter().chain(overrides).collect());
    let decide_keys = |keymap: &Keymap, pending: &str, typed: &str| {
        let pending = if pending.is_empty() {
            Vec::new()
        } else {
            keystrokes(pending)
        };
        decide(keymap, &pending, &key(typed), &stack)
    };
    let exits = |text: &str| Decision::Exit(keystrokes(text));

    // Text for the string, with or without Shift.
    for typed in ["a", "shift-a", "space", "1", "shift-1", "["] {
        assert_eq!(
            decide_keys(&keymap, "", typed),
            Decision::Through,
            "{typed}"
        );
    }
    // The session's own keys.
    for typed in [
        "ctrl-s",
        "ctrl-r",
        "backspace",
        "ctrl-g",
        "escape",
        "enter",
        "ctrl-enter",
        "alt-p",
        "alt-n",
        "ctrl-w",
        "ctrl-alt-y",
        "ctrl-y",
        "ctrl-j",
        "alt-e",
        "alt-c",
        "alt-r",
        "ctrl-l",
    ] {
        assert_eq!(
            decide_keys(&keymap, "", typed),
            Decision::Through,
            "{typed}"
        );
    }
    // Everything else exits and runs: a motion, a prefix, the starters (whose fallthrough in
    // the input would be `workspace::SaveAll`), M-x, keys of the minibuffer itself, and keys
    // bound to nothing.
    for typed in [
        "ctrl-f",
        "ctrl-x",
        "ctrl-alt-s",
        "alt-x",
        "tab",
        "ctrl-n",
        "ctrl-q",
        "alt-q",
        "f5",
        "left",
    ] {
        assert_eq!(decide_keys(&keymap, "", typed), exits(typed), "{typed}");
    }

    // `alt-s` is the prefix of session rows, so it waits for the next key: a session pair goes
    // through, any other pair exits with both keys, a printable one too.
    assert_eq!(decide_keys(&keymap, "", "alt-s"), Decision::Through);
    for typed in [".", "ctrl-e", "space", "alt-<", "alt->"] {
        assert_eq!(
            decide_keys(&keymap, "alt-s", typed),
            Decision::Through,
            "alt-s {typed}"
        );
    }
    assert_eq!(decide_keys(&keymap, "alt-s", "x"), exits("alt-s x"));
    assert_eq!(
        decide_keys(&keymap, "alt-s", "ctrl-f"),
        exits("alt-s ctrl-f")
    );
    keys("ctrl-g", cx);
}

#[gpui::test]
async fn isearch_the_key_rule_in_a_centered_session(cx: &mut TestAppContext) {
    the_key_rule_follows_the_first_binding(false, cx).await;
}

#[gpui::test]
async fn isearch_the_key_rule_in_the_strip(cx: &mut TestAppContext) {
    the_key_rule_follows_the_first_binding(true, cx).await;
}

/// A session in `XS` with the base rows bound, the match of `x` after offset 0 at 2..3.
async fn session_on_x(
    routed: bool,
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<Workspace>,
    gpui::Entity<Editor>,
    VisualTestContext,
) {
    let (workspace, editor, mut cx) = workspace_text(XS, routed, cx).await;
    cx.update(|_, cx| cx.bind_keys(base_rows()));
    start(Forward, &workspace, &mut cx);
    type_slowly("x", &mut cx);
    assert_eq!(cursor(&editor, &mut cx), 3);
    (workspace, editor, cx)
}

/// PRD 9: C-f after a match ends the session at the match end and moves one character right,
/// once.
#[gpui::test]
async fn isearch_ctrl_f_exits_and_runs_once_from_the_match(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(true, cx).await;
    let cx = &mut cx;
    let (log, _subscription) = record_actions(cx);

    keys("ctrl-f", cx);
    assert!(!session_open(&workspace, cx));
    assert!(test_session(&workspace, cx).is_none());
    assert_eq!(
        cursor(&editor, cx),
        4,
        "one character right of the match end"
    );
    assert_eq!(*log.borrow(), vec!["editor::MoveRight".to_string()]);
    assert_eq!(mark(&editor, cx), Some(0), "the origin is pushed");
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["x".to_string()]);
    assert_eq!(text(&editor, cx), XS);
    let focused = cx.update(|window, cx| editor.focus_handle(cx).is_focused(window));
    assert!(focused, "focus is back in the origin editor");

    // The interceptor ended with the session: the next C-f is the editor's alone.
    keys("ctrl-f", cx);
    assert_eq!(cursor(&editor, cx), 5);
    assert_eq!(log.borrow().len(), 2);
}

/// Going backward the implicit exit leaves point at the match start.
#[gpui::test]
async fn isearch_ctrl_f_after_a_backward_match_runs_from_its_start(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| cx.bind_keys(base_rows()));
    set_selections(&editor, vec![16..16], cx);
    start(Backward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 14);
    keys("ctrl-f", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(cursor(&editor, cx), 15);
    assert_eq!(mark(&editor, cx), Some(16));
    assert_eq!(text(&editor, cx), XS);
}

/// A key typed while the find of the string still runs waits for it, so it runs from the match
/// of the string as typed; keys typed after it run after it, in order.
#[gpui::test]
async fn isearch_an_exit_key_waits_for_the_running_find(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| cx.bind_keys(base_rows()));
    let (log, _subscription) = record_actions(cx);
    start(Forward, &workspace, cx);
    // In one go: no find lands before the keys after `b x` arrive.
    keys("b space x ctrl-f ctrl-f", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(cursor(&editor, cx), 9, "two right of the end of `b x`");
    assert_eq!(
        *log.borrow(),
        vec![
            "editor::MoveRight".to_string(),
            "editor::MoveRight".to_string()
        ]
    );
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["b x".to_string()]);
    assert_eq!(text(&editor, cx), XS);
}

/// C-M-s in the input first resolves to the starter row, which propagates outside a full editor
/// and would fall through to `workspace::SaveAll`. It exits instead, and in the origin editor it
/// starts a regexp session from the match; `SaveAll` never runs.
#[gpui::test]
async fn isearch_a_starter_exits_and_never_falls_through(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(false, cx).await;
    let cx = &mut cx;
    let (log, _subscription) = record_actions(cx);

    keys("ctrl-alt-s", cx);
    assert_eq!(*log.borrow(), vec!["isearch::ForwardRegexp".to_string()]);
    assert!(
        session_open(&workspace, cx),
        "a regexp session in the editor"
    );
    assert_eq!(prompt(&workspace, cx), "Regexp I-search: ");
    assert_eq!(
        mark(&editor, cx),
        Some(0),
        "the first session pushed its origin"
    );
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["x".to_string()]);
    // The new session searches from the first one's match.
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 7);
    keys("ctrl-g", cx);
    assert_eq!(cursor(&editor, cx), 3);
    assert!(!log.borrow().iter().any(|name| name == "workspace::SaveAll"));
    assert_eq!(text(&editor, cx), XS);
}

/// `alt-s` is the prefix of session rows, and `x` is none of them: the session exits and both keys
/// run in the editor, where they complete its binding, once, and neither reaches the string.
#[gpui::test]
async fn isearch_alt_s_x_exits_and_runs_both_keys(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(true, cx).await;
    let cx = &mut cx;
    let (log, _subscription) = record_actions(cx);
    let ran = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        let ran = ran.clone();
        cx.on_action(move |_: &AltSX, _| ran.set(ran.get() + 1));
    });

    keys("alt-s x", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(ran.get(), 1);
    assert_eq!(*log.borrow(), vec!["intercept_tests::AltSX".to_string()]);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["x".to_string()]);
    assert_eq!(text(&editor, cx), XS);
}

/// M-x in the strip first resolves to the minibuffer's own row; it exits, and the palette opens
/// from the editor, not from inside the session.
#[gpui::test]
async fn isearch_alt_x_exits_and_opens_the_palette_in_the_editor(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(true, cx).await;
    let cx = &mut cx;
    let (log, _subscription) = record_actions(cx);
    // Handled on the origin editor only, so it counts the palette opened from there.
    let opened_in_editor = Rc::new(Cell::new(0));
    editor.update(cx, |editor, _| {
        let opened = opened_in_editor.clone();
        editor
            .register_action(move |_: &Palette, _, _| opened.set(opened.get() + 1))
            .detach();
    });

    keys("alt-x", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(*log.borrow(), vec!["intercept_tests::Palette".to_string()]);
    assert_eq!(opened_in_editor.get(), 1);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(text(&editor, cx), XS);
}

/// The origin pane's editor closes during the session: the key that ends it still runs once,
/// where focus lands, and the interceptor goes with the session.
#[gpui::test]
async fn isearch_an_exit_after_the_origin_pane_closed(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(true, cx).await;
    let cx = &mut cx;
    let counted = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new("ctrl-o", Counted, None)]);
        let counted = counted.clone();
        cx.on_action(move |_: &Counted, _| counted.set(counted.get() + 1));
    });
    let buffer = editor.read_with(cx, |editor, _| editor.buffer().clone());
    drop(editor);

    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| {
        pane.close_active_item(&Default::default(), window, cx)
            .detach()
    });
    cx.run_until_parked();
    assert!(pane.read_with(cx, |pane, _| pane.items_len() == 0));
    assert!(
        session_open(&workspace, cx),
        "the session outlives the pane item"
    );

    keys("ctrl-o", cx);
    assert!(!session_open(&workspace, cx));
    assert!(test_session(&workspace, cx).is_none());
    assert_eq!(counted.get(), 1, "the key ran once");
    keys("ctrl-o", cx);
    assert_eq!(counted.get(), 2, "nothing intercepts it any more");
    assert_eq!(
        buffer.read_with(cx, |buffer, cx| buffer.read(cx).text()),
        XS
    );
}

/// PRD 17: the interceptor acts on the session's window and context only. An editor in another
/// window keeps every key while the session stays open.
#[gpui::test]
async fn isearch_keys_elsewhere_are_not_intercepted(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = session_on_x(true, cx).await;
    let other_window = cx.add_window(|window, cx| Editor::multi_line(window, cx));
    let other_editor = other_window.root(&mut cx.cx).unwrap();
    let mut other = VisualTestContext::from_window(other_window.into(), &cx.cx);
    other_editor.update_in(&mut other, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    other.run_until_parked();

    other.simulate_input("ab");
    other.simulate_keystrokes("ctrl-f enter");
    other.run_until_parked();
    assert_eq!(
        other_editor.update(&mut other, |editor, cx| editor.text(cx)),
        "ab\n"
    );

    let cx = &mut cx;
    assert!(session_open(&workspace, cx), "the session goes on");
    assert_eq!(input(&workspace, cx), "x");
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 7);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![6..7]);
    assert_eq!(text(&editor, cx), XS);
}
