//! The keystroke interceptor of a session. It sees every key-down of the session's window before
//! gpui matches it against the keymap, and decides, from the bindings the key resolves to in the
//! focused context, whether the key belongs to the session or ends it:
//!
//! 1. The pending keystrokes and the key are resolved against the context stack.
//! 2. When the first binding runs an in-session action ([`IN_SESSION`]), or the keys are a pending
//!    prefix of an in-session binding, the key goes on to the session. The starters are not
//!    in-session actions: in the session's input they would propagate (it is not a full editor)
//!    and gpui would run the next binding of the key instead.
//! 3. Otherwise, with no prefix pending, a printable character with no command modifier (Shift
//!    is fine) goes on to the session as text for the string.
//! 4. Otherwise the key is stopped and the session ends with an implicit exit, which runs the
//!    keys again in the origin editor once focus is back there.
//!
//! While the search string is being edited (M-e) every key goes through.
//!
//! The interceptor lives as long as the session: the session holds its subscription and drops
//! it when it ends.

use std::cell::RefCell;
use std::rc::Weak;

use bongos_minibuffer::SessionView;
use gpui::{
    AnyWindowHandle, App, Focusable as _, KeyContext, Keymap, Keystroke, Subscription, WeakEntity,
    Window,
};
use workspace::Workspace;

use crate::actions::IN_SESSION;
use crate::session::{KEY_CONTEXT, SessionState, exit_implicitly, holds_keys, is_editing};

/// What becomes of a key typed in a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The session handles the key: an in-session command, or text for the string.
    Through,
    /// The session ends, and these keys (the pending ones and the key) run again after it.
    Exit(Vec<Keystroke>),
}

/// Decides what becomes of `key`, typed after `pending`, in the context `stack`.
pub(crate) fn decide(
    keymap: &Keymap,
    pending: &[Keystroke],
    key: &Keystroke,
    stack: &[KeyContext],
) -> Decision {
    let mut input = pending.to_vec();
    input.push(key.clone());
    let (bindings, has_pending) = keymap.bindings_for_input(&input, stack);
    if bindings
        .first()
        .is_some_and(|binding| is_in_session(binding.action().name()))
    {
        return Decision::Through;
    }
    if has_pending && is_in_session_prefix(keymap, &input, stack) {
        return Decision::Through;
    }
    if pending.is_empty() && is_text(key) {
        return Decision::Through;
    }
    Decision::Exit(input)
}

fn is_in_session(name: &str) -> bool {
    IN_SESSION.contains(&name)
}

/// Whether `input` begins a longer binding of an in-session action that holds in `stack`.
fn is_in_session_prefix(keymap: &Keymap, input: &[Keystroke], stack: &[KeyContext]) -> bool {
    keymap.bindings().any(|binding| {
        is_in_session(binding.action().name())
            && binding
                .predicate()
                .is_none_or(|predicate| predicate.depth_of(stack).is_some())
            && binding.match_keystrokes(input) == Some(true)
    })
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

/// Whether focus is in the prompt `workspace` shows, which is the session's while it is open.
/// Another workspace of the window may hold a session of its own, with its own interceptor.
fn in_session_prompt(workspace: &WeakEntity<Workspace>, window: &Window, cx: &App) -> bool {
    workspace.upgrade().is_some_and(|workspace| {
        workspace
            .read(cx)
            .active_modal::<SessionView>(cx)
            .is_some_and(|view| view.focus_handle(cx).contains_focused(window, cx))
    })
}

/// Starts intercepting the keys of `window` for the session `state`, which `workspace` shows.
/// Dropping the subscription stops it.
pub(crate) fn install(
    state: Weak<RefCell<SessionState>>,
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Subscription {
    cx.intercept_keystrokes(move |event, current, cx| {
        // A keystroke without a key names no key (gpui's simulated input sends some); it is left
        // alone.
        if event.keystroke.key.is_empty()
            || current.window_handle() != window
            || !event
                .context_stack
                .iter()
                .any(|context| context.contains(KEY_CONTEXT))
            || !in_session_prompt(&workspace, current, cx)
        {
            return;
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        if is_editing(&state) {
            // The string is edited as any input: every key is the input's.
            return;
        }
        let pending = current
            .pending_input_keystrokes()
            .map(<[Keystroke]>::to_vec)
            .unwrap_or_default();
        let keys = if holds_keys(&state) {
            // The session is already ending on an earlier key: this one runs after it.
            let mut keys = pending;
            keys.push(event.keystroke.clone());
            keys
        } else {
            let decision = decide(
                &cx.key_bindings().borrow(),
                &pending,
                &event.keystroke,
                &event.context_stack,
            );
            match decision {
                Decision::Through => return,
                Decision::Exit(keys) => keys,
            }
        };
        cx.stop_propagation();
        exit_implicitly(&state, keys, current, cx);
    })
}
