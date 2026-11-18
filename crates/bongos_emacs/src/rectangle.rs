//! The rectangle commands: the rectangle between the mark and the point, and the one path that
//! applies a rectangle plan to the editor. The row plans live in
//! `bongos_emacs_model::rectangle_ops`.

use bongos_emacs_model::rectangle::{Columns, line_width};
use bongos_emacs_model::rectangle_ops::{self, RectOp, RowText};
use editor::{Editor, SelectionEffects};
use gpui::{App, Entity, Window};
use language::BufferSnapshot;
use language::language_settings::LanguageSettings;
use multi_buffer::{MultiBufferOffset, ToOffset as _};
use text::{Point, ToOffset as _};

use crate::actions::{
    ClearRectangle, CopyRectangleAsKill, DeleteRectangle, KeepRectangle, KillRectangle,
    OpenRectangle, RectangleMarkMode, YankRectangle,
};
use crate::mark;

/// The rectangle between the mark and the point: its buffer rows, top first, and its columns.
pub(crate) struct Corners {
    /// The buffer the rectangle is in.
    pub buffer: BufferSnapshot,
    /// Every buffer line from the upper corner's to the lower corner's, without newlines.
    pub rows: Vec<RowText>,
    /// The display columns between the two corners.
    pub columns: Columns,
    /// The buffer's tab size.
    pub tab: usize,
}

/// The rectangle with the point (the newest selection's head) and the mark at opposite
/// corners. Rows are buffer lines; columns are display columns. Says why and returns `None`
/// when there is no mark or the mark is in another excerpt than the point.
pub(crate) fn corners(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Corners> {
    let Some(mark) = mark::mark_for(editor, cx) else {
        bongos_minibuffer::message(window, cx, "The mark is not set");
        return None;
    };
    let (snapshot, point) = {
        let editor = editor.read(cx);
        (
            editor.buffer().read(cx).snapshot(cx),
            editor.selections.newest_anchor().head(),
        )
    };
    let point_offset = point.to_offset(&snapshot);
    let mark_offset = mark.to_offset(&snapshot);
    if snapshot
        .excerpt_containing(point_offset.min(mark_offset)..point_offset.max(mark_offset))
        .is_none()
    {
        bongos_minibuffer::message(window, cx, "The rectangle spans more than one excerpt");
        return None;
    }
    let (point_anchor, buffer) = snapshot.anchor_to_buffer_anchor(point)?;
    let (mark_anchor, _) = snapshot.anchor_to_buffer_anchor(mark)?;
    let buffer = buffer.clone();
    let point = buffer.offset_to_point(point_anchor.to_offset(&buffer));
    let mark = buffer.offset_to_point(mark_anchor.to_offset(&buffer));
    let tab = LanguageSettings::for_buffer_snapshot(&buffer, None, cx)
        .tab_size
        .get() as usize;
    let rows: Vec<RowText> = (point.row.min(mark.row)..=point.row.max(mark.row))
        .map(|row| {
            let start = buffer.point_to_offset(Point::new(row, 0));
            let end = buffer.point_to_offset(Point::new(row, buffer.line_len(row)));
            RowText {
                start,
                text: buffer.text_for_range(start..end).collect(),
            }
        })
        .collect();
    let column = |at: Point| {
        let row = &rows[(at.row - point.row.min(mark.row)) as usize];
        let byte = (at.column as usize).min(row.text.len());
        line_width(&row.text[..byte], tab)
    };
    let columns = Columns::from_corners(column(point), column(mark));
    Some(Corners {
        buffer,
        rows,
        columns,
        tab,
    })
}

/// Runs the rectangle operation `op` between the mark and the point. A read-only buffer is
/// left alone. All rows change in one undo step that also drops the secondary carets (undo
/// brings them back); afterwards one caret sits at the plan's point and mark mode is off. The
/// mark stays where it was. A plan without edits makes no edit and no undo step. Returns whether
/// the rectangle was applied.
pub(crate) fn apply(
    editor: &Entity<Editor>,
    op: RectOp,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if editor.read(cx).read_only(cx) {
        bongos_minibuffer::message(window, cx, "Buffer is read-only");
        return false;
    }
    let Some(corners) = corners(editor, window, cx) else {
        return false;
    };
    apply_to(editor, op, corners, window, cx)
}

/// [`apply`] on corners captured earlier: the plan is made from their rows, and its edits land
/// by anchor, so text changed since then moves with them.
pub(crate) fn apply_to(
    editor: &Entity<Editor>,
    op: RectOp,
    corners: Corners,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if editor.read(cx).read_only(cx) {
        bongos_minibuffer::message(window, cx, "Buffer is read-only");
        return false;
    }
    let Some(top) = corners.rows.first() else {
        return false;
    };
    let plan = rectangle_ops::plan(op, &corners.rows, corners.columns, corners.tab);
    let buffer = &corners.buffer;
    let top_start = top.start;
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let lift = |offset: usize| {
            let anchor = buffer.anchor_before(offset);
            snapshot
                .anchor_range_in_buffer(anchor..anchor)
                .map(|range| range.start)
        };
        // The top row's start: no edit comes before it, so the point after the edits is the
        // same distance from it as in the plan.
        let Some(top_anchor) = lift(top_start) else {
            return;
        };
        let place_caret =
            |editor: &mut Editor, window: &mut Window, cx: &mut gpui::Context<Editor>| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let point =
                    MultiBufferOffset(top_anchor.to_offset(&snapshot).0 + (plan.point - top_start));
                editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                    s.select_ranges([point..point])
                });
            };
        if plan.edits.is_empty() {
            place_caret(editor, window, cx);
        } else {
            let edits: Vec<_> = plan
                .edits
                .iter()
                .filter_map(|(range, text)| {
                    let start = lift(range.start)?;
                    let end = lift(range.end)?;
                    Some((start..end, text.clone()))
                })
                .collect();
            // Text typed just before the command must not merge into the command's undo step.
            editor.finalize_last_transaction(cx);
            editor.transact(window, cx, |editor, window, cx| {
                editor.edit(edits, cx);
                place_caret(editor, window, cx);
            });
            editor.finalize_last_transaction(cx);
        }
        editor.set_selection_mark_mode(false, cx);
    });
    true
}

fn not_implemented(window: &mut Window, cx: &mut App) {
    bongos_minibuffer::message(window, cx, "Not implemented yet");
}

/// `delete-rectangle`: deletes the rectangle without saving it.
pub(crate) fn delete_rectangle_command(
    _: &DeleteRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Delete, window, cx);
}

/// `open-rectangle`: inserts blank space over the rectangle, pushing its text right.
pub(crate) fn open_rectangle_command(
    _: &OpenRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Open, window, cx);
}

/// `clear-rectangle`: replaces the rectangle with spaces.
pub(crate) fn clear_rectangle_command(
    _: &ClearRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Clear, window, cx);
}

/// `keep-rectangle`: keeps only the rectangle's text on its rows.
pub(crate) fn keep_rectangle_command(
    _: &KeepRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Keep, window, cx);
}

pub(crate) fn rectangle_mark_mode_command(
    _: &RectangleMarkMode,
    _: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    not_implemented(window, cx);
}

pub(crate) fn copy_rectangle_as_kill_command(
    _: &CopyRectangleAsKill,
    _: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    not_implemented(window, cx);
}

pub(crate) fn kill_rectangle_command(
    _: &KillRectangle,
    _: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    not_implemented(window, cx);
}

pub(crate) fn yank_rectangle_command(
    _: &YankRectangle,
    _: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    not_implemented(window, cx);
}
