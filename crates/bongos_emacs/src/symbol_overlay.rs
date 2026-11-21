//! Symbol overlays: M-i highlights every whole-word occurrence of the symbol at point in a
//! persistent color, and M-i on a highlighted symbol removes it; M-I removes every highlight of
//! the buffer. The overlays belong to the buffer, so two editors on one buffer show the same
//! ones. They are not selections and change no text. Each editor recomputes the occurrences in
//! its visible range, extended one screen above and below, whenever an overlay is put or removed,
//! the editor scrolls, or a buffer is edited. The model lives in `bongos_emacs_model::symbol`.

use std::ops::Range;

use bongos_emacs_model::symbol::{occurrences, symbol_at};
use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent, HighlightKey};
use gpui::{App, BorrowAppContext as _, Context, Entity, Global, Window};
use language::BufferId;
use multi_buffer::{MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use text::Bias;

use crate::actions::{SymbolOverlayPut, SymbolOverlayRemoveAll};
use crate::lines::propagate_in_single_line;
use crate::snapshot_text::SnapshotText;

/// The most ranges one editor highlights.
pub(crate) const MAX_RANGES: usize = 1_000;

/// How much of the accent color the overlay shows. The full color would leave ordinary text
/// under 4.5 to 1 against it.
pub(crate) const OVERLAY_ALPHA: f32 = 0.12;

/// Rows assumed to be on screen before the first layout.
const DEFAULT_ROWS: f64 = 50.0;

/// One highlighted word and the palette index it shows in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Overlay {
    pub word: String,
    pub color_index: u32,
}

#[derive(Default)]
struct BufferOverlays {
    overlays: Vec<Overlay>,
    next_color: u32,
}

/// The overlays of every buffer.
#[derive(Default)]
pub(crate) struct SymbolOverlays {
    buffers: HashMap<BufferId, BufferOverlays>,
    watched: HashSet<BufferId>,
}

impl Global for SymbolOverlays {}

impl SymbolOverlays {
    /// Removes `word` from the buffer when it is there, else adds it with the next color.
    fn toggle(&mut self, buffer: BufferId, word: &str) {
        let entry = self.buffers.entry(buffer).or_default();
        if let Some(position) = entry.overlays.iter().position(|o| o.word == word) {
            entry.overlays.remove(position);
            return;
        }
        let color_index = entry.next_color;
        entry.next_color += 1;
        entry.overlays.push(Overlay {
            word: word.to_string(),
            color_index,
        });
    }

    fn clear(&mut self, buffer: BufferId) {
        self.buffers.remove(&buffer);
    }

    pub(crate) fn overlays(&self, buffer: BufferId) -> &[Overlay] {
        self.buffers
            .get(&buffer)
            .map_or(&[][..], |entry| &entry.overlays)
    }
}

/// Makes the editor redraw its overlays when they change, when it scrolls and when a buffer is
/// edited.
pub(crate) fn init_editor(cx: &mut Context<Editor>) {
    cx.observe_global::<SymbolOverlays>(|editor, cx| redraw(editor, cx))
        .detach();
    cx.subscribe_self(|editor, event: &EditorEvent, cx| {
        if matches!(
            event,
            EditorEvent::BufferEdited
                | EditorEvent::ScrollPositionChanged { .. }
                | EditorEvent::BufferRangesUpdated { .. }
                | EditorEvent::BuffersRemoved { .. }
        ) {
            redraw(editor, cx);
        }
    })
    .detach();
}

/// Sets the editor's overlay highlight from the store.
fn redraw(editor: &mut Editor, cx: &mut Context<Editor>) {
    let mut buffers = Vec::new();
    let has_overlays = cx.try_global::<SymbolOverlays>().is_some_and(|store| {
        editor.buffer().read(cx).for_each_buffer(&mut |buffer| {
            let id = buffer.read(cx).remote_id();
            if !store.overlays(id).is_empty() {
                buffers.push(id);
            }
        });
        !buffers.is_empty()
    });
    if !has_overlays {
        editor.clear_background_highlights(HighlightKey::SymbolOverlay, cx);
        return;
    }
    let window = visible_offsets(editor, cx);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let text = SnapshotText::new(&snapshot);
    let store = cx.global::<SymbolOverlays>();
    let mut found: Vec<(Range<usize>, u32)> = Vec::new();
    for buffer in &buffers {
        for overlay in store.overlays(*buffer) {
            for range in occurrences(&text, &overlay.word, window.clone(), MAX_RANGES) {
                if owner(&snapshot, range.start) == Some(*buffer) {
                    found.push((range, overlay.color_index));
                }
            }
        }
    }
    found.sort_by_key(|(range, _)| range.start);
    found.truncate(MAX_RANGES);
    let ranges: Vec<_> = found
        .iter()
        .map(|(range, _)| {
            snapshot.anchor_before(MultiBufferOffset(range.start))
                ..snapshot.anchor_after(MultiBufferOffset(range.end))
        })
        .collect();
    let colors: Vec<u32> = found.iter().map(|(_, color)| *color).collect();
    editor.highlight_background(
        HighlightKey::SymbolOverlay,
        &ranges,
        move |index, theme| overlay_color(theme, colors.get(*index).copied().unwrap_or(0)),
        cx,
    );
}

