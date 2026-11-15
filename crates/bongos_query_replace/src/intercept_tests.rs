//! Tests of the keystroke interceptor of the decision view: the key rule as a table over a
//! synthetic keymap and the view's real context stack, and the implicit exit end to end: the key
//! that ends the session runs exactly once, in the origin editor, after the session's edits are one
//! undo step; during a `!` batch it waits for the batch.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    Action as _, Focusable as _, KeyBinding, KeyContext, Keymap, Keystroke, Subscription,
    TestAppContext, actions,
};
use settings::KeymapFile;

use crate::actions::{All, Exit, QueryReplace};
use crate::commands_tests::{Scene, scene};
use crate::engine::SCAN_CHUNK;
use crate::intercept::in_session;
use crate::session::batch_running;
use bongos_isearch::intercept::{Decision, decide_with};

actions!(query_replace_intercept_tests, [Palette, Counted, XSave]);

fn key(text: &str) -> Keystroke {
    Keystroke::parse(text).unwrap().with_simulated_ime()
}

fn keystrokes(text: &str) -> Vec<Keystroke> {
    text.split(' ').map(key).collect()
}

/// The rows of a base keymap: editor motion and undo, an editor prefix, a workspace `alt-x`, and
/// a two-key decision row (`ctrl-c q`) whose prefix the view waits on.
fn base_rows() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-f", editor::actions::MoveRight, Some("Editor")),
        KeyBinding::new("ctrl-/", editor::actions::Undo, Some("Editor")),
        KeyBinding::new("ctrl-x ctrl-s", XSave, Some("Editor")),
        KeyBinding::new("alt-x", Palette, Some("Workspace")),
    ]
}

async fn asking(text: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene(text, cx).await;
    scene.cx.update(|_, cx| cx.bind_keys(base_rows()));
    scene.select(&[0..0]);
    scene.replace(QueryReplace, "foo", "bar");
    assert!(scene.view_open(), "the decision view opens");
    scene
}

impl Scene {
    /// Records the action of every handled keystroke, by name.
    fn record_actions(&mut self) -> (Rc<RefCell<Vec<String>>>, Subscription) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let subscription = self.cx.update(|_, cx| {
            let log = log.clone();
            cx.observe_keystrokes(move |event, _, _| {
                if let Some(action) = &event.action {
                    log.borrow_mut().push(action.name().to_string());
                }
            })
        });
        (log, subscription)
    }

    fn editor_focused(&mut self) -> bool {
        let editor = self.editor.clone();
        self.cx
            .update(|window, cx| editor.focus_handle(cx).is_focused(window))
    }

    /// The context stack of the decision view, as a key typed there sees it (`?`, which shows the
    /// help).
    fn view_stack(&mut self) -> Vec<KeyContext> {
        let stack = Rc::new(RefCell::new(Vec::new()));
        let subscription = self.cx.update(|_, cx| {
            let stack = stack.clone();
            cx.intercept_keystrokes(move |event, _, _| {
                *stack.borrow_mut() = event.context_stack.clone();
            })
        });
        self.keys("?");
        drop(subscription);
        stack.take()
    }
}

#[gpui::test]
async fn the_key_rule_lets_only_decision_keys_through(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo\n", cx).await;
    let stack = scene.view_stack();
    assert!(
        stack.iter().any(|context| context.contains("QueryReplace")),
        "{stack:?}"
    );
    let overrides = scene.cx.update(|_, cx| {
        KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx).unwrap()
    });
    let mut rows = base_rows();
    rows.extend(overrides);
    rows.push(KeyBinding::new("ctrl-c q", Exit, Some("QueryReplace")));
    let keymap = Keymap::new(rows);
    let decide = |pending: &str, typed: &str, batch: bool| {
        let pending = if pending.is_empty() {
            Vec::new()
        } else {
            keystrokes(pending)
        };
        decide_with(
            &keymap,
            &pending,
            &key(typed),
            &stack,
            |name| in_session(name, batch),
            false,
        )
    };
    let exits = |text: &str| Decision::Exit(keystrokes(text));

    for typed in [
        "y",
        "space",
        "n",
        "backspace",
        "!",
        ".",
        ",",
        "^",
        "u",
        "e",
        "q",
        "enter",
        "escape",
        "ctrl-g",
        "ctrl-l",
        "?",
        "ctrl-h",
        "ctrl-c",
    ] {
        assert_eq!(decide("", typed, false), Decision::Through, "{typed}");
    }
    assert_eq!(decide("ctrl-c", "q", false), Decision::Through);
    // The view takes no text: a printable key that is no decision key ends the session too, as
    // do motions, prefixes, M-x and keys bound to nothing, with or without a modifier.
    for typed in [
        "x", "shift-a", "1", "ctrl-f", "ctrl-/", "ctrl-x", "alt-x", "tab", "f5", "left", "alt-q",
    ] {
        assert_eq!(decide("", typed, false), exits(typed), "{typed}");
    }
    assert_eq!(decide("ctrl-c", "x", false), exits("ctrl-c x"));

    // During a batch only C-g goes through.
    assert_eq!(decide("", "ctrl-g", true), Decision::Through);
    for typed in ["y", "q", "?", "x", "ctrl-f"] {
        assert_eq!(decide("", typed, true), exits(typed), "{typed}");
    }
    scene.keys("q");
}

