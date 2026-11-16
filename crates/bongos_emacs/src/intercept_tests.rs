//! Tests of the prefix argument's key interceptor: the key rule as a table over a synthetic
//! keymap, and the keys typed in a window with an argument pending, end to end.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::Editor;
use gpui::{
    Action, AppContext as _, Entity, Focusable as _, KeyBinding, KeyContext, Keymap, Keystroke,
    TestAppContext, VisualTestContext, Window, actions,
};
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

use crate::actions::{
    BackwardWord, DigitArgument, ExchangePointAndMark, ForwardWord, KeyboardQuit,
    NegativeArgument, UniversalArgument,
};
use crate::intercept::{Route, decide};
use crate::prefix_arg::{self, MAX_INSERT, Meaning};
use crate::prefix_arg_tests::{echo, route_echo};
use crate::quit_tests::workspace_window_with;

actions!(intercept_tests, [Adapted, Redirected, Target, Plain]);

/// An action whose count is filled in from the argument.
#[derive(PartialEq, Clone, Debug, Default, Deserialize, JsonSchema, Action)]
#[action(namespace = intercept_tests)]
#[serde(deny_unknown_fields)]
struct Counted {
    #[serde(default)]
    count: Option<i32>,
}

/// The argument the adapter of `Adapted` last saw.
static ADAPTED: AtomicI32 = AtomicI32::new(0);

fn adapter(arg: PrefixArg, _: &Entity<Editor>, _: &mut Window, _: &mut gpui::App) {
    ADAPTED.store(arg.value(), Ordering::SeqCst);
}

fn key(text: &str) -> Keystroke {
    Keystroke::parse(text).unwrap().with_simulated_ime()
}

fn bindings() -> Vec<KeyBinding> {
    let editor = Some("Editor");
    vec![
        KeyBinding::new("ctrl-u", UniversalArgument, editor),
        KeyBinding::new("alt-3", DigitArgument { digit: 3 }, editor),
        KeyBinding::new("alt--", NegativeArgument, editor),
        KeyBinding::new("alt-f", ForwardWord::default(), editor),
        KeyBinding::new("alt-b", BackwardWord::default(), editor),
        KeyBinding::new("ctrl-x ctrl-x", ExchangePointAndMark, editor),
        KeyBinding::new("ctrl-o", Counted::default(), editor),
        KeyBinding::new("ctrl-t", Redirected, editor),
        KeyBinding::new("ctrl-a", Adapted, editor),
        KeyBinding::new("ctrl-p", Plain, editor),
    ]
}

fn declare_test_meanings(cx: &mut gpui::App) {
    prefix_arg::declare(cx, Counted::name_for_type(), Meaning::CountField);
    prefix_arg::declare(
        cx,
        Redirected::name_for_type(),
        Meaning::Redirect(Target::name_for_type()),
    );
    prefix_arg::declare(cx, Adapted::name_for_type(), Meaning::Adapter(adapter));
}

fn test_meaning(name: &str) -> Option<Meaning> {
    if name == ForwardWord::name_for_type() || name == BackwardWord::name_for_type() {
        Some(Meaning::Reads)
    } else if name == ExchangePointAndMark::name_for_type() {
        Some(Meaning::Ignored)
    } else if name == Counted::name_for_type() {
        Some(Meaning::CountField)
    } else if name == Redirected::name_for_type() {
        Some(Meaning::Redirect(Target::name_for_type()))
    } else if name == Adapted::name_for_type() {
        Some(Meaning::Adapter(adapter))
    } else {
        None
    }
}

fn route(pending: &str, typed: &str, arg: PrefixArg, in_origin: bool) -> Route {
    let mut rows = bindings();
    rows.push(KeyBinding::new("ctrl-g", KeyboardQuit, Some("Editor")));
    let keymap = Keymap::new(rows);
    let stack = [
        KeyContext::parse("Workspace").unwrap(),
        KeyContext::parse("Editor").unwrap(),
    ];
    let pending: Vec<Keystroke> = pending.split_whitespace().map(key).collect();
    decide(
        &keymap,
        &pending,
        &key(typed),
        &stack,
        arg,
        in_origin,
        test_meaning,
    )
}

/// C-u, then C-u again: the number ended and accepts no more digits.
fn ended() -> PrefixArg {
    let mut arg = PrefixArg::universal();
    arg.press_digit(3);
    arg.press_universal();
    arg
}

