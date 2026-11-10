//! Line commands: move-line-up and -down (Zed's own line mover, N times in one undo step),
//! duplicate-line and copy-from-above (through `apply::run`). A single-line editor passes the
//! key on: it has no lines to move, copy or duplicate.

use bongos_emacs_model::{Plan, copy_from_above, duplicate_line};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{CopyFromAbove, DuplicateLine, MoveLineDown, MoveLineUp};
use crate::apply::{self, Command};

/// Whether the key was passed on to the next handler because the editor has one line.
pub(crate) fn propagate_in_single_line(editor: &Entity<Editor>, cx: &mut App) -> bool {
    let single_line = editor.read(cx).mode().is_single_line();
    if single_line {
        cx.propagate();
    }
    single_line
}

/// Moves the lines under every caret `count` lines up (a negative count: down) with Zed's
/// `move_line_up`, all in one transaction so one undo undoes the whole move. Stops early when a
/// move changes nothing (the block is at the edge), so a huge count costs nothing.
fn move_lines(
    editor: &Entity<Editor>,
    count: Option<i32>,
    up: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let count = count.unwrap_or(1);
    let up = up == (count >= 0);
    let times = count.unsigned_abs();
    editor.update(cx, |editor, cx| {
        if times == 0 || editor.read_only(cx) {
            return;
        }
        // Text typed just before the command must not merge into the command's undo step.
        editor.finalize_last_transaction(cx);
        editor.transact(window, cx, |editor, window, cx| {
            for _ in 0..times {
                let before = editor.selections.disjoint_anchors_arc();
                if up {
                    editor.move_line_up(&editor::actions::MoveLineUp, window, cx);
                } else {
                    editor.move_line_down(&editor::actions::MoveLineDown, window, cx);
                }
                if editor.selections.disjoint_anchors_arc() == before {
                    break;
                }
            }
        });
        editor.finalize_last_transaction(cx);
    });
}

pub(crate) fn move_line_up_command(
    action: &MoveLineUp,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    move_lines(editor, action.count, true, window, cx);
}

pub(crate) fn move_line_down_command(
    action: &MoveLineDown,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    move_lines(editor, action.count, false, window, cx);
}

pub(crate) fn duplicate_line_command(
    action: &DuplicateLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()),
        |text, carets| -> Plan { duplicate_line(text, carets, count) },
    );
}

pub(crate) fn copy_from_above_command(
    action: &CopyFromAbove,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()),
        |text, carets| -> Plan { copy_from_above(text, carets, count) },
    );
}
