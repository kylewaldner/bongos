//! Point skipping (design D9). A marker row is drawn as a rule (a replace block), and the display
//! clips a point inside it to the row's start, so a head that lands on a marker row is moved off
//! it: forward to the first body row below the run of markers, or back to the end of the row above
//! it. The text is never changed, and a selection's tail stays where it is.

use editor::{Anchor, Editor, SelectionEffects, ToPoint as _};
use gpui::{App, Context, Window};
use language::{Point, SelectionGoal};

use crate::mode::PromptsMode;

/// Where a head on a marker row goes, or `None` when it stays.
///
/// `prev` is where the head was before it moved; the head goes forward when it moved to a later
/// row (or the run starts at row 0), back when it moved to an earlier one. A run of consecutive
/// marker rows is skipped as one. Forward from a run that reaches the last row goes to the end of
/// the file, and a head already there stays: the mode never appends a row to make room. The
/// display clips the end of the file to the start of its rule, so a move that stays on that row
/// (`C-n` or `C-f` there) also goes forward, back to the end.
pub(crate) fn skip_target(
    head: Point,
    prev: Point,
    last_row: u32,
    is_marker: impl Fn(u32) -> bool,
    line_len: impl Fn(u32) -> u32,
) -> Option<Point> {
    if !is_marker(head.row) {
        return None;
    }
    let end_of_file = Point::new(last_row, line_len(last_row));
    if head == end_of_file {
        return None;
    }
    let mut first = head.row;
    while first > 0 && is_marker(first - 1) {
        first -= 1;
    }
    let mut last = head.row;
    while last < last_row && is_marker(last + 1) {
        last += 1;
    }
    let forward = first == 0
        || match prev.row.cmp(&head.row) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => prev == end_of_file || prev <= head,
        };
    if forward {
        if last < last_row {
            Some(Point::new(last + 1, 0))
        } else {
            Some(end_of_file)
        }
    } else {
        Some(Point::new(first - 1, line_len(first - 1)))
    }
}

/// Moves every selection head on a marker row off it, in one selection change, and remembers the
/// heads for the next call. A drag in progress is left alone until the mouse is released.
pub(crate) fn skip_markers(
    mode: &mut PromptsMode,
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if editor.selections.pending_anchor().is_some() {
        return;
    }
    // The selections as stored, not as the display resolves them: that clips a point inside a
    // rule to the rule's start, which would lose a tail's column and the end of the file.
    let buffer = &editor.buffer().read(cx).snapshot(cx);
    let previous: Vec<Point> = mode
        .heads
        .iter()
        .map(|head| head.to_point(buffer))
        .collect();
    let mut selections: Vec<_> = editor
        .selections
        .disjoint_anchors()
        .iter()
        .map(|selection| selection.map(|anchor| anchor.to_point(buffer)))
        .collect();
    let last_row = buffer.max_point().row;
    let markers = mode.index.markers();
    let is_marker = |row: u32| {
        markers
            .binary_search_by_key(&row, |marker| marker.row)
            .is_ok()
    };
    let line_len = |row: u32| buffer.line_len(multi_buffer::MultiBufferRow(row));
    let count = selections.len();
    let mut moved = false;
    for (ix, selection) in selections.iter_mut().enumerate() {
        let head = selection.head();
        let prev = if previous.len() == count {
            previous[ix]
        } else {
            previous.last().copied().unwrap_or(head)
        };
        let Some(target) = skip_target(head, prev, last_row, is_marker, line_len) else {
            continue;
        };
        if selection.is_empty() {
            selection.collapse_to(target, SelectionGoal::None);
        } else {
            selection.set_head(target, SelectionGoal::None);
        }
        moved = true;
    }
    if moved {
        mode.stats.skip_moves += 1;
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select(selections)
        });
    }
    remember_heads(mode, editor, cx);
}

/// Records the current heads, the "previous heads" of the next skip, and whether Kyle has a
/// region (a selection that is not empty), for the guard.
pub(crate) fn remember_heads(mode: &mut PromptsMode, editor: &Editor, cx: &App) {
    let selections = editor.selections.disjoint_anchors();
    mode.heads = selections
        .iter()
        .map(|selection| selection.head())
        .collect::<Vec<Anchor>>();
    let buffer = editor.buffer().read(cx).snapshot(cx);
    mode.region = selections
        .iter()
        .any(|selection| selection.start.cmp(&selection.end, &buffer).is_ne());
}
