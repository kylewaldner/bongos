//! The key interceptor of the prefix argument. It sees every key-down before gpui matches it
//! against the keymap and, while a window has an argument pending, decides what the key does
//! with it (`decide`), in this order:
//!
//! 1. Focus is not in the editor the argument was started in: the argument is cleared and the
//!    key acts normally.
//! 2. The key runs a command that builds the argument (C-u, M-digit, M--) or quits: it goes on
//!    untouched; those commands handle the argument themselves.
//! 3. A digit (or a `-` before any digit) typed with no modifier but Shift, while the argument
//!    accepts it, extends the argument; the key is stopped.
//! 4. The keys are a pending prefix of a longer binding: the key goes on and the argument waits
//!    for the binding to complete.
//! 5. A binding resolved: its action's `Meaning` routes it.
//! 6. A typed character with no binding is inserted as many times as the argument says, as one
//!    edit.
//! 7. Anything else clears the argument and acts normally.

use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::Editor;
use gpui::{
    Action, App, Entity, Focusable as _, KeyContext, Keymap, Keystroke, Subscription, Window,
};

use crate::actions::{DigitArgument, KeyboardQuit, NegativeArgument, UniversalArgument};
use crate::prefix_arg::{self, MAX_INSERT, Meaning};
use crate::repeat;

/// The action that ends a running query-replace batch. It runs the quit hooks, which clear the
/// argument.
const QUERY_REPLACE_ABORT: &str = "query_replace::Abort";

