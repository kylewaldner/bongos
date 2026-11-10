//! Transpose commands: chars, words (both directions) and lines. Every one goes through
//! `apply::run`; the no-mark forms only, so a region swaps with the next or previous word and
//! is cleared by the other commands.

use bongos_emacs_model::{
    Plan, transpose_chars, transpose_lines, transpose_words, transpose_words_reverse,
};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{TransposeChars, TransposeLines, TransposeWords, TransposeWordsReverse};
use crate::apply::{self, Command};

macro_rules! transpose_command {
    ($handler:ident, $action:ident, $model:ident) => {
        pub(crate) fn $handler(
            action: &$action,
            editor: &Entity<Editor>,
            window: &mut Window,
            cx: &mut App,
        ) {
            let count = action.count;
            apply::run(
                editor,
                window,
                cx,
                Command::edit(action.name()),
                |text, carets| -> Plan { $model(text, carets, count) },
            );
        }
    };
}

transpose_command!(transpose_chars_command, TransposeChars, transpose_chars);
transpose_command!(transpose_words_command, TransposeWords, transpose_words);
transpose_command!(
    transpose_words_reverse_command,
    TransposeWordsReverse,
    transpose_words_reverse
);
transpose_command!(transpose_lines_command, TransposeLines, transpose_lines);
