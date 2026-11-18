//! The rectangle commands: the rectangle between the mark and the point, and the one path that
//! applies a rectangle plan to the editor. The row plans live in
//! `bongos_emacs_model::rectangle_ops`.

use bongos_emacs_model::kill_ring::EntryKind;
use bongos_emacs_model::rectangle::{Columns, line_width};
use bongos_emacs_model::rectangle_ops::{self, RectOp, RectPlan, RowText};
use editor::{Editor, SelectionEffects, kill_ring};
use gpui::{App, Context, Entity, Window};
use language::BufferSnapshot;
use language::language_settings::LanguageSettings;
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};
use text::{Point, ToOffset as _, TransactionId};

use crate::actions::{
    ClearRectangle, CopyRectangleAsKill, DeleteRectangle, KeepRectangle, KillRectangle,
    OpenRectangle, RectangleMarkMode, YankRectangle,
};
use crate::{mark, rectangle_mode, yank};

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
    /// The point, in `buffer`.
    pub point: Point,
    /// The mark, in `buffer`.
    pub mark: Point,
}

/// The rectangle with the point and the mark at opposite corners. The point is the newest
/// selection's head, or the rectangle mark mode's point while that mode is on. Rows are buffer
/// lines; columns are display columns. Says why and returns `None` when there is no mark or the
/// mark is in another excerpt than the point.
pub(crate) fn corners(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Corners> {
    let Some(mark) = mark::mark_for(editor, cx) else {
        bongos_minibuffer::message(window, cx, "The mark is not set");
        return None;
    };
    let point = rectangle_mode::point(editor.read(cx));
    match corners_between(editor.read(cx), point, mark, cx) {
        Ok(corners) => Some(corners),
        Err(Some(why)) => {
            bongos_minibuffer::message(window, cx, why);
            None
        }
        Err(None) => None,
    }
}

/// The rectangle with `point` and `mark` at opposite corners. Fails with the reason to show
/// when they are in different excerpts, and without one when either is not in a buffer.
pub(crate) fn corners_between(
    editor: &Editor,
    point: Anchor,
    mark: Anchor,
    cx: &App,
) -> Result<Corners, Option<&'static str>> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let point_offset = point.to_offset(&snapshot);
    let mark_offset = mark.to_offset(&snapshot);
    if snapshot
        .excerpt_containing(point_offset.min(mark_offset)..point_offset.max(mark_offset))
        .is_none()
    {
        return Err(Some("The rectangle spans more than one excerpt"));
    }
    let (point_anchor, buffer) = snapshot.anchor_to_buffer_anchor(point).ok_or(None)?;
    let (mark_anchor, _) = snapshot.anchor_to_buffer_anchor(mark).ok_or(None)?;
    let buffer = buffer.clone();
    let point = buffer.offset_to_point(point_anchor.to_offset(&buffer));
    let mark = buffer.offset_to_point(mark_anchor.to_offset(&buffer));
    let tab = tab_size(&buffer, cx);
    let rows: Vec<RowText> = (point.row.min(mark.row)..=point.row.max(mark.row))
        .map(|row| row_text(&buffer, row))
        .collect();
    let column = |at: Point| {
        let row = &rows[(at.row - point.row.min(mark.row)) as usize];
        let byte = (at.column as usize).min(row.text.len());
        line_width(&row.text[..byte], tab)
    };
    let columns = Columns::from_corners(column(point), column(mark));
    Ok(Corners {
        buffer,
        rows,
        columns,
        tab,
        point,
        mark,
    })
}

/// The tab size of `buffer`'s language.
fn tab_size(buffer: &BufferSnapshot, cx: &App) -> usize {
    LanguageSettings::for_buffer_snapshot(buffer, None, cx)
        .tab_size
        .get() as usize
}