/// The color an overlay with palette index `color_index` shows in the theme.
pub(crate) fn overlay_color(theme: &theme::Theme, color_index: u32) -> gpui::Hsla {
    let mut color = theme.accents().color_for_index(color_index);
    color.a = OVERLAY_ALPHA;
    color
}

fn owner(snapshot: &MultiBufferSnapshot, offset: usize) -> Option<BufferId> {
    snapshot
        .point_to_buffer_offset(MultiBufferOffset(offset))
        .map(|(buffer, _)| buffer.remote_id())
}

/// The multi-buffer offsets of the visible rows, one screen more above and below.
fn visible_offsets(editor: &mut Editor, cx: &mut Context<Editor>) -> Range<usize> {
    let rows = editor.visible_line_count().unwrap_or(DEFAULT_ROWS).ceil();
    let top = editor.scroll_position(cx).y.floor();
    let display = editor.display_snapshot(cx);
    let first = (top - rows).max(0.0) as u32;
    let last = ((top + 2.0 * rows) as u32).min(display.max_point().row().0);
    let start = display.display_point_to_point(
        editor::display_map::DisplayPoint::new(editor::display_map::DisplayRow(first), 0),
        Bias::Left,
    );
    let end = display.display_point_to_point(
        editor::display_map::DisplayPoint::new(editor::display_map::DisplayRow(last), u32::MAX),
        Bias::Right,
    );
    let snapshot = display.buffer_snapshot();
    snapshot.point_to_offset(start).0..snapshot.point_to_offset(end).0
}

/// The buffer under the newest caret and the symbol at it.
fn symbol_at_caret(editor: &Editor, cx: &App) -> Option<(BufferId, String)> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let head = editor
        .selections
        .newest_anchor()
        .head()
        .to_offset(&snapshot)
        .0;
    let range = symbol_at(&SnapshotText::new(&snapshot), head)?;
    let word: String = snapshot
        .text_for_range(MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
        .collect();
    let buffer = owner(&snapshot, head)?;
    Some((buffer, word))
}

/// Drops a buffer's overlays when the buffer is released.
fn watch(editor: &Entity<Editor>, buffer: BufferId, cx: &mut App) {
    let first = cx.default_global::<SymbolOverlays>().watched.insert(buffer);
    if !first {
        return;
    }
    let Some(entity) = editor.read(cx).buffer().read(cx).buffer(buffer) else {
        return;
    };
    cx.observe_release(&entity, move |_, cx| {
        cx.update_default_global::<SymbolOverlays, _>(|store, _| {
            store.watched.remove(&buffer);
            store.clear(buffer);
        });
    })
    .detach();
}

pub(crate) fn symbol_overlay_put_command(
    _: &SymbolOverlayPut,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let Some((buffer, word)) = symbol_at_caret(editor.read(cx), cx) else {
        bongos_minibuffer::message(window, cx, "No symbol at point");
        return;
    };
    watch(editor, buffer, cx);
    cx.update_default_global::<SymbolOverlays, _>(|store, _| store.toggle(buffer, &word));
}

pub(crate) fn symbol_overlay_remove_all_command(
    _: &SymbolOverlayRemoveAll,
    editor: &Entity<Editor>,
    _: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let head = editor
        .read(cx)
        .selections
        .newest_anchor()
        .head()
        .to_offset(&snapshot)
        .0;
    let Some(buffer) = owner(&snapshot, head) else {
        return;
    };
    cx.default_global::<SymbolOverlays>();
    cx.update_default_global::<SymbolOverlays, _>(|store, _| store.clear(buffer));
}
