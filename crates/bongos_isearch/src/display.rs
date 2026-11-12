//! What the session shows in the origin editor besides the cursor: every match near the visible
//! text, the current one in its own color, and the folds and collapsed excerpts a match sits in.
//!
//! A landing opens the folds that hide the match, as buffer search does, and remembers them. When
//! the search moves on, the folds it opened that do not hide the new match close again, so a
//! search that passes through folded code leaves it folded. An abort closes every fold the session
//! opened; an exit that keeps the match leaves the folds around it open.

use std::ops::Range;

use editor::display_map::{Crease, FoldPlaceholder};
use editor::{Editor, HighlightKey};
use gpui::{Context, Window};
use language::{BufferId, Point};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot, ToOffset as _};
use project::search::SearchQuery;

use crate::engine::in_range;

/// The most matches highlighted at once, so a pattern that matches everywhere stays cheap.
pub(crate) const MAX_HIGHLIGHTS: usize = 1000;

/// A fold the session opened, kept to close it again.
struct OpenedFold {
    range: Range<Anchor>,
    placeholder: FoldPlaceholder,
}

/// The folds and the collapsed buffers the session has opened and not closed again.
#[derive(Default)]
pub(crate) struct Display {
    folds: Vec<OpenedFold>,
    buffers: Vec<BufferId>,
}

fn clamp(snapshot: &MultiBufferSnapshot, range: &Range<usize>) -> Range<MultiBufferOffset> {
    let len = snapshot.len();
    MultiBufferOffset(range.start).min(len)..MultiBufferOffset(range.end).min(len)
}

fn buffers_of(
    editor: &Editor,
    snapshot: &MultiBufferSnapshot,
    range: Range<MultiBufferOffset>,
    cx: &Context<Editor>,
) -> Vec<BufferId> {
    if editor.buffer().read(cx).is_singleton() {
        return Vec::new();
    }
    snapshot
        .range_to_buffer_ranges(range)
        .into_iter()
        .map(|(buffer, _, _)| buffer.remote_id())
        .collect()
}

impl Display {
    /// Opens the folds and collapsed buffers that hide `found`, before the cursor lands on it.
    pub(crate) fn open(
        &mut self,
        editor: &mut Editor,
        found: &Range<usize>,
        cx: &mut Context<Editor>,
    ) {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let range = clamp(&snapshot, found);

        let display = editor.display_snapshot(cx);
        let folds: Vec<_> = display
            .folds_in_range(range.clone())
            .map(|fold| OpenedFold {
                range: fold.range.start..fold.range.end,
                placeholder: fold.placeholder.clone(),
            })
            .collect();
        drop(display);
        if !folds.is_empty() {
            editor.unfold_ranges(std::slice::from_ref(&range), false, false, cx);
            self.folds.extend(folds);
        }

        for id in buffers_of(editor, &snapshot, range, cx) {
            if editor.is_buffer_folded(id, cx) {
                editor.unfold_buffer(id, cx);
                if !self.buffers.contains(&id) {
                    self.buffers.push(id);
                }
            }
        }
    }

    /// Closes again the folds and buffers the session opened that do not hide `keep`, the match
    /// the cursor is on now (`None`: no match, so all of them).
    pub(crate) fn refold(
        &mut self,
        editor: &mut Editor,
        keep: Option<&Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        if self.folds.is_empty() && self.buffers.is_empty() {
            return;
        }
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let keep = keep.map(|range| clamp(&snapshot, range));

        let mut creases = Vec::new();
        self.folds.retain(|fold| {
            let start = fold.range.start.to_offset(&snapshot);
            let end = fold.range.end.to_offset(&snapshot);
            let hides = keep
                .as_ref()
                .is_some_and(|keep| start < keep.end && keep.start < end);
            if !hides {
                creases.push(Crease::simple(fold.range.clone(), fold.placeholder.clone()));
            }
            hides
        });
        editor.fold_creases(creases, false, window, cx);

        let kept = keep
            .map(|keep| buffers_of(editor, &snapshot, keep, cx))
            .unwrap_or_default();
        let mut closing = Vec::new();
        self.buffers.retain(|id| {
            let hides = kept.contains(id);
            if !hides {
                closing.push(*id);
            }
            hides
        });
        for id in closing {
            editor.fold_buffer(id, cx);
        }
    }

    /// Closes everything the session opened, for an abort.
    pub(crate) fn restore(
        &mut self,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        self.refold(editor, None, window, cx);
    }
}

/// Highlights every match of `query` in the visible text and one screen above and below it, the
/// `current` one in the active color. No query (an empty or invalid string) clears them.
pub(crate) fn highlight(
    editor: &mut Editor,
    query: Option<&SearchQuery>,
    current: Option<&Range<usize>>,
    cx: &mut Context<Editor>,
) {
    let Some(query) = query else {
        clear(editor, cx);
        return;
    };
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let display = editor.display_snapshot(cx);
    let visible = editor.multi_buffer_visible_range(&display, cx);
    drop(display);
    let screen = visible.end.row.saturating_sub(visible.start.row).max(1);
    let last_row = snapshot.max_point().row;
    let start_row = visible.start.row.saturating_sub(screen).min(last_row);
    let end_row = visible.end.row.saturating_add(screen).min(last_row);
    let start = snapshot.point_to_offset(Point::new(start_row, 0));
    let end = snapshot.point_to_offset(Point::new(
        end_row,
        snapshot.line_len(MultiBufferRow(end_row)),
    ));

    let mut ranges = in_range(&snapshot, query, start..end);
    let is_current = |range: &Range<Anchor>| {
        current.is_some_and(|current| {
            range.start.to_offset(&snapshot).0 == current.start
                && range.end.to_offset(&snapshot).0 == current.end
        })
    };
    // The cap never drops the current match: it takes the last place of the capped set.
    if ranges.len() > MAX_HIGHLIGHTS
        && let Some(position) = ranges.iter().position(is_current)
        && position >= MAX_HIGHLIGHTS
    {
        let current = ranges.swap_remove(position);
        ranges.truncate(MAX_HIGHLIGHTS - 1);
        ranges.push(current);
    } else {
        ranges.truncate(MAX_HIGHLIGHTS);
    }
    let active = ranges.iter().position(is_current);
    editor.highlight_background(
        HighlightKey::Isearch,
        &ranges,
        move |index, theme| {
            if active == Some(*index) {
                theme.colors().search_active_match_background
            } else {
                theme.colors().search_match_background
            }
        },
        cx,
    );
}

/// Removes the highlights.
pub(crate) fn clear(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.clear_background_highlights(HighlightKey::Isearch, cx);
}