/// Buffer line `row`, without its newline.
fn row_text(buffer: &BufferSnapshot, row: u32) -> RowText {
    let start = buffer.point_to_offset(Point::new(row, 0));
    let end = buffer.point_to_offset(Point::new(row, buffer.line_len(row)));
    RowText {
        start,
        text: buffer.text_for_range(start..end).collect(),
    }
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
    if corners.rows.is_empty() {
        return false;
    }
    let plan = rectangle_ops::plan(op, &corners.rows, corners.columns, corners.tab);
    apply_plan(editor, &corners, &plan, window, cx);
    true
}

/// The rectangle between the mark and the point and the plan of `op` on it. `None`, having
/// said why, when there is no rectangle.
fn plan_here(
    editor: &Entity<Editor>,
    op: RectOp,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Corners, RectPlan)> {
    let corners = corners(editor, window, cx)?;
    corners.rows.first()?;
    let plan = rectangle_ops::plan(op, &corners.rows, corners.columns, corners.tab);
    Some((corners, plan))
}

/// Applies `plan`, made on `corners`, and turns mark mode off.
fn apply_plan(
    editor: &Entity<Editor>,
    corners: &Corners,
    plan: &RectPlan,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(top) = corners.rows.first() else {
        return;
    };
    editor.update(cx, |editor, cx| {
        run_plan(editor, &corners.buffer, top.start, plan, window, cx);
        editor.set_selection_mark_mode(false, cx);
    });
}

/// Makes the edits of `plan` (in offsets of `buffer`, none before `top_start`) as one undo step
/// that also leaves a single caret at the plan's point, and returns that step. A plan without
/// edits only moves the caret and returns `None`.
fn run_plan(
    editor: &mut Editor,
    buffer: &BufferSnapshot,
    top_start: usize,
    plan: &RectPlan,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<TransactionId> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let lift = |offset: usize| {
        let anchor = buffer.anchor_before(offset);
        snapshot
            .anchor_range_in_buffer(anchor..anchor)
            .map(|range| range.start)
    };
    // The top row's start: no edit comes before it, so the point after the edits is the
    // same distance from it as in the plan.
    let top_anchor = lift(top_start)?;
    let place_caret = |editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let point = MultiBufferOffset(top_anchor.to_offset(&snapshot).0 + (plan.point - top_start));
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges([point..point])
        });
    };
    if plan.edits.is_empty() {
        place_caret(editor, window, cx);
        return None;
    }
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
    let transaction = editor.transact(window, cx, |editor, window, cx| {
        editor.edit(edits, cx);
        place_caret(editor, window, cx);
    });
    editor.finalize_last_transaction(cx);
    transaction
}

/// Inserts the rectangle `rows` with its upper left corner at `at`, row `i` on the `i`th line
/// below, padding short lines with spaces and adding lines at the end of the buffer as needed.
/// One undo step; afterwards a single caret sits after the bottom row. All-empty rows change
/// nothing and leave the caret at `at`. Returns the caret and the undo step, if any.
pub(crate) fn insert_rectangle(
    editor: &Entity<Editor>,
    rows: &[String],
    at: Anchor,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Anchor, Option<TransactionId>)> {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let (at, buffer) = snapshot.anchor_to_buffer_anchor(at)?;
    let buffer = buffer.clone();
    let at = buffer.offset_to_point(at.to_offset(&buffer));
    let tab = tab_size(&buffer, cx);
    let last_row = (at.row as usize + rows.len().max(1) - 1).min(buffer.max_point().row as usize);
    let lines: Vec<RowText> = (at.row..=last_row as u32)
        .map(|row| row_text(&buffer, row))
        .collect();
    let first = lines.first()?;
    let column = line_width(
        first.text.get(..at.column as usize).unwrap_or(&first.text),
        tab,
    );
    let top_start = first.start;
    let plan = rectangle_ops::insert(&lines, 0, column, rows, buffer.len(), tab);
    editor.update(cx, |editor, cx| {
        let transaction = run_plan(editor, &buffer, top_start, &plan, window, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = editor.selections.newest_anchor().head();
        Some((
            snapshot.anchor_before(head.to_offset(&snapshot)),
            transaction,
        ))
    })
}

/// `delete-rectangle`: deletes the rectangle without saving it.
pub(crate) fn delete_rectangle_command(
    _: &DeleteRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Delete, window, cx);
    rectangle_mode::finish(editor, window, cx);
}

