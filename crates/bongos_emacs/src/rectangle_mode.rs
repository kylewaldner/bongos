//! Rectangle mark mode. While it is on, the selections are the rectangle between the mark and
//! the point as a column selection, one selection per row, and the editor has the
//! `rectangle_mode` key context. Mark mode stays on, so the region motions move the point
//! corner.

use std::any::{Any, TypeId};
use std::ops::Range;

use bongos_emacs_model::rectangle::{cells, line_width};
use editor::actions::{
    SelectDown, SelectLeft, SelectPageDown, SelectPageUp, SelectRight, SelectToBeginning,
    SelectToBeginningOfLine, SelectToEnd, SelectToEndOfLine, SelectToEndOfParagraph,
    SelectToNextWordEnd, SelectToPreviousWordStart, SelectToStartOfParagraph, SelectUp,
};
use editor::{Addon, Editor, EditorEvent, SelectionEffects};
use gpui::{
    Action, App, Context, DispatchPhase, Entity, Focusable as _, Global, KeyContext, WeakEntity,
    Window,
};
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};

use crate::actions::{BackwardWord, ForwardWord, SetMarkCommand};
use crate::{mark, quit, rectangle};

/// The key context set while the mode is on.
pub(crate) const KEY_CONTEXT: &str = "rectangle_mode";

/// The mode's state on one editor.
#[derive(Default)]
pub(crate) struct RectangleMode {
    on: bool,
    /// The selection on the point's row, whose head is the point.
    point_selection_id: Option<usize>,
}

impl Addon for RectangleMode {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        if self.on {
            context.add(KEY_CONTEXT);
        }
    }

    fn to_any(&self) -> &dyn Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

/// The editors that turned the mode on, so C-g can find them.
#[derive(Default)]
struct ActiveEditors(Vec<WeakEntity<Editor>>);

impl Global for ActiveEditors {}

/// The actions of the region-selection keymap block that move the point. Each collapses the
/// column selection to the region before it runs.
macro_rules! motions {
    ($($action:ty),* $(,)?) => {
        /// Every motion action, by type and by name.
        pub(crate) fn motions() -> Vec<(TypeId, &'static str)> {
            vec![$((TypeId::of::<$action>(), <$action as Action>::name_for_type())),*]
        }
    };
}

motions!(
    SelectRight,
    SelectLeft,
    SelectDown,
    SelectUp,
    SelectToPreviousWordStart,
    SelectToNextWordEnd,
    SelectPageDown,
    SelectPageUp,
    SelectToBeginningOfLine,
    SelectToEndOfLine,
    ForwardWord,
    BackwardWord,
    SelectToStartOfParagraph,
    SelectToEndOfParagraph,
    SelectToBeginning,
    SelectToEnd,
);

/// Whether the mode is on in `editor`.
pub(crate) fn is_on(editor: &Editor) -> bool {
    editor.addon::<RectangleMode>().is_some_and(|mode| mode.on)
}

/// The point: while the mode is on, the head of the selection on the point's row; otherwise
/// (or when that selection is gone) the newest selection's head.
pub(crate) fn point(editor: &Editor) -> Anchor {
    let id = editor
        .addon::<RectangleMode>()
        .filter(|mode| mode.on)
        .and_then(|mode| mode.point_selection_id);
    id.and_then(|id| {
        editor
            .selections
            .disjoint_anchors()
            .iter()
            .find(|selection| selection.id == id)
            .map(|selection| selection.head())
    })
    .unwrap_or_else(|| editor.selections.newest_anchor().head())
}

/// What the selections become when the mode ends.
#[derive(Clone, Copy)]
enum Collapse {
    /// The single region from the mark to the point.
    ToRegion,
    /// A single caret at the point.
    ToPoint,
}

/// Registers the C-g hook that ends the mode in the focused editor.
pub(crate) fn init(cx: &mut App) {
    quit::register(cx, |window, cx| {
        let editors = std::mem::take(&mut cx.default_global::<ActiveEditors>().0);
        for editor in editors.into_iter().filter_map(|weak| weak.upgrade()) {
            if !is_on(editor.read(cx)) {
                continue;
            }
            if editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                end(&editor, Collapse::ToRegion, window, cx);
            } else {
                cx.default_global::<ActiveEditors>()
                    .0
                    .push(editor.downgrade());
            }
        }
    });
}

