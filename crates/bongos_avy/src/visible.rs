//! The jump candidates on screen: the text of every visible editor, row by row as it was last
//! painted, and the matches of a query in it with their window positions, nearest to the cursor
//! first. Everything here only reads: nothing scrolls, moves a cursor or edits, and the results
//! hold weak editor handles only, so an editor closed in the meantime is skipped by whoever uses
//! them.

use std::{ops::Range, sync::Arc};

use bongos_avy_model::{ScreenPoint, char_matches, rank, word_starts};
use bongos_window::targets::targets;
use editor::{
    Anchor, Bias, DisplayPoint, Editor, MultiBufferOffset, ToOffset, ToPoint,
    display_map::{DisplayRow, DisplaySnapshot},
};
use gpui::{App, Entity, EntityId, LineLayout, WeakEntity, Window};
use workspace::Workspace;

/// A place a label can jump to.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub editor: WeakEntity<Editor>,
    /// The matched text; empty for the start of a line.
    pub range: Range<Anchor>,
    /// Where the start of the range is drawn: the top left of its character, in window pixels.
    pub screen: ScreenPoint,
}

/// What a jump looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every occurrence of the typed characters.
    CharTimer,
    /// Every word start that is the typed character.
    Word,
    /// Every line start; the query is not read.
    Line,
}

/// A run of buffer text shown on one display row with no fold placeholder inside it.
#[derive(Clone, Debug)]
pub struct Segment {
    pub text: String,
    /// The buffer offset of the first character.
    pub start: MultiBufferOffset,
    /// The buffer character just before `start`, which decides whether the first character
    /// starts a word; `None` at the start of the buffer.
    pub before: Option<char>,
}

/// One fully visible display row of text.
#[derive(Clone)]
pub struct RowText {
    pub display_row: DisplayRow,
    /// The top of the row, in window pixels.
    pub y: f32,
    /// The buffer offset of a line start drawn at the row's first column, if the row has one: a
    /// soft-wrapped continuation row, or a row whose start is hidden under a fold, has none.
    pub line_start: Option<MultiBufferOffset>,
    /// The row's buffer text, split around fold placeholders. Inlay hints are not buffer text and
    /// never appear here.
    pub segments: Vec<Segment>,
    line: Arc<LineLayout>,
}

/// The visible text of one editor, as of its last paint.
pub struct PaneText {
    pub editor: WeakEntity<Editor>,
    pub rows: Vec<RowText>,
    /// The buffer offset of the primary cursor.
    pub cursor: MultiBufferOffset,
    /// Where the primary cursor is drawn, in window pixels.
    pub cursor_screen: ScreenPoint,
    snapshot: DisplaySnapshot,
    /// The window x of the editor's first text column, after horizontal scrolling.
    x_origin: f32,
    /// The window x range of the text area; a character drawn outside it is not visible.
    x_visible: Range<f32>,
}

impl PaneText {
    fn editor_id(&self) -> EntityId {
        self.editor.entity_id()
    }

    /// Where the character at `offset`, which lies on `row`, is drawn.
    fn screen_point(&self, row: &RowText, offset: MultiBufferOffset) -> ScreenPoint {
        let point = offset.to_point(self.snapshot.buffer_snapshot());
        let display = self.snapshot.point_to_display_point(point, Bias::Right);
        // A soft wrap boundary belongs to both rows; place it on the row that holds the text.
        let column = if display.row() == row.display_row {
            display.column()
        } else if display.row() < row.display_row {
            0
        } else {
            self.snapshot.line_len(row.display_row)
        };
        ScreenPoint {
            x: self.x_origin + f32::from(row.line.x_for_index(column as usize)),
            y: row.y,
        }
    }
}

/// The visible text of every window a jump can reach: the active item of each target pane when
/// it is itself a full editor (project search and diagnostics results, terminals and panels
/// without an editor are left out), only the zoomed pane while one is zoomed, and nothing for an
/// editor that has not been painted yet.
pub fn panes(workspace: &Workspace, window: &mut Window, cx: &mut App) -> Vec<PaneText> {
    let mut panes = targets(workspace, window, cx);
    if let Some(zoomed) = workspace.zoomed_item() {
        let zoomed = zoomed.upgrade().map(|view| view.entity_id());
        panes.retain(|pane| pane.read(cx).is_zoomed() || Some(pane.entity_id()) == zoomed);
    }
    let editors: Vec<Entity<Editor>> = panes
        .iter()
        .filter_map(|pane| pane.read(cx).active_item()?.downcast::<Editor>())
        .collect();
    editors
        .iter()
        .filter_map(|editor| pane_text(editor, window, cx))
        .collect()
}