#[gpui::test]
async fn ctrl_f_ends_the_session_and_moves_once_from_the_match_end(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", cx).await;
    scene.keys("y");
    assert_eq!(scene.selections(), vec![7..7]);
    let (log, _subscription) = scene.record_actions();

    scene.keys("ctrl-f");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.selections(), vec![8..8], "one right of the match end");
    assert_eq!(*log.borrow(), vec!["editor::MoveRight".to_string()]);
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
    assert!(scene.editor_focused(), "focus is back in the origin editor");

    // The interceptor went with the session.
    scene.keys("ctrl-f");
    assert_eq!(scene.selections(), vec![9..9]);
    assert_eq!(log.borrow().len(), 2);
}

#[gpui::test]
async fn ctrl_slash_after_three_replacements_undoes_them_in_one_step(cx: &mut TestAppContext) {
    let original = "foo foo foo foo\n";
    let mut scene = asking(original, cx).await;
    scene.keys("y");
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "bar bar bar foo\n");

    scene.keys("ctrl-/");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
}

#[gpui::test]
async fn x_ends_the_session_and_inserts_x_once(cx: &mut TestAppContext) {
    let original = "foo foo foo\n";
    let mut scene = asking(original, cx).await;
    scene.keys("y");

    scene.keys("x");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar foox foo\n");
    assert_eq!(scene.selections(), vec![8..8]);
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);

    // The typing is its own undo step above the session's.
    scene.undo();
    assert_eq!(scene.text(), "bar foo foo\n");
    scene.undo();
    assert_eq!(scene.text(), original);
}

#[gpui::test]
async fn alt_x_ends_the_session_and_opens_the_palette_from_the_editor(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo\n", cx).await;
    let opened = Rc::new(Cell::new(0));
    scene.editor.update(&mut scene.cx, |editor, _| {
        let opened = opened.clone();
        editor
            .register_action(move |_: &Palette, _, _| opened.set(opened.get() + 1))
            .detach();
    });
    let (log, _subscription) = scene.record_actions();

    scene.keys("alt-x");
    assert!(!scene.view_open());
    assert_eq!(opened.get(), 1, "the palette opens once, from the editor");
    assert_eq!(
        *log.borrow(),
        vec!["query_replace_intercept_tests::Palette".to_string()]
    );
    assert_eq!(scene.text(), "foo foo\n");
    assert_eq!(scene.selections(), vec![3..3]);
}

#[gpui::test]
async fn a_key_typed_during_a_batch_is_held_and_runs_once_after_it(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + SCAN_CHUNK / 2;
    let original = "foo\n".repeat(lines);
    let mut scene = asking(&original, cx).await;
    let (log, _subscription) = scene.record_actions();
    scene
        .cx
        .update(|window, cx| window.dispatch_action(All.boxed_clone(), cx));
    let mut ticks = 0;
    while scene.text().matches("bar").count() < SCAN_CHUNK {
        assert!(ticks < 1_000, "the first chunk never landed");
        scene.cx.executor().tick();
        ticks += 1;
    }
    let editor = scene.editor.clone();
    let running = |scene: &mut Scene| {
        scene
            .editor
            .read_with(&scene.cx, |editor, _| batch_running(editor))
    };
    assert!(running(&mut scene));
    let before = scene.selections();

    scene
        .cx
        .update(|window, cx| window.dispatch_keystroke(key("ctrl-f"), cx));
    // Nothing acts on the buffer while it changes.
    assert!(running(&mut scene));
    assert!(scene.view_open());
    assert_eq!(scene.selections(), before);
    assert!(log.borrow().is_empty(), "{:?}", log.borrow());

    scene.cx.run_until_parked();
    assert!(!running(&mut scene));
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar\n".repeat(lines));
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {lines} occurrences")]
    );
    // From the end of the last replacement, one character right, once.
    assert_eq!(scene.selections(), vec![4 * lines..4 * lines]);
    assert_eq!(*log.borrow(), vec!["editor::MoveRight".to_string()]);
    let focused = scene
        .cx
        .update(|window, cx| editor.focus_handle(cx).is_focused(window));
    assert!(focused);
    scene.undo();
    assert_eq!(scene.text(), original);
}