/// What becomes of a key typed while an argument is pending.
#[derive(Debug)]
pub(crate) enum Route {
    /// The key goes on and the argument is left alone: a command that builds the argument or
    /// quits, or a key that only extends a pending chord.
    Through,
    /// The argument is cleared and the key goes on.
    Cancel,
    /// The argument is handed to the command the key runs, and the key goes on.
    HandOff,
    /// The key is stopped and the argument becomes this one.
    Press(PrefixArg),
    /// The key is stopped and the adapter is called once with the argument.
    Adapter(fn(PrefixArg, &Entity<Editor>, &mut Window, &mut App)),
    /// The key is stopped and its action runs once, rebuilt with the argument as its count.
    CountField(Box<dyn Action>),
    /// The key is stopped and the named action runs once instead.
    Redirect(&'static str),
    /// The key is stopped and its action is repeated by the argument.
    Repeat(Box<dyn Action>),
    /// The key is stopped and its text is inserted as many times as the argument says.
    Type(String),
}

/// Decides what becomes of `key`, typed after the pending chord `pending` in the context
/// `stack`, while `arg` is pending. `in_origin` says whether focus is in the editor `arg` was
/// started in; `meaning` gives what an action, by name, does with an argument.
pub(crate) fn decide(
    keymap: &Keymap,
    pending: &[Keystroke],
    key: &Keystroke,
    stack: &[KeyContext],
    arg: PrefixArg,
    in_origin: bool,
    meaning: impl Fn(&str) -> Option<Meaning>,
) -> Route {
    if !in_origin {
        return Route::Cancel;
    }
    let mut input = pending.to_vec();
    input.push(key.clone());
    let (bindings, has_pending) = keymap.bindings_for_input(&input, stack);
    let first = bindings.first().map(|binding| binding.action());
    if first.is_some_and(|action| handles_argument(action.name())) {
        return Route::Through;
    }
    if pending.is_empty()
        && let Some(pressed) = press(arg, key)
    {
        return Route::Press(pressed);
    }
    // A digit or `-` the argument no longer accepts (after C-u ended the number) leaves the
    // argument as it was and is an ordinary key: typed, it inserts its character as many times
    // as the argument says, as in Emacs.
    if has_pending && first.is_none() {
        return Route::Through;
    }
    if let Some(action) = first {
        return match meaning(action.name()) {
            Some(Meaning::Reads) => Route::HandOff,
            Some(Meaning::Ignored) => Route::Cancel,
            Some(Meaning::Adapter(adapter)) => Route::Adapter(adapter),
            Some(Meaning::CountField) => Route::CountField(action.boxed_clone()),
            Some(Meaning::Redirect(target)) => Route::Redirect(target),
            None => Route::Repeat(action.boxed_clone()),
        };
    }
    if pending.is_empty()
        && is_text(key)
        && let Some(text) = &key.key_char
    {
        return Route::Type(text.clone());
    }
    Route::Cancel
}

/// Whether the action named `name` builds or quits the argument itself.
fn handles_argument(name: &str) -> bool {
    name == UniversalArgument::name_for_type()
        || name == DigitArgument::name_for_type()
        || name == NegativeArgument::name_for_type()
        || name == KeyboardQuit::name_for_type()
        || name == QUERY_REPLACE_ABORT
}

/// The argument after `key` extends it: a digit, or a `-`, typed with no modifier but Shift and
/// accepted by `arg`. `None` when the key does not extend it.
fn press(mut arg: PrefixArg, key: &Keystroke) -> Option<PrefixArg> {
    let modifiers = &key.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return None;
    }
    let mut chars = key.key_char.as_deref()?.chars();
    let (Some(char), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let accepted = match char {
        '-' => arg.press_minus(),
        digit => digit
            .to_digit(10)
            .is_some_and(|digit| arg.press_digit(digit as u8)),
    };
    accepted.then_some(arg)
}

/// A printable character, with or without Shift, and no other modifier. Tab and Enter carry a
/// control character and are not text.
fn is_text(key: &Keystroke) -> bool {
    let modifiers = &key.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return false;
    }
    key.key_char
        .as_deref()
        .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

/// Installs the interceptor for every window. It does nothing in a window with no argument
/// pending, beyond clearing a "Count limited" echo on the next key.
pub(crate) fn install(cx: &mut App) -> Subscription {
    cx.intercept_keystrokes(|event, window, cx| {
        // A keystroke without a key names no key (gpui's simulated input sends some).
        if event.keystroke.key.is_empty() {
            return;
        }
        prefix_arg::clear_limited(window, cx);
        let Some(arg) = prefix_arg::pending(window, cx) else {
            return;
        };
        let origin = prefix_arg::origin(window, cx);
        let in_origin = origin
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
        let pending = window
            .pending_input_keystrokes()
            .map(<[Keystroke]>::to_vec)
            .unwrap_or_default();
        let keymap = cx.key_bindings();
        let route = decide(
            &keymap.borrow(),
            &pending,
            &event.keystroke,
            &event.context_stack,
            arg,
            in_origin,
            |name| prefix_arg::meaning(cx, name),
        );
        let Some(origin) = origin.filter(|_| in_origin) else {
            prefix_arg::cancel(window, cx);
            return;
        };
        match route {
            Route::Through => return,
            Route::Cancel => {
                prefix_arg::cancel(window, cx);
                return;
            }
            Route::HandOff => {
                prefix_arg::hand_off(window, cx);
                return;
            }
            Route::Press(pressed) => prefix_arg::start(pressed, &origin, window, cx),
            Route::Adapter(adapter) => {
                prefix_arg::cancel(window, cx);
                adapter(arg, &origin, window, cx);
            }
            Route::CountField(action) => {
                let data = serde_json::json!({ "count": arg.value() });
                match cx.build_action(action.name(), Some(data)) {
                    Ok(counted) => {
                        prefix_arg::cancel(window, cx);
                        dispatch(counted.as_ref(), window, cx);
                    }
                    Err(error) => {
                        log::error!("{} takes no count: {error}", action.name());
                        repeat::start(window, cx, action.as_ref(), arg);
                    }
                }
            }
            Route::Redirect(target) => {
                prefix_arg::cancel(window, cx);
                match cx.build_action(target, None) {
                    Ok(action) => dispatch(action.as_ref(), window, cx),
                    Err(error) => log::error!("cannot redirect to {target}: {error}"),
                }
            }
            Route::Repeat(action) => repeat::start(window, cx, action.as_ref(), arg),
            Route::Type(text) => type_text(&text, arg, &origin, window, cx),
        }
        // The key is consumed here, so the chord it completed is no longer pending. Dispatching
        // an action lets events propagate again, so the key is stopped last.
        window.discard_pending_keystrokes(cx);
        cx.stop_propagation();
    })
}

/// Dispatches `action` on the focused element.
fn dispatch(action: &dyn Action, window: &mut Window, cx: &mut App) {
    if let Some(focused) = window.focused(cx) {
        focused.dispatch_action(action, window, cx);
    }
}

/// Inserts `text` as many times as `arg` says, clamped to `MAX_INSERT`, as one edit and one undo
/// step, and consumes the argument. A count of zero or less, or a read-only editor, inserts
/// nothing.
fn type_text(
    text: &str,
    arg: PrefixArg,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    prefix_arg::cancel(window, cx);
    let count = arg.value();
    if count <= 0 || editor.read(cx).read_only(cx) {
        return;
    }
    let clamped = count > MAX_INSERT;
    let repeated = text.repeat(count.min(MAX_INSERT) as usize);
    editor.update(cx, |editor, cx| {
        editor.transact(window, cx, |editor, window, cx| {
            editor.handle_input(&repeated, window, cx);
        });
        editor.finalize_last_transaction(cx);
    });
    if clamped {
        prefix_arg::show_limited(window, cx);
    }
}
