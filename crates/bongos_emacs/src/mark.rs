//! The mark and the mark ring. Both live per buffer in a global store, as buffer anchors, so
//! two editors showing one buffer share them and edits move them with their text. The ring
//! model lives in `bongos_emacs_model::mark_ring`.

use bongos_emacs_model::mark_ring::MarkRing;
use collections::HashMap;
use editor::{Editor, EditorEvent, SelectionEffects};
use gpui::{App, Context, Entity, Global, WeakEntity, Window};
use language::Buffer;
use multi_buffer::{Anchor, MultiBuffer, MultiBufferOffset, ToOffset as _};
use text::{BufferId, SelectionGoal, ToOffset as _};

use crate::actions::{ExchangePointAndMark, SetMarkCommand};
use crate::region;

/// One buffer's mark and ring. The ring never holds the current mark.
struct BufferMarks {
    buffer: WeakEntity<Buffer>,
    mark: Option<text::Anchor>,
    ring: MarkRing<text::Anchor>,
}

/// Every buffer's mark and ring, keyed by the `language::Buffer` id. Entries whose buffer is
/// gone are removed on each access.
#[derive(Default)]
pub struct MarkStore {
    buffers: HashMap<BufferId, BufferMarks>,
}

impl Global for MarkStore {}

/// The store, with the entries of dropped buffers removed.
fn store(cx: &mut App) -> &mut MarkStore {
    let store = cx.default_global::<MarkStore>();
    store
        .buffers
        .retain(|_, marks| marks.buffer.upgrade().is_some());
    store
}

/// A buffer position in the form the store keeps: attached to the character after it, so text
/// typed at the mark goes after the mark.
struct Place {
    buffer: Entity<Buffer>,
    snapshot: text::BufferSnapshot,
    anchor: text::Anchor,
}

/// Finds the buffer behind `anchor` and the matching `anchor_before` buffer anchor.
fn locate(multibuffer: &Entity<MultiBuffer>, anchor: Anchor, cx: &App) -> Option<Place> {
    let multibuffer = multibuffer.read(cx);
    let snapshot = multibuffer.snapshot(cx);
    let (text_anchor, _) = snapshot.anchor_to_buffer_anchor(anchor)?;
    let buffer = multibuffer.buffer(text_anchor.buffer_id)?;
    let snapshot = buffer.read(cx).text_snapshot();
    let anchor = snapshot.anchor_before(text_anchor.to_offset(&snapshot));
    Some(Place {
        buffer,
        snapshot,
        anchor,
    })
}

/// The buffer's entry, created when it has none, with the dead ring entries removed.
fn marks_mut<'a>(cx: &'a mut App, place: &Place) -> &'a mut BufferMarks {
    let marks = store(cx)
        .buffers
        .entry(place.snapshot.remote_id())
        .or_insert_with(|| BufferMarks {
            buffer: place.buffer.downgrade(),
            mark: None,
            ring: MarkRing::new(),
        });
    marks.ring.retain(|anchor| anchor.is_valid(&place.snapshot));
    marks
}

fn primary_cursor(editor: &Entity<Editor>, cx: &App) -> (Entity<MultiBuffer>, Anchor) {
    let editor = editor.read(cx);
    (
        editor.buffer().clone(),
        editor.selections.newest_anchor().head(),
    )
}

/// The mark of the buffer under the editor's primary cursor (the newest selection), as a
/// position in this editor. `None` when the buffer has no mark or the mark is not in one of the
/// editor's excerpts. A mark whose character was deleted resolves to where the deletion was.
pub fn mark_for(editor: &Entity<Editor>, cx: &mut App) -> Option<Anchor> {
    let (multibuffer, cursor) = primary_cursor(editor, cx);
    let place = locate(&multibuffer, cursor, cx)?;
    let mark = store(cx).buffers.get(&place.snapshot.remote_id())?.mark?;
    multibuffer.read(cx).snapshot(cx).anchor_in_excerpt(mark)
}

/// The buffer's ring under the editor's primary cursor, newest first, as positions in this
/// editor. Dead entries are dropped from the ring; entries outside the editor's excerpts are
/// kept in the ring but left out of the result.
pub fn ring_for(editor: &Entity<Editor>, cx: &mut App) -> Vec<Anchor> {
    let (multibuffer, cursor) = primary_cursor(editor, cx);
    let Some(place) = locate(&multibuffer, cursor, cx) else {
        return Vec::new();
    };
    let entries: Vec<text::Anchor> = {
        let marks = marks_mut(cx, &place);
        marks.ring.iter_newest_first().copied().collect()
    };
    let snapshot = multibuffer.read(cx).snapshot(cx);
    entries
        .into_iter()
        .filter_map(|anchor| snapshot.anchor_in_excerpt(anchor))
        .collect()
}

/// Sets the mark of the buffer behind `at`, without touching the ring. Returns whether `at` is
/// in a buffer.
pub fn set_mark(editor: &Entity<Editor>, at: Anchor, cx: &mut App) -> bool {
    let multibuffer = editor.read(cx).buffer().clone();
    set_mark_in(&multibuffer, at, cx)
}

fn set_mark_in(multibuffer: &Entity<MultiBuffer>, at: Anchor, cx: &mut App) -> bool {
    let Some(place) = locate(multibuffer, at, cx) else {
        return false;
    };
    marks_mut(cx, &place).mark = Some(place.anchor);
    true
}

