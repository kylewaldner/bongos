//! C-u, M-digit and M--: the commands that build the prefix argument. They run in full editors
//! only; a single-line editor propagates the key.

use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::Editor;
use gpui::{App, Entity, Window};

use crate::actions::{DigitArgument, NegativeArgument, UniversalArgument};
use crate::prefix_arg;

pub(crate) fn universal_argument_command(
    _: &UniversalArgument,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    prefix_arg::press(editor, window, cx, PrefixArg::universal, |arg| {
        arg.press_universal()
    });
}

pub(crate) fn digit_argument_command(
    action: &DigitArgument,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let digit = action.digit;
    prefix_arg::press(
        editor,
        window,
        cx,
        || PrefixArg::digit(digit),
        |arg| arg.press_digit(digit),
    );
}

pub(crate) fn negative_argument_command(
    _: &NegativeArgument,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    prefix_arg::press(editor, window, cx, PrefixArg::negative, |arg| {
        arg.negative_argument();
        true
    });
}