#[test]
fn decide_routes_every_rule() {
    let universal = PrefixArg::universal();

    // Focus is not in the editor the argument belongs to.
    for typed in ["7", "a", "alt-f", "ctrl-u"] {
        assert!(
            matches!(route("", typed, universal, false), Route::Cancel),
            "{typed}"
        );
    }

    // The commands that build or quit the argument handle it themselves.
    for typed in ["ctrl-u", "alt-3", "alt--", "ctrl-g"] {
        assert!(
            matches!(route("", typed, universal, true), Route::Through),
            "{typed}"
        );
    }

    // Digits and `-` extend the argument while it accepts them.
    assert!(matches!(
        route("", "7", universal, true),
        Route::Press(arg) if arg.value() == 7 && arg.echo() == "C-u 7"
    ));
    assert!(matches!(
        route("", "-", universal, true),
        Route::Press(arg) if arg.value() == -4
    ));
    // A `-` after a digit, and a digit after the number ended, are typed characters.
    assert!(matches!(
        route("", "-", PrefixArg::digit(2), true),
        Route::Type(text) if text == "-"
    ));
    assert!(matches!(
        route("", "7", ended(), true),
        Route::Type(text) if text == "7"
    ));

    // A pending prefix keeps the argument for the binding it completes.
    assert!(matches!(
        route("", "ctrl-x", universal, true),
        Route::Through
    ));
    // A digit inside a chord does not extend the argument.
    assert!(matches!(route("ctrl-x", "7", universal, true), Route::Cancel));

    // A resolved binding is routed by its action's meaning.
    assert!(matches!(
        route("", "alt-f", universal, true),
        Route::HandOff
    ));
    assert!(matches!(
        route("ctrl-x", "ctrl-x", universal, true),
        Route::Cancel
    ));
    assert!(matches!(
        route("", "ctrl-a", universal, true),
        Route::Adapter(_)
    ));
    assert!(matches!(
        route("", "ctrl-o", universal, true),
        Route::CountField(action) if action.name() == Counted::name_for_type()
    ));
    assert!(matches!(
        route("", "ctrl-t", universal, true),
        Route::Redirect(target) if target == Target::name_for_type()
    ));
    assert!(matches!(
        route("", "ctrl-p", universal, true),
        Route::Repeat(action) if action.name() == Plain::name_for_type()
    ));

    // A typed character, with or without Shift.
    assert!(matches!(
        route("", "a", universal, true),
        Route::Type(text) if text == "a"
    ));
    assert!(matches!(
        route("", "shift-a", universal, true),
        Route::Type(text) if text == "A"
    ));

    // Anything else clears the argument.
    for typed in ["enter", "tab", "ctrl-z", "alt-z"] {
        assert!(
            matches!(route("", typed, universal, true), Route::Cancel),
            "{typed}"
        );
    }
}

/// `count` words, `w0 w1 ...`.
fn words(count: usize) -> String {
    (0..count)
        .map(|index| format!("w{index}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A workspace window with the test keys bound, the echo routed and `text` in the focused
/// editor, with the cursor at its start.
async fn window_with(
    cx: &mut TestAppContext,
    text: &str,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let (workspace, editor, mut cx) = workspace_window_with(cx, |cx| {
        route_echo(cx);
        cx.bind_keys(bindings());
        declare_test_meanings(cx);
    })
    .await;
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.set_text(text, window, cx);
        editor.move_to_beginning(&editor::actions::MoveToBeginning, window, cx);
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn pending(cx: &mut VisualTestContext) -> Option<PrefixArg> {
    cx.update(|window, cx| prefix_arg::pending(window, cx))
}

/// Counts the runs of action `A` in `editor`.
fn count_runs<A: Action>(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Rc<Cell<usize>> {
    let runs = Rc::new(Cell::new(0));
    let seen = runs.clone();
    editor.update(cx, |editor, _| {
        editor
            .register_action(move |_: &A, _, _| seen.set(seen.get() + 1))
            .detach();
    });
    runs
}

#[gpui::test]
async fn digits_build_the_count_and_echo_it(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, &words(20)).await;
    cx.simulate_keystrokes("ctrl-u");
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 4"));
    cx.simulate_keystrokes("1");
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 1"));
    cx.simulate_keystrokes("0");
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 10"));
    assert_eq!(text(&editor, &mut cx), words(20));
    cx.simulate_keystrokes("alt-f");
    assert_eq!(pending(&mut cx), None);
    assert_eq!(echo(&mut cx), None);
    cx.simulate_keystrokes("|");
    assert!(text(&editor, &mut cx).starts_with("w0 w1 w2 w3 w4 w5 w6 w7 w8 w9| w10"));
}

#[gpui::test]
async fn digits_insert_normally_with_nothing_pending(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "").await;
    cx.simulate_keystrokes("7");
    assert_eq!(text(&editor, &mut cx), "7");
}

#[gpui::test]
async fn a_digit_after_the_number_ended_is_typed_by_the_count(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "").await;
    cx.simulate_keystrokes("ctrl-u 3 ctrl-u 7");
    assert_eq!(text(&editor, &mut cx), "777");
    assert_eq!(pending(&mut cx), None);
    // One edit: one undo removes all of it.
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.undo(&editor::actions::Undo, window, cx)
    });
    assert_eq!(text(&editor, &mut cx), "");
}

#[gpui::test]
async fn two_digits_move_that_many_words(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, &words(40)).await;
    cx.simulate_keystrokes("ctrl-u 3 7 alt-f |");
    let expected = format!("{}| w37 w38 w39", words(37));
    assert_eq!(text(&editor, &mut cx), expected);
}

