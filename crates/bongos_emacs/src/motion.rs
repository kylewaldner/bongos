//! Word motions: forward-word and backward-word, with a count and an optional region.

use bongos_emacs_model::{Plan, backward_word_carets, forward_word_carets};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{BackwardWord, ForwardWord};
use crate::apply::{self, Command};

pub(crate) fn forward_word(
    action: &ForwardWord,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let (count, select) = (action.count, action.select);
    apply::run(
        editor,
        window,
        cx,
        Command::motion(action.name()),
        |text, carets| Plan {
            carets: forward_word_carets(text, carets, count, select),
            ..Plan::default()
        },
    );
}

pub(crate) fn backward_word(
    action: &BackwardWord,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let (count, select) = (action.count, action.select);
    apply::run(
        editor,
        window,
        cx,
        Command::motion(action.name()),
        |text, carets| Plan {
            carets: backward_word_carets(text, carets, count, select),
            ..Plan::default()
        },
    );
}
