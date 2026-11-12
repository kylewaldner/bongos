//! Yank and yank-pop. A yank inserts a kill ring entry at every caret as one undo step and
//! remembers, as anchors, the text it inserted; a yank-pop right after replaces exactly that
//! text with another entry. Neither writes the system clipboard.

use bongos_emacs_model::kill_ring::Entry;
use bongos_minibuffer::message;
use editor::{ClipboardSelection, Editor, EditorSettings, SelectionEffects, kill_ring};
use gpui::{Action as _, App, ClipboardEntry, Context, Entity, EntityId, Global, Window, WindowId};
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};
use settings::Settings as _;
use text::{SelectionGoal, TransactionId};

use crate::actions::{Yank, YankPop};
use crate::last_command::last_command;
use crate::mark;

/// What the last yank or yank-pop inserted, so a yank-pop directly after it can replace it.
struct Yanked {
    window: WindowId,
    editor: EntityId,
    /// Per caret, the inserted text: a start anchor biased left, so the text inserted at it (and
    /// indentation added later by auto-indent) lands inside, and an end anchor biased left, so
    /// text typed after it stays outside.
    ranges: Vec<(Anchor, Anchor)>,
    /// The ring index of the inserted entry.
    index: usize,
}

#[derive(Default)]
struct YankState(Option<Yanked>);

impl Global for YankState {}

/// Forgets the last yank when it was in `editor`. Called on a mouse selection there.
pub(crate) fn forget_yank_in(editor: EntityId, cx: &mut App) {
    if cx
        .try_global::<YankState>()
        .and_then(|state| state.0.as_ref())
        .is_some_and(|yanked| yanked.editor == editor)
    {
        cx.set_global(YankState(None));
    }
}

/// `yank`: inserts the newest entry (the Nth newest with a count; 0 is the oldest, negatives
/// wrap) at every caret, with the mark at the insert start and point after the text. Without
/// `delete_selection` an active region is deactivated, not replaced.
pub(crate) fn yank_command(
    action: &Yank,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if editor.read(cx).read_only(cx) {
        message(window, cx, "Buffer is read-only");
        return;
    }
    kill_ring::capture_clipboard(cx);
    let Some((index, entry)) = entry_at(cx, |ring| ring.yank_index(action.count)) else {
        message(window, cx, "Kill ring is empty");
        return;
    };
    let replaces_region = EditorSettings::get_global(cx).typing_replaces_selection;
    let starts = editor.update(cx, |editor, cx| {
        editor.set_selection_mark_mode(false, cx);
        if !replaces_region {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                s.move_with(&mut |_, selection| {
                    selection.collapse_to(selection.head(), SelectionGoal::None)
                });
            });
        }
        let display = editor.display_snapshot(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let newest = editor
            .selections
            .newest::<MultiBufferOffset>(&display)
            .start;
        let starts = editor
            .selections
            .all::<MultiBufferOffset>(&display)
            .iter()
            .map(|selection| snapshot.anchor_before(selection.start))
            .collect::<Vec<_>>();
        (snapshot.anchor_before(newest), starts)
    });
    let (mark_at, starts) = starts;
    mark::push_mark(editor, Some(mark_at), false, window, cx);
    if let Some(ranges) = insert(editor, &entry, starts, None, window, cx) {
        remember(editor, window, ranges, index, cx);
    }
}

/// `yank-pop`: right after a yank or yank-pop in this editor, replaces the text it inserted
/// with the next older entry (N older with a count, negative toward newer, wrapping). The mark,
/// the ring order and the clipboard are left alone.
pub(crate) fn yank_pop_command(
    action: &YankPop,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let after_yank = matches!(
        last_command(window, cx),
        Some(name) if name == Yank::name_for_type() || name == YankPop::name_for_type()
    );
    let window_id = window.window_handle().window_id();
    let yanked = cx
        .try_global::<YankState>()
        .and_then(|state| state.0.as_ref())
        .filter(|yanked| yanked.window == window_id && yanked.editor == editor.entity_id())
        .map(|yanked| (yanked.ranges.clone(), yanked.index));
    let Some((ranges, from)) = yanked.filter(|_| after_yank) else {
        message(window, cx, "Previous command was not a yank");
        return;
    };
    if editor.read(cx).read_only(cx) {
        message(window, cx, "Buffer is read-only");
        return;
    }
    let Some((index, entry)) = entry_at(cx, |ring| ring.pop_index(from, action.count)) else {
        message(window, cx, "Kill ring is empty");
        return;
    };
    let starts = ranges.iter().map(|(start, _)| *start).collect();
    let Some(ranges) = insert(editor, &entry, starts, Some(&ranges), window, cx) else {
        // Touching ranges would merge into one selection and misplace the pieces.
        cx.set_global(YankState(None));
        message(window, cx, "Previous command was not a yank");
        return;
    };
    remember(editor, window, ranges, index, cx);
}

