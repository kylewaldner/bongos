//! Goto-line. With a count the caret jumps straight to that line. Without one the command asks
//! for a line in the minibuffer; each change of the text highlights the target row and scrolls
//! it into view without moving the caret. RET moves the caret there and records the jump in
//! the navigation history; C-g, Escape or a replacing prompt clears the highlight and puts the
//! scroll back where it was when the prompt opened. The text is read by the model's
//! `goto::parse` (`42`, `42:7`), so a column is accepted too.

use bongos_emacs_model::goto::{GotoQuery, parse};
use bongos_minibuffer::{Outcome, Session};
use editor::scroll::Autoscroll;
use editor::{Editor, RowHighlightOptions, SelectionEffects};
use gpui::{App, Entity, Focusable as _, WeakEntity, Window};
use multi_buffer::MultiBufferSnapshot;
use text::{Bias, Point};
use theme::ActiveTheme as _;

use crate::actions::GotoLine;
use crate::lines::propagate_in_single_line;
use crate::mark::push_mark;

/// The name the prompt is routed under (`minibuffer.enabled_for`).
const MINIBUFFER_NAME: &str = "goto_line";

/// What the status says when the text is not a line.
const NO_LINE: &str = "no line";

/// Keys the preview highlight, so clearing it leaves other row highlights alone.
pub(crate) enum GotoLinePreview {}

/// The point a 1-based line (and column) stands for, clamped to the buffer: a line below 1 is
/// line 1 and a line past the end is the last one.
fn target(snapshot: &MultiBufferSnapshot, line: usize, column: Option<usize>) -> Point {
    let row = line
        .saturating_sub(1)
        .min(snapshot.max_point().row as usize) as u32;
    let column = column.map_or(0, |column| {
        column.saturating_sub(1).min(u32::MAX as usize) as u32
    });
    snapshot.clip_point(Point::new(row, column), Bias::Left)
}

pub(crate) fn goto_line_command(
    action: &GotoLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    match action.count {
        Some(count) => {
            let line = count.max(1) as usize;
            jump(editor, line, None, window, cx);
        }
        None => ask(editor, window, cx),
    }
}

/// Pushes the mark, then moves the one caret to the line, clears the region and records the
/// jump.
fn jump(
    editor: &Entity<Editor>,
    line: usize,
    column: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    push_mark(editor, None, false, window, cx);
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<GotoLinePreview>();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_before(target(&snapshot, line, column));
        let effects = SelectionEffects::scroll(Autoscroll::center()).nav_history(true);
        editor.change_selections(effects, window, cx, |s| {
            s.select_anchor_ranges([anchor..anchor]);
        });
        editor.set_selection_mark_mode(false, cx);
        cx.notify();
    });
}

/// Shows the target row as the preview, scrolled into view; the caret stays.
fn preview(editor: &Entity<Editor>, line: usize, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<GotoLinePreview>();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let start = target(&snapshot, line, None);
        let mut end = snapshot.clip_point(Point::new(start.row, start.column + 1), Bias::Left);
        if end == start {
            end = snapshot.clip_point(Point::new(start.row + 1, 0), Bias::Left);
        }
        editor.highlight_rows::<GotoLinePreview>(
            snapshot.anchor_before(start)..snapshot.anchor_after(end),
            |cx| cx.theme().colors().editor_highlighted_line_background,
            RowHighlightOptions {
                autoscroll: true,
                ..Default::default()
            },
            cx,
        );
        editor.request_autoscroll(Autoscroll::center(), cx);
        cx.notify();
    });
}

fn ask(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let scroll = editor.update(cx, |editor, cx| editor.scroll_position(cx));
    let on_input: WeakEntity<Editor> = editor.downgrade();
    let on_confirm = editor.downgrade();
    let on_cancel = editor.downgrade();
    let session = Session::string(MINIBUFFER_NAME, "Goto line: ")
        .on_input(move |text, handle, _window, cx| {
            let Some(editor) = on_input.upgrade() else {
                return;
            };
            match parse(text) {
                GotoQuery::Position { line, .. } => {
                    handle.set_status("", cx);
                    preview(&editor, line, cx);
                }
                GotoQuery::Empty => {
                    handle.set_status("", cx);
                    editor.update(cx, |editor, _| {
                        editor.clear_row_highlights::<GotoLinePreview>();
                    });
                }
                GotoQuery::Invalid => {
                    handle.set_status(NO_LINE, cx);
                    editor.update(cx, |editor, _| {
                        editor.clear_row_highlights::<GotoLinePreview>();
                    });
                }
            }
        })
        .on_confirm(move |outcome, window, cx| {
            let Some(editor) = on_confirm.upgrade() else {
                return;
            };
            window.focus(&editor.focus_handle(cx), cx);
            let query = match outcome {
                Outcome::Input(text) => parse(&text),
                Outcome::Candidate(_) => GotoQuery::Invalid,
            };
            match query {
                GotoQuery::Position { line, column } => jump(&editor, line, column, window, cx),
                // Nothing to go to: the scroll goes back, the caret never moved.
                _ => restore(&editor, scroll, window, cx),
            }
        })
        .on_cancel(move |window, cx| {
            let Some(editor) = on_cancel.upgrade() else {
                return;
            };
            restore(&editor, scroll, window, cx);
        });
    workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx);
    });
}

/// Clears the preview and puts the scroll back where the prompt found it.
fn restore(editor: &Entity<Editor>, scroll: gpui::Point<f64>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<GotoLinePreview>();
        editor.set_scroll_position(scroll, window, cx);
        cx.notify();
    });
}
