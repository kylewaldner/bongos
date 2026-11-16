//! Repeating an action that does not read the prefix argument. For now the action runs once and
//! the argument is consumed; running it as many times as the argument says replaces this.

use bongos_emacs_model::prefix_arg::PrefixArg;
use gpui::{Action, App, Window};

use crate::prefix_arg;

/// Runs `action` for the argument `arg`, on the focused element, and clears the argument.
pub(crate) fn start(window: &mut Window, cx: &mut App, action: &dyn Action, arg: PrefixArg) {
    let _ = arg;
    prefix_arg::cancel(window, cx);
    if let Some(focused) = window.focused(cx) {
        focused.dispatch_action(action, window, cx);
    }
}