/// Adds the mode to a new editor: its state, the listener that collapses the column selection
/// before a motion or a set-mark, and the subscriptions that rebuild or end it.
pub(crate) fn init_editor(
    editor: &mut Editor,
    window: Option<&mut Window>,
    cx: &mut Context<Editor>,
) {
    editor.register_addon(RectangleMode::default());
    let weak = cx.entity().downgrade();
    let motions = motions();
    editor
        .register_action_renderer(move |_, window, _| {
            // A motion extends the region; a set-mark ends the mode.
            let listeners = motions
                .iter()
                .map(|(type_id, _)| (*type_id, false))
                .chain([(TypeId::of::<SetMarkCommand>(), true)]);
            for (type_id, ends) in listeners {
                let weak = weak.clone();
                window.on_action(type_id, move |_, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        // A bubble listener stops the action unless it propagates it.
                        cx.propagate();
                        return;
                    }
                    let Some(editor) = weak.upgrade() else {
                        return;
                    };
                    if ends {
                        end(&editor, Collapse::ToRegion, window, cx);
                    } else {
                        to_region(&editor, window, cx);
                    }
                });
            }
        })
        .detach();
    let Some(window) = window else {
        return;
    };
    let this = cx.entity();
    cx.subscribe_in(
        &this,
        window,
        |editor, _, event: &EditorEvent, window, cx| {
            if !is_on(editor) {
                return;
            }
            match event {
                EditorEvent::Edited { .. } => turn_off(editor, cx),
                EditorEvent::SelectionsChanged { local: true } => {
                    if editor.selections.pending_anchor().is_some() {
                        // A mouse selection ends the mode.
                        turn_off(editor, cx);
                        return;
                    }
                    let this = cx.entity();
                    window.defer(cx, move |window, cx| rebuild(&this, window, cx));
                }
                _ => {}
            }
        },
    )
    .detach();
}

/// `rectangle-mark-mode`: turns the mode on, setting the mark at the point first when the
/// region is not active, or turns it off, leaving the region from the mark to the point.
pub(crate) fn toggle(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if is_on(editor.read(cx)) {
        end(editor, Collapse::ToRegion, window, cx);
        return;
    }
    let has_addon = editor.update(cx, |editor, _| {
        if editor.addon::<RectangleMode>().is_none() {
            editor.register_addon(RectangleMode::default());
        }
        editor.addon::<RectangleMode>().is_some()
    });
    if !has_addon {
        return;
    }
    let (mark_mode, newest) = {
        let editor = editor.read(cx);
        (
            editor.selection_mark_mode(),
            editor.selections.newest_anchor().clone(),
        )
    };
    let region_active = mark_mode || newest.start != newest.end;
    if !region_active || mark::mark_for(editor, cx).is_none() {
        if newest.start != newest.end {
            mark::set_mark(editor, newest.tail(), cx);
            editor.update(cx, |editor, cx| editor.set_selection_mark_mode(true, cx));
        } else {
            mark::push_mark(editor, None, true, window, cx);
        }
    } else if !mark_mode {
        // A region made outside mark mode (shift-selection, a mouse drag): its tail is the mark.
        mark::set_mark(editor, newest.tail(), cx);
        editor.update(cx, |editor, cx| editor.set_selection_mark_mode(true, cx));
    }
    editor.update(cx, |editor, cx| {
        if let Some(mode) = editor.addon_mut::<RectangleMode>() {
            mode.on = true;
            mode.point_selection_id = None;
        }
        cx.notify();
    });
    // Only editors still in the mode stay, each once.
    let mut active = std::mem::take(&mut cx.default_global::<ActiveEditors>().0);
    active.retain(|weak| {
        weak.upgrade()
            .is_some_and(|other| other != *editor && is_on(other.read(cx)))
    });
    active.push(editor.downgrade());
    cx.default_global::<ActiveEditors>().0 = active;
    rebuild(editor, window, cx);
}

/// Ends the mode, if on, after a rectangle command: a single caret at the point.
pub(crate) fn finish(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    end(editor, Collapse::ToPoint, window, cx);
}

/// Turns the mode off without touching the selections.
fn turn_off(editor: &mut Editor, cx: &mut Context<Editor>) {
    if let Some(mode) = editor.addon_mut::<RectangleMode>() {
        mode.on = false;
        mode.point_selection_id = None;
    }
    editor.clear_past_line_end_highlights(cx);
    cx.notify();
}

/// Ends the mode, if on, collapsing the column selection as `collapse` says.
fn end(editor: &Entity<Editor>, collapse: Collapse, window: &mut Window, cx: &mut App) {
    if !is_on(editor.read(cx)) {
        return;
    }
    let point = point(editor.read(cx));
    let mark = mark::mark_for(editor, cx);
    editor.update(cx, |editor, cx| {
        turn_off(editor, cx);
        let tail = match collapse {
            Collapse::ToRegion => mark.unwrap_or(point),
            Collapse::ToPoint => point,
        };
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_anchor_ranges([tail..point])
        });
    });
}

/// Before a motion: the column selection becomes the single region from the mark to the point,
/// which the motion extends. The selection change that follows rebuilds the column selection.
pub(crate) fn to_region(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if !is_on(editor.read(cx)) {
        return;
    }
    let Some(mark) = mark::mark_for(editor, cx) else {
        return;
    };
    let point = point(editor.read(cx));
    editor.update(cx, |editor, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_anchor_ranges([mark..point])
        });
    });
}

/// The column selection of the rectangle between the mark and the newest selection's head,
/// and the past-line-end highlights of its short rows.
struct Build {
    /// One `tail..head` range per row that reaches the left column, the point's row last.
    ranges: Vec<Range<MultiBufferOffset>>,
    highlights: Vec<(Anchor, Range<u32>)>,
}

