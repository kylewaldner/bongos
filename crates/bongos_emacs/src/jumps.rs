//! The jump commands that push the mark before they move: beginning and end of the buffer and
//! the xref lookups. With mark mode on, `push_mark` does nothing and the jump extends the
//! region. A jump into another file records its origin in the pane's navigation history, which
//! pop-global-mark reads.

use editor::Editor;
use editor::actions::{
    GoToDefinition, GoToImplementation, MoveToBeginning, MoveToEnd, SelectToBeginning, SelectToEnd,
};
use gpui::{App, Entity, TaskExt as _, Window};

use crate::actions::{
    BeginningOfBuffer, EndOfBuffer, XrefFindDefinitions, XrefFindImplementations,
};
use crate::lines::propagate_in_single_line;
use crate::mark::push_mark;

pub(crate) fn beginning_of_buffer_command(
    _: &BeginningOfBuffer,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let extend = editor.read(cx).selection_mark_mode();
    push_mark(editor, None, false, window, cx);
    editor.update(cx, |editor, cx| {
        if extend {
            editor.select_to_beginning(&SelectToBeginning, window, cx);
        } else {
            editor.move_to_beginning(&MoveToBeginning, window, cx);
        }
    });
}

pub(crate) fn end_of_buffer_command(
    _: &EndOfBuffer,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let extend = editor.read(cx).selection_mark_mode();
    push_mark(editor, None, false, window, cx);
    editor.update(cx, |editor, cx| {
        if extend {
            editor.select_to_end(&SelectToEnd, window, cx);
        } else {
            editor.move_to_end(&MoveToEnd, window, cx);
        }
    });
}

pub(crate) fn xref_find_definitions_command(
    _: &XrefFindDefinitions,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    push_mark(editor, None, false, window, cx);
    editor
        .update(cx, |editor, cx| {
            editor.go_to_definition(&GoToDefinition, window, cx)
        })
        .detach_and_log_err(cx);
}

pub(crate) fn xref_find_implementations_command(
    _: &XrefFindImplementations,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    push_mark(editor, None, false, window, cx);
    editor
        .update(cx, |editor, cx| {
            editor.go_to_implementation(&GoToImplementation, window, cx)
        })
        .detach_and_log_err(cx);
}