fn pane_text(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Option<PaneText> {
    if !editor.read(cx).mode().is_full() {
        return None;
    }
    let layout = editor.read(cx).visible_text_layout()?;
    let (snapshot, details, scroll_y, cursor) = editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let details = editor.text_layout_details(window, cx);
        let scroll_y = editor.scroll_position(cx).y;
        let cursor = editor.selections.newest_anchor().head();
        (snapshot, details, scroll_y, cursor)
    });
    let x_origin = f32::from(layout.content_origin.x - layout.scroll_pixel_x);
    let line_height = f32::from(layout.line_height);
    let top = f32::from(layout.content_origin.y);
    let row_y = |row: DisplayRow| top + (f64::from(row.0) - scroll_y) as f32 * line_height;

    let max_row = snapshot.max_point().row();
    let mut rows = Vec::new();
    for row in layout.fully_visible_rows.start.0..layout.fully_visible_rows.end.0 {
        let row = DisplayRow(row);
        if row > max_row {
            break;
        }
        if snapshot.is_block_line(row) {
            continue;
        }
        let (line_start, segments) = row_segments(&snapshot, row);
        rows.push(RowText {
            display_row: row,
            y: row_y(row),
            line_start,
            segments,
            line: snapshot.layout_row(row, &details),
        });
    }

    let buffer = snapshot.buffer_snapshot();
    let cursor_offset = cursor.to_offset(buffer);
    let cursor_display = snapshot.point_to_display_point(cursor.to_point(buffer), Bias::Right);
    let cursor_line = snapshot.layout_row(cursor_display.row(), &details);
    let cursor_screen = ScreenPoint {
        x: x_origin + f32::from(cursor_line.x_for_index(cursor_display.column() as usize)),
        y: row_y(cursor_display.row()),
    };
    Some(PaneText {
        editor: editor.downgrade(),
        rows,
        cursor: cursor_offset,
        cursor_screen,
        snapshot,
        x_origin,
        x_visible: f32::from(layout.content_origin.x)..f32::from(layout.text_right),
    })
}

/// The buffer text of display row `row` minus every fold placeholder on it, and the line start
/// drawn at its first column, if any.
fn row_segments(
    snapshot: &DisplaySnapshot,
    row: DisplayRow,
) -> (Option<MultiBufferOffset>, Vec<Segment>) {
    let buffer = snapshot.buffer_snapshot();
    let start_point = snapshot.display_point_to_point(DisplayPoint::new(row, 0), Bias::Left);
    let end_point = snapshot
        .display_point_to_point(DisplayPoint::new(row, snapshot.line_len(row)), Bias::Right);
    let start = start_point.to_offset(buffer);
    let end = end_point.to_offset(buffer).max(start);

    let mut folds: Vec<Range<MultiBufferOffset>> = snapshot
        .folds_in_range(start..end)
        .map(|fold| fold.range.start.to_offset(buffer)..fold.range.end.to_offset(buffer))
        .filter(|fold| fold.start < fold.end)
        .collect();
    folds.sort_by_key(|fold| fold.start);

    let mut kept = Vec::new();
    let mut cursor = start;
    for fold in &folds {
        if fold.start > cursor {
            kept.push(cursor..fold.start.min(end));
        }
        cursor = cursor.max(fold.end);
    }
    if cursor < end {
        kept.push(cursor..end);
    }
    let segments = kept
        .into_iter()
        .filter(|range| range.start < range.end)
        .map(|range| Segment {
            text: buffer.text_for_range(range.clone()).collect(),
            before: buffer.reversed_chars_at(range.start).next(),
            start: range.start,
        })
        .collect();

    let hidden = folds
        .iter()
        .any(|fold| fold.start <= start && start < fold.end);
    let line_start = (start_point.column == 0 && !hidden).then_some(start);
    (line_start, segments)
}

/// The candidates of `mode` for `query` in `panes`, nearest first to the primary cursor of the
/// editor `origin`, without the one that starts at that cursor. Empty when `origin` is not among
/// `panes` (it has no layout yet), or when the query is empty in a mode that reads it.
pub fn collect(mode: Mode, query: &str, panes: &[PaneText], origin: EntityId) -> Vec<Candidate> {
    let Some(origin_pane) = panes.iter().find(|pane| pane.editor_id() == origin) else {
        return Vec::new();
    };
    let word_char = query.chars().next();
    let mut candidates = Vec::new();
    for pane in panes {
        let buffer = pane.snapshot.buffer_snapshot();
        let is_origin = pane.editor_id() == origin;
        let mut push = |row: &RowText, range: Range<MultiBufferOffset>| {
            if is_origin && range.start == pane.cursor {
                return;
            }
            let screen = pane.screen_point(row, range.start);
            // The row is laid out whole: a match scrolled off to the left or past the right edge
            // is not on visible text.
            if !pane.x_visible.contains(&screen.x) {
                return;
            }
            let start = buffer.anchor_after(range.start);
            let end = if range.end == range.start {
                start
            } else {
                buffer.anchor_before(range.end)
            };
            candidates.push(Candidate {
                editor: pane.editor.clone(),
                range: start..end,
                screen,
            });
        };
        for row in &pane.rows {
            match mode {
                Mode::Line => {
                    if let Some(start) = row.line_start {
                        push(row, start..start);
                    }
                }
                Mode::CharTimer => {
                    for segment in &row.segments {
                        for found in char_matches(query, &segment.text) {
                            push(row, segment.start + found.start..segment.start + found.end);
                        }
                    }
                }
                Mode::Word => {
                    let Some(ch) = word_char else {
                        continue;
                    };
                    for segment in &row.segments {
                        for offset in word_starts(ch, &segment.text, segment.before) {
                            let len = segment.text[offset..]
                                .chars()
                                .next()
                                .map_or(0, char::len_utf8);
                            let start = segment.start + offset;
                            push(row, start..start + len);
                        }
                    }
                }
            }
        }
    }
    let points: Vec<ScreenPoint> = candidates
        .iter()
        .map(|candidate| candidate.screen)
        .collect();
    let mut slots: Vec<Option<Candidate>> = candidates.into_iter().map(Some).collect();
    rank(origin_pane.cursor_screen, &points)
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}
