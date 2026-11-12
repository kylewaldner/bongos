//! Transpose commands: chars, words (both directions) and lines. Every one goes through
//! `apply::run`; the no-mark forms only, so a region swaps with the next or previous word and
//! is cleared by the other commands.

use bongos_emacs_model::{
    Plan, transpose_chars, transpose_lines, transpose_lines_with_mark, transpose_words,
    transpose_words_reverse, transpose_words_with_mark,
};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};
use multi_buffer::ToOffset as _;

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
transpose_command!(
    transpose_words_reverse_command,
    TransposeWordsReverse,
    transpose_words_reverse
);

/// The mark's offset when a transpose takes its mark form: the buffer has a mark away from
/// point, there is exactly one caret with no active region, and the count is none or 0.
fn mark_form_offset(editor: &Entity<Editor>, count: Option<i32>, cx: &mut App) -> Option<usize> {
    if !matches!(count, None | Some(0)) {
        return None;
    }
    let mark = crate::mark::mark_for(editor, cx)?;
    let editor = editor.read(cx);
    let selections = editor.selections.disjoint_anchors();
    let [selection] = selections else {
        return None;
    };
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let head = selection.head().to_offset(&snapshot).0;
    let tail = selection.tail().to_offset(&snapshot).0;
    if head != tail {
        return None;
    }
    let mark = mark.to_offset(&snapshot).0;
    // A mark at point has nothing to swap with: the plain form runs.
    (mark != head).then_some(mark)
}

macro_rules! transpose_with_mark {
    ($handler:ident, $action:ident, $model:ident, $with_mark:ident) => {
        pub(crate) fn $handler(
            action: &$action,
            editor: &Entity<Editor>,
            window: &mut Window,
            cx: &mut App,
        ) {
            let count = action.count;
            let mark = mark_form_offset(editor, count, cx);
            let applied = apply::run(
                editor,
                window,
                cx,
                Command::edit(action.name()),
                |text, carets| -> Plan {
                    match (mark, carets) {
                        (Some(mark), [caret]) => $with_mark(text, caret.head, mark),
                        _ => $model(text, carets, count),
                    }
                },
            );
            if !applied && mark.is_some() {
                bongos_minibuffer::message(window, cx, "Don't have two things to transpose");
            }
        }
    };
}

transpose_with_mark!(
    transpose_words_command,
    TransposeWords,
    transpose_words,
    transpose_words_with_mark
);
transpose_with_mark!(
    transpose_lines_command,
    TransposeLines,
    transpose_lines,
    transpose_lines_with_mark
);