#[gpui::test]
async fn a_digit_alone_is_pending_and_types_nothing(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    cx.simulate_keystrokes("ctrl-u 1");
    assert_eq!(text(&editor, &mut cx), "one");
    assert_eq!(pending(&mut cx).map(|arg| arg.value()), Some(1));
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 1"));
}

#[gpui::test]
async fn universal_minus_moves_four_words_back(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, &words(10)).await;
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
    });
    cx.simulate_keystrokes("ctrl-u -");
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u -"));
    cx.simulate_keystrokes("alt-f |");
    assert_eq!(text(&editor, &mut cx), "w0 w1 w2 w3 w4 w5 |w6 w7 w8 w9");
}

#[gpui::test]
async fn a_chord_after_the_argument_runs_once(cx: &mut TestAppContext) {
    let (_workspace, _editor, mut cx) = window_with(cx, "one two").await;
    let names = Rc::new(RefCell::new(Vec::new()));
    let log = names.clone();
    let _subscription = cx.update(|_, cx| {
        cx.observe_keystrokes(move |event, _, _| {
            if let Some(action) = &event.action {
                log.borrow_mut().push(action.name().to_string());
            }
        })
    });
    cx.simulate_keystrokes("ctrl-u ctrl-x");
    assert!(pending(&mut cx).is_some());
    cx.simulate_keystrokes("ctrl-x");
    let runs = names
        .borrow()
        .iter()
        .filter(|name| name.as_str() == ExchangePointAndMark::name_for_type())
        .count();
    assert_eq!(runs, 1);
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn a_key_in_another_editor_clears_the_argument(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = window_with(cx, "one").await;
    let other = workspace.update_in(&mut cx, |workspace, window, cx| {
        let other = cx.new(|cx| Editor::multi_line(window, cx));
        workspace.add_item_to_active_pane(Box::new(other.clone()), None, true, window, cx);
        window.focus(&other.focus_handle(cx), cx);
        other
    });
    cx.run_until_parked();
    // The argument belongs to the first editor while focus is in the other one.
    cx.update(|window, cx| prefix_arg::start(PrefixArg::universal(), &editor, window, cx));
    cx.simulate_keystrokes("a");
    assert_eq!(pending(&mut cx), None);
    assert_eq!(text(&other, &mut cx), "a");
    assert_eq!(text(&editor, &mut cx), "one");
}

#[gpui::test]
async fn a_zero_count_types_nothing(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    cx.simulate_keystrokes("ctrl-u 0 a");
    assert_eq!(text(&editor, &mut cx), "one");
    assert_eq!(pending(&mut cx), None);
    cx.simulate_keystrokes("a");
    assert_eq!(text(&editor, &mut cx), "aone");
}

#[gpui::test]
async fn a_read_only_editor_types_nothing(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    editor.update(&mut cx, |editor, _| editor.set_read_only(true));
    cx.simulate_keystrokes("ctrl-u a");
    assert_eq!(text(&editor, &mut cx), "one");
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn a_redirect_runs_its_target_once(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    let targets = count_runs::<Target>(&editor, &mut cx);
    let sources = count_runs::<Redirected>(&editor, &mut cx);
    cx.simulate_keystrokes("ctrl-u ctrl-t");
    assert_eq!(targets.get(), 1);
    assert_eq!(sources.get(), 0);
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn a_count_field_action_receives_the_count(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    editor.update(&mut cx, |editor, _| {
        editor
            .register_action(move |action: &Counted, _, _| log.borrow_mut().push(action.count))
            .detach();
    });
    cx.simulate_keystrokes("ctrl-u 5 ctrl-o");
    assert_eq!(*seen.borrow(), vec![Some(5)]);
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn an_adapter_is_called_once_with_the_argument(cx: &mut TestAppContext) {
    let (_workspace, _editor, mut cx) = window_with(cx, "one").await;
    cx.simulate_keystrokes("ctrl-u 6 ctrl-a");
    assert_eq!(ADAPTED.load(Ordering::SeqCst), 6);
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn an_undeclared_action_runs_once_and_consumes_the_argument(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "one").await;
    let runs = count_runs::<Plain>(&editor, &mut cx);
    cx.simulate_keystrokes("ctrl-u ctrl-p");
    assert_eq!(runs.get(), 1);
    assert_eq!(pending(&mut cx), None);
    assert_eq!(echo(&mut cx), None);
}

#[gpui::test]
async fn the_limited_echo_clears_on_the_next_key(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window_with(cx, "").await;
    cx.simulate_keystrokes("ctrl-u 7 0 0 0 0 a");
    assert_eq!(text(&editor, &mut cx).len(), MAX_INSERT as usize);
    assert_eq!(
        echo(&mut cx),
        Some(format!("Count limited to {MAX_INSERT}"))
    );
    cx.simulate_keystrokes("b");
    assert_eq!(echo(&mut cx), None);
    assert_eq!(text(&editor, &mut cx).len(), MAX_INSERT as usize + 1);
}