/// Pushes the current mark onto the buffer's ring and sets the mark at `at` (the primary
/// cursor when `None`). Does nothing when mark mode is on and `activate` is false, so a jump
/// with a region extends the region. `activate` also turns mark mode on. Returns whether the
/// mark was set. The old mark is not pushed when it is where the newest ring entry or the new
/// mark is, or when its character was deleted.
pub fn push_mark(
    editor: &Entity<Editor>,
    at: Option<Anchor>,
    activate: bool,
    _window: &mut Window,
    cx: &mut App,
) -> bool {
    if !activate && editor.read(cx).selection_mark_mode() {
        return false;
    }
    let (multibuffer, cursor) = primary_cursor(editor, cx);
    let Some(place) = locate(&multibuffer, at.unwrap_or(cursor), cx) else {
        return false;
    };
    let marks = marks_mut(cx, &place);
    let snapshot = &place.snapshot;
    if let Some(old) = marks.mark.replace(place.anchor)
        && old.is_valid(snapshot)
        && old.to_offset(snapshot) != place.anchor.to_offset(snapshot)
    {
        marks.ring.push(old, |newest, old| {
            newest.to_offset(snapshot) == old.to_offset(snapshot)
        });
    }
    if activate {
        editor.update(cx, |editor, cx| editor.set_selection_mark_mode(true, cx));
    }
    true
}

/// Makes a mouse drag set the mark at the drag start, without a ring push. Shift-selection,
/// select-all and other selection changes have no pending selection and leave the mark alone.
pub(crate) fn init_editor(cx: &mut Context<Editor>) {
    cx.subscribe_self(|editor, event: &EditorEvent, cx| {
        let EditorEvent::SelectionsChanged { local: true } = event else {
            return;
        };
        let Some(pending) = editor.selections.pending_anchor() else {
            return;
        };
        if pending.start == pending.end {
            return;
        }
        let tail = pending.tail();
        let multibuffer = editor.buffer().clone();
        set_mark_in(&multibuffer, tail, cx);
    })
    .detach();
}

/// Whether any caret's region is active.
fn any_region_active(editor: &Entity<Editor>, cx: &mut App) -> bool {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selections = editor.selections.all::<MultiBufferOffset>(&display);
        selections
            .iter()
            .any(|selection| region::is_active(editor, selection, cx))
    })
}

/// Collapses every selection to its head.
fn collapse_to_heads(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
        s.move_with(&mut |_, selection| {
            selection.collapse_to(selection.head(), SelectionGoal::None)
        });
    });
}

/// `set-mark-command`: deactivates an active region (the mark stays), otherwise sets the mark
/// at the primary cursor, pushing the old one onto the ring, and turns mark mode on. With a
/// count it only sets, since popping the ring is not implemented yet.
pub(crate) fn set_mark_command(
    action: &SetMarkCommand,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if action.count.is_none() && any_region_active(editor, cx) {
        let outside_mark_mode = !editor.read(cx).selection_mark_mode();
        let tail = {
            let editor = editor.read(cx);
            let primary = editor.selections.newest_anchor();
            (outside_mark_mode && primary.start != primary.end).then(|| primary.tail())
        };
        if let Some(tail) = tail {
            set_mark(editor, tail, cx);
        }
        editor.update(cx, |editor, cx| {
            editor.set_selection_mark_mode(false, cx);
            collapse_to_heads(editor, window, cx);
        });
        bongos_minibuffer::message(window, cx, "Mark deactivated");
        return;
    }
    editor.update(cx, |editor, cx| collapse_to_heads(editor, window, cx));
    push_mark(editor, None, true, window, cx);
    if action.count.is_none() {
        bongos_minibuffer::message(window, cx, "Mark set");
    }
}

/// `exchange-point-and-mark`: with a region, swaps its ends; without one, selects from the
/// point to the mark with the cursor at the mark. Never touches the ring.
pub(crate) fn exchange_point_and_mark_command(
    _: &ExchangePointAndMark,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if any_region_active(editor, cx) {
        editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                s.move_with(&mut |_, selection| {
                    if selection.start != selection.end {
                        selection.reversed = !selection.reversed;
                    }
                });
            });
            editor.set_selection_mark_mode(true, cx);
        });
        let tail = editor.read(cx).selections.newest_anchor().tail();
        set_mark(editor, tail, cx);
        return;
    }
    let Some(mark) = mark_for(editor, cx) else {
        bongos_minibuffer::message(window, cx, "No mark set in this buffer");
        return;
    };
    let point = editor.read(cx).selections.newest_anchor().head();
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mark_offset = mark.to_offset(&snapshot);
        let point_offset = point.to_offset(&snapshot);
        let newest = editor.selections.newest_anchor().id;
        let display = editor.display_snapshot(cx);
        let mut selections = editor.selections.all::<MultiBufferOffset>(&display);
        for selection in &mut selections {
            if selection.id == newest {
                selection.start = mark_offset.min(point_offset);
                selection.end = mark_offset.max(point_offset);
                selection.reversed = mark_offset < point_offset;
                selection.goal = SelectionGoal::None;
            }
        }
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select(selections);
        });
        editor.set_selection_mark_mode(true, cx);
    });
    set_mark(editor, point, cx);
}