#[gpui::test]
async fn ctrl_g_after_a_held_key_stops_the_batch_and_drops_the_key(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + SCAN_CHUNK / 2;
    let original = "foo\n".repeat(lines);
    let mut scene = asking(&original, cx).await;
    let (log, _subscription) = scene.record_actions();
    scene
        .cx
        .update(|window, cx| window.dispatch_action(All.boxed_clone(), cx));
    let mut ticks = 0;
    while scene.text().matches("bar").count() < SCAN_CHUNK {
        assert!(ticks < 1_000, "the first chunk never landed");
        scene.cx.executor().tick();
        ticks += 1;
    }
    let running = |scene: &mut Scene| {
        scene
            .editor
            .read_with(&scene.cx, |editor, _| batch_running(editor))
    };
    assert!(running(&mut scene));

    // `ctrl-f` is held; `ctrl-g` still reaches the session and stops the batch.
    scene
        .cx
        .update(|window, cx| window.dispatch_keystroke(key("ctrl-f"), cx));
    assert!(running(&mut scene));
    scene
        .cx
        .update(|window, cx| window.dispatch_keystroke(key("ctrl-g"), cx));
    scene.cx.run_until_parked();

    assert!(!running(&mut scene));
    assert!(!scene.view_open());
    let expected = "bar\n".repeat(SCAN_CHUNK) + &"foo\n".repeat(lines - SCAN_CHUNK);
    assert_eq!(scene.text(), expected);
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {SCAN_CHUNK} occurrences (interrupted)")]
    );
    // The held `ctrl-f` is dropped: the cursor stays at the end of the last replacement.
    assert_eq!(
        scene.selections(),
        vec![4 * SCAN_CHUNK - 1..4 * SCAN_CHUNK - 1]
    );
    assert!(
        !log.borrow().iter().any(|name| name == "editor::MoveRight"),
        "{:?}",
        log.borrow()
    );
    assert!(scene.editor_focused());
}

#[gpui::test]
async fn an_exit_after_the_origin_pane_closed(cx: &mut TestAppContext) {
    let original = "foo foo foo\n";
    let mut scene = asking(original, cx).await;
    let counted = Rc::new(Cell::new(0));
    scene.cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new("ctrl-o", Counted, None)]);
        let counted = counted.clone();
        cx.on_action(move |_: &Counted, _| counted.set(counted.get() + 1));
    });
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "bar bar foo\n");
    let buffer = scene.editor.read_with(&scene.cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().unwrap()
    });
    let Scene {
        workspace,
        editor,
        messages,
        mut cx,
    } = scene;
    drop(editor);

    let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(&mut cx, |pane, window, cx| {
        let close = workspace::CloseActiveItem {
            save_intent: Some(workspace::SaveIntent::Skip),
            close_pinned: false,
        };
        pane.close_active_item(&close, window, cx).detach()
    });
    cx.run_until_parked();
    assert!(pane.read_with(&cx, |pane, _| pane.items_len() == 0));

    cx.simulate_keystrokes("ctrl-o");
    cx.run_until_parked();
    assert_eq!(counted.get(), 1, "the key ran once");
    assert_eq!(*messages.borrow(), vec!["Replaced 2 occurrences"]);
    cx.simulate_keystrokes("ctrl-o");
    assert_eq!(counted.get(), 2, "nothing intercepts it any more");

    // The session's edits are one undo step in the buffer.
    let text = |cx: &mut gpui::VisualTestContext| buffer.read_with(cx, |buffer, _| buffer.text());
    assert_eq!(text(&mut cx), "bar bar foo\n");
    buffer.update(&mut cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(text(&mut cx), original);
}
