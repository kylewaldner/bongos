//! The keystroke interceptor of an interactive replace. It sees every key-down of the session's
//! window while focus is in the decision view, before gpui matches it against the keymap, and
//! decides with isearch's key rule ([`decide_with`]) whether the key belongs to the session:
//!
//! - A key whose first binding is a decision action ([`IN_SESSION`]), or that begins one, goes on
//!   to the view.
//! - Any other key, a printable one included (the view takes no text), is stopped and ends the
//!   session as an exit: the edits become one undo step, focus goes back to the origin editor, and
//!   the keys run there once.
//! - While a `!` batch runs only `C-g` goes on; any other key is held and runs in the origin editor
//!   once the batch is over, so nothing acts on the buffer while it changes. A `C-g` that stops
//!   the batch drops the held keys, as Emacs drops typed-ahead input on quit.
//!
//! The interceptor lives as long as the session, which holds its subscription.

use std::rc::Weak;

use bongos_isearch::intercept::{Decision, decide_with};
use gpui::{AnyWindowHandle, App, Keystroke, Subscription};

use crate::actions::IN_SESSION;
use crate::session::{Interactive, exit_implicitly, holds_keys, in_batch, view_focused};
use crate::view::KEY_CONTEXT;

/// The one action a key may run while a `!` batch runs.
const ABORT: &str = "query_replace::Abort";

/// Whether `name` is the session's, in a batch (`batch`) or not.
pub(crate) fn in_session(name: &str, batch: bool) -> bool {
    if batch {
        name == ABORT
    } else {
        IN_SESSION.contains(&name)
    }
}

/// Starts intercepting the keys of `window` for the session `state`. Dropping the subscription
/// stops it.
pub(crate) fn install(
    state: Weak<std::cell::RefCell<Interactive>>,
    window: AnyWindowHandle,
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
        {
            return;
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        if !view_focused(&state, current, cx) {
            return;
        }
        let mut keys = current
            .pending_input_keystrokes()
            .map(<[Keystroke]>::to_vec)
            .unwrap_or_default();
        let batch = in_batch(&state);
        if holds_keys(&state) && !batch {
            // The session is already ending on an earlier key: this one runs after it.
            keys.push(event.keystroke.clone());
        } else {
            // In a batch `C-g` still goes on when keys are held, so it can stop the batch.
            let decision = decide_with(
                &cx.key_bindings().borrow(),
                &keys,
                &event.keystroke,
                &event.context_stack,
                |name| in_session(name, batch),
                false,
            );
            match decision {
                Decision::Through => return,
                Decision::Exit(exit) => keys = exit,
            }
        }
        cx.stop_propagation();
        exit_implicitly(&state, keys, current, cx);
    })
}