/// `open-rectangle`: inserts blank space over the rectangle, pushing its text right.
pub(crate) fn open_rectangle_command(
    _: &OpenRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Open, window, cx);
    rectangle_mode::finish(editor, window, cx);
}

/// `clear-rectangle`: replaces the rectangle with spaces.
pub(crate) fn clear_rectangle_command(
    _: &ClearRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Clear, window, cx);
    rectangle_mode::finish(editor, window, cx);
}

/// `keep-rectangle`: keeps only the rectangle's text on its rows.
pub(crate) fn keep_rectangle_command(
    _: &KeepRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply(editor, RectOp::Keep, window, cx);
    rectangle_mode::finish(editor, window, cx);
}

/// `rectangle-mark-mode`: turns rectangle mark mode on, or off when it is on.
pub(crate) fn rectangle_mark_mode_command(
    _: &RectangleMarkMode,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    rectangle_mode::toggle(editor, window, cx);
}

/// `copy-rectangle-as-kill`: saves the rectangle to the kill ring as a rectangle entry. The
/// buffer, the point and the mark stay as they are, and the clipboard is not written. Works in
/// a read-only buffer; a masked editor stores nothing.
pub(crate) fn copy_rectangle_as_kill_command(
    _: &CopyRectangleAsKill,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if !editor.read(cx).is_masked(cx)
        && let Some((_, plan)) = plan_here(editor, RectOp::Copy, window, cx)
    {
        kill_ring::push_rectangle(&plan.extracted.unwrap_or_default(), cx);
        editor.update(cx, |editor, cx| editor.set_selection_mark_mode(false, cx));
    }
    rectangle_mode::finish(editor, window, cx);
}

/// `kill-rectangle`: saves the rectangle to the kill ring as a rectangle entry, then deletes
/// it in one undo step. A masked editor neither stores nor deletes anything; a read-only one
/// stores the rectangle but deletes nothing.
pub(crate) fn kill_rectangle_command(
    _: &KillRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    kill_rectangle(editor, window, cx);
    rectangle_mode::finish(editor, window, cx);
}

fn kill_rectangle(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if editor.read(cx).is_masked(cx) {
        return;
    }
    if editor.read(cx).read_only(cx) {
        // As in Emacs, the rows still reach the ring before the error.
        if let Some((_, plan)) = plan_here(editor, RectOp::Copy, window, cx) {
            kill_ring::push_rectangle(&plan.extracted.unwrap_or_default(), cx);
        }
        bongos_minibuffer::message(window, cx, "Buffer is read-only");
        return;
    }
    let Some((corners, plan)) = plan_here(editor, RectOp::Delete, window, cx) else {
        return;
    };
    // The rows reach the ring before the text goes.
    kill_ring::push_rectangle(plan.extracted.as_deref().unwrap_or_default(), cx);
    apply_plan(editor, &corners, &plan, window, cx);
}

/// `yank-rectangle`: inserts the newest rectangle entry of the kill ring at point. The
/// clipboard is not consulted.
pub(crate) fn yank_rectangle_command(
    _: &YankRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    rectangle_mode::finish(editor, window, cx);
    if editor.read(cx).read_only(cx) {
        bongos_minibuffer::message(window, cx, "Buffer is read-only");
        return;
    }
    let newest = kill_ring::read(cx).and_then(|ring| {
        (0..ring.len()).find_map(|index| {
            let entry = ring.get(index)?;
            (entry.kind == EntryKind::Rectangle).then(|| (index, entry.clone()))
        })
    });
    let Some((index, entry)) = newest else {
        bongos_minibuffer::message(window, cx, "No rectangle to yank");
        return;
    };
    yank::yank_entry(editor, index, &entry, window, cx);
}