/// Byte of the first character at or right of display column `col` that is not inside a
/// character starting left of it, or the line's length. A wide character or tab that `col`
/// cuts is included: `left` gives its start.
fn left_byte(text: &str, col: usize, tab: usize) -> usize {
    cells(text, tab)
        .into_iter()
        .find(|cell| cell.width > 0 && cell.start_col + cell.width > col)
        .map_or(text.len(), |cell| cell.byte)
}

/// Byte after the last character that ends at or left of display column `col`, counting a
/// character that `col` cuts as inside, or the line's length.
fn right_byte(text: &str, col: usize, tab: usize) -> usize {
    cells(text, tab)
        .into_iter()
        .find(|cell| cell.width > 0 && cell.start_col >= col)
        .map_or(text.len(), |cell| cell.byte)
}

fn build(editor: &Editor, mark: Anchor, cx: &App) -> Option<Build> {
    let point = editor.selections.newest_anchor().head();
    let corners = rectangle::corners_between(editor, point, mark, cx).ok()?;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let lift = |offset: usize| {
        let anchor = corners.buffer.anchor_before(offset);
        snapshot
            .anchor_range_in_buffer(anchor..anchor)
            .map(|range| range.start)
    };
    let (left, right) = (corners.columns.left, corners.columns.right);
    let top = corners.point.row.min(corners.mark.row);
    let point_row = (corners.point.row - top) as usize;
    let point_text = &corners.rows[point_row].text;
    let point_byte = (corners.point.column as usize).min(point_text.len());
    let mark_text = &corners.rows[(corners.mark.row - top) as usize].text;
    let mark_byte = (corners.mark.column as usize).min(mark_text.len());
    // Heads go on the point's side.
    let reversed = line_width(&point_text[..point_byte], corners.tab)
        < line_width(&mark_text[..mark_byte], corners.tab);
    let mut ranges = Vec::new();
    let mut point_range = None;
    let mut highlights = Vec::new();
    for (index, row) in corners.rows.iter().enumerate() {
        let width = line_width(&row.text, corners.tab);
        if width < right {
            let start = lift(row.start)?;
            highlights.push((
                start,
                left.saturating_sub(width) as u32..(right - width) as u32,
            ));
        }
        let (start, end) = if index == point_row {
            if reversed {
                (
                    point_byte,
                    right_byte(&row.text, right, corners.tab).max(point_byte),
                )
            } else {
                (
                    left_byte(&row.text, left, corners.tab).min(point_byte),
                    point_byte,
                )
            }
        } else if width < left {
            continue;
        } else {
            (
                left_byte(&row.text, left, corners.tab),
                right_byte(&row.text, right, corners.tab),
            )
        };
        let start = lift(row.start + start)?.to_offset(&snapshot);
        let end = lift(row.start + end)?.to_offset(&snapshot);
        let range = if reversed { end..start } else { start..end };
        if index == point_row {
            point_range = Some(range);
        } else {
            ranges.push(range);
        }
    }
    ranges.push(point_range?);
    Some(Build { ranges, highlights })
}

/// Makes the selections the column selection of the rectangle between the mark and the newest
/// selection's head, unless they already are, and sets the past-line-end highlights. Ends the
/// mode when mark mode went off or there is no rectangle; an edit ends it through the editor's
/// edited event.
fn rebuild(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if !is_on(editor.read(cx)) {
        return;
    }
    let mark = mark::mark_for(editor, cx);
    editor.update(cx, |editor, cx| {
        if !editor.selection_mark_mode() {
            turn_off(editor, cx);
            return;
        }
        let Some(build) = mark.and_then(|mark| build(editor, mark, cx)) else {
            turn_off(editor, cx);
            return;
        };
        let display = editor.display_snapshot(cx);
        let current = editor.selections.all::<MultiBufferOffset>(&display);
        let newest = editor.selections.newest::<MultiBufferOffset>(&display);
        let as_range = |start, end, reversed: bool| if reversed { end..start } else { start..end };
        let mut have: Vec<_> = current
            .iter()
            .map(|s| as_range(s.start, s.end, s.reversed))
            .collect();
        let mut want = build.ranges.clone();
        have.sort_by_key(|range| (range.start.min(range.end), range.start));
        want.sort_by_key(|range| (range.start.min(range.end), range.start));
        let same = have == want
            && build.ranges.last() == Some(&as_range(newest.start, newest.end, newest.reversed));
        if !same {
            editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                s.select_ranges(build.ranges.clone())
            });
        }
        let point_selection_id = editor.selections.newest_anchor().id;
        if let Some(mode) = editor.addon_mut::<RectangleMode>() {
            mode.point_selection_id = Some(point_selection_id);
        }
        if editor.past_line_end_highlights() != build.highlights.as_slice() {
            editor.set_past_line_end_highlights(build.highlights, cx);
        }
    });
}
