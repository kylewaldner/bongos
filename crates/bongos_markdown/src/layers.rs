//! The live view's layers and the window they work on (design D8). A layer draws one kind of
//! decoration (styles now; conceals, images and tables later) from the index over the window: the
//! buffer rows on screen plus one screen above and one below. The addon refreshes every layer on a
//! scroll, a selection change, a new index and a resize, and clears every layer in source mode and
//! on detach.

use std::ops::Range;

use bongos_markdown_model::MarkdownIndex;
use editor::display_map::DisplayRow;
use editor::{Bias, DisplayPoint, Editor};
use gpui::Context;

use crate::styles::StyleLayer;

/// The screen height, in rows, used before the editor's first layout tells the real one.
pub const FALLBACK_SCREEN_ROWS: u32 = 40;

/// What a layer refreshes from.
pub struct LiveContext<'a> {
    /// The index in the offsets of `snapshot`.
    pub index: &'a MarkdownIndex,
    /// Bumped on every new parse result.
    pub generation: u64,
    pub snapshot: &'a language::BufferSnapshot,
    /// The window, in buffer offsets of `snapshot`.
    pub window: Range<usize>,
    /// Set when something besides the index, the text and the window changed (the theme, the
    /// mode), so a layer must not skip the refresh on its own cache.
    pub force: bool,
}

/// One kind of live decoration.
pub trait LiveLayer {
    /// Brings the layer's decorations in line with the index over the window.
    fn refresh(&mut self, live: &LiveContext<'_>, editor: &mut Editor, cx: &mut Context<Editor>);

    /// Removes everything the layer put on the editor.
    fn clear(&mut self, editor: &mut Editor, cx: &mut Context<Editor>);
}

/// The layers a new addon starts with, in refresh order.
pub fn default_layers() -> Vec<Box<dyn LiveLayer>> {
    vec![Box::new(StyleLayer::default())]
}

/// The window of `editor`: the buffer rows on screen plus one screen above and one below, as
/// buffer offsets.
pub fn window(editor: &mut Editor, cx: &mut Context<Editor>) -> Range<usize> {
    let display = editor.display_snapshot(cx);
    let top = editor.scroll_position(cx).y.max(0.0).floor() as u32;
    let screen = editor
        .visible_line_count()
        .map_or(FALLBACK_SCREEN_ROWS, |lines| lines.ceil() as u32)
        .max(1);
    let max_row = display.max_point().row().0;
    let first = top.saturating_sub(screen).min(max_row);
    let last = top.saturating_add(screen.saturating_mul(2));
    let buffer = display.buffer_snapshot();
    let start = display.display_point_to_point(DisplayPoint::new(DisplayRow(first), 0), Bias::Left);
    let start = buffer.point_to_offset(start).0;
    let end = if last > max_row {
        buffer.len().0
    } else {
        let end =
            display.display_point_to_point(DisplayPoint::new(DisplayRow(last), 0), Bias::Left);
        buffer.point_to_offset(end).0
    };
    start..end.max(start)
}