/// The ring entry at the index `pick` chooses, with that index.
fn entry_at(
    cx: &App,
    pick: impl FnOnce(&bongos_emacs_model::kill_ring::Ring) -> Option<usize>,
) -> Option<(usize, Entry)> {
    let ring = kill_ring::read(cx)?;
    let index = pick(ring)?;
    Some((index, ring.get(index)?.clone()))
}

/// Pastes `entry` over every selection (first selecting `replace`, when given) as one undo
/// step, one piece per caret when the entry has one per caret. Leaves each caret after its
/// text and returns the inserted ranges, from `starts` (one per selection, in order) to a
/// left-biased anchor at each caret. `None`, with nothing changed, when `replace` does not
/// select one range each.
fn insert(
    editor: &Entity<Editor>,
    entry: &Entry,
    starts: Vec<Anchor>,
    replace: Option<&[(Anchor, Anchor)]>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Vec<(Anchor, Anchor)>> {
    let item = kill_ring::clipboard_item(entry);
    let pieces = item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::String(string) => string.metadata_json::<Vec<ClipboardSelection>>(),
        _ => None,
    });
    editor.update(cx, |editor, cx| {
        editor.finalize_last_transaction(cx);
        let mut ends = None;
        // Selecting `replace` inside the transaction makes undo put the carets back where
        // they were, without a region.
        let transaction = editor.transact(window, cx, |editor, window, cx| {
            if let Some(replace) = replace {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let offsets = replace
                    .iter()
                    .map(|(start, end)| start.to_offset(&snapshot)..end.to_offset(&snapshot))
                    .collect::<Vec<_>>();
                let mut merged = false;
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    let before = s.disjoint_anchors_arc();
                    s.select_ranges(offsets);
                    if s.count() != replace.len() {
                        merged = true;
                        s.select_anchors(before.to_vec());
                    }
                });
                if merged {
                    return;
                }
            }
            editor.do_paste(&entry.text, pieces, false, window, cx);
            // Anchors, not offsets: auto-indent may still change the text before the
            // transaction ends.
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let display = editor.display_snapshot(cx);
            let anchors = editor
                .selections
                .all::<MultiBufferOffset>(&display)
                .iter()
                .map(|selection| snapshot.anchor_before(selection.end))
                .collect::<Vec<_>>();
            editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                s.select_anchor_ranges(anchors.iter().map(|&end| end..end))
            });
            ends = Some(anchors);
        });
        if let Some(transaction) = transaction {
            // Auto-indent applied once the transaction ended is a transaction of its own.
            editor.group_until_transaction(transaction, cx);
        }
        editor.finalize_last_transaction(cx);
        if let Some(transaction) = transaction {
            group_late_auto_indent(editor, transaction, cx);
        }
        let ends = ends?;
        Some(starts.into_iter().zip(ends).collect())
    })
}

/// Auto-indent that waits for a background reparse lands after the paste's transaction was
/// finalized, as a transaction of its own. Folds it into the paste's once applied, so one undo removes both.
fn group_late_auto_indent(
    editor: &mut Editor,
    transaction: TransactionId,
    cx: &mut Context<Editor>,
) {
    let waits = editor
        .buffer()
        .read(cx)
        .all_buffers()
        .into_iter()
        .filter_map(|buffer| buffer.update(cx, |buffer, _| buffer.wait_for_autoindent_applied()))
        .collect::<Vec<_>>();
    if waits.is_empty() {
        return;
    }
    cx.spawn(async move |editor, cx| {
        for wait in waits {
            wait.await.ok();
        }
        editor
            .update(cx, |editor, cx| {
                editor.group_until_transaction(transaction, cx)
            })
            .ok();
    })
    .detach();
}

fn remember(
    editor: &Entity<Editor>,
    window: &Window,
    ranges: Vec<(Anchor, Anchor)>,
    index: usize,
    cx: &mut App,
) {
    cx.set_global(YankState(Some(Yanked {
        window: window.window_handle().window_id(),
        editor: editor.entity_id(),
        ranges,
        index,
    })));
}
