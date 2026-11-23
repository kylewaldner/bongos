//! A quiet live preview of a buffer's undo history.
//!
//! The preview never edits the real buffer. It shows a read-only editor over a detached copy of
//! the buffer (no file, no project) in place of the real editor's content, and moves that copy
//! between undo states. While it is open the real buffer is read-only, so no other editor edits
//! or undoes it, and its undo tree gains no node; nothing reaches a language server, autosave or
//! the disk. Closing it, by any path, puts back the real buffer's capability and lets the pane
//! draw the real editor again, whose text, selections and scroll were never touched.

use std::{ops::Range, sync::Arc};

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::{
    Anchor, Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll, scroll::ScrollOffset,
};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Point,
    Subscription, WeakEntity, Window,
};
use language::{Buffer, BufferEvent, Capability, Selection};
use text::{ToOffset as _, Transaction, UndoNodeId};
use workspace::{MinibufferMessageKind, MinibufferRouting, Pane};

/// The echo when an edit from outside the preview (a reload from disk, a remote edit) closes it.
pub const BUFFER_CHANGED_MESSAGE: &str = "Buffer changed; undo tree closed";

/// The echo when the preview is asked for a buffer that is not a single file.
pub const NEEDS_SINGLE_BUFFER_MESSAGE: &str = "Undo tree needs a single file buffer";

/// Why a preview closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// It was closed on purpose.
    Quit,
    /// The real buffer was edited from outside the preview.
    OutsideEdit,
    /// The real editor, its pane or the real buffer went away.
    Released,
}

/// Events of a [`PreviewSession`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewEvent {
    Closed(CloseReason),
}

/// The open previews, one per real buffer.
#[derive(Default)]
struct OpenPreviews(HashMap<EntityId, WeakEntity<PreviewSession>>);

impl Global for OpenPreviews {}

/// A preview of one buffer's undo states in one pane.
pub struct PreviewSession {
    session_id: EntityId,
    buffer: WeakEntity<Buffer>,
    buffer_id: EntityId,
    copy: Entity<Buffer>,
    editor: WeakEntity<Editor>,
    pane: WeakEntity<Pane>,
    window: AnyWindowHandle,
    preview_editor: Entity<Editor>,
    saved_capability: Capability,
    entry: UndoNodeId,
    entry_pinned: bool,
    entry_selections: Arc<[Selection<Anchor>]>,
    entry_scroll: Point<ScrollOffset>,
    preview_node: UndoNodeId,
    restored: bool,
    closed: Option<CloseReason>,
    undo_operation_calls: usize,
    last_move_toggles: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PreviewEvent> for PreviewSession {}

impl PreviewSession {
    /// Opens a preview of `editor`'s buffer in `pane`, which shows the preview editor instead
    /// of its active item until the preview closes. Focus does not move. A buffer that already
    /// has an open preview gets that preview back. Fails for an editor that is not over a
    /// single buffer.
    pub fn open(
        editor: &Entity<Editor>,
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let buffer = editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .context(NEEDS_SINGLE_BUFFER_MESSAGE)?;
        if let Some(existing) = Self::for_buffer(&buffer, cx) {
            return Ok(existing);
        }

        let (entry_selections, entry_scroll) = editor.update(cx, |editor, cx| {
            (
                editor.selections.disjoint_anchors_arc(),
                editor.scroll_position(cx),
            )
        });
        let (saved_capability, entry, entry_pinned, copy) = buffer.update(cx, |buffer, cx| {
            let saved_capability = buffer.capability();
            let entry = buffer.undo_tree().current();
            let entry_pinned = buffer.pin_undo_node(entry);
            let copy = buffer.detached_copy(cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            (saved_capability, entry, entry_pinned, copy)
        });

        let preview_editor = cx.new(|cx| {
            let mut preview_editor = Editor::for_buffer(copy.clone(), None, window, cx);
            preview_editor.set_read_only(true);
            preview_editor
        });
        let entry_ranges = resolve_selections(&entry_selections, &copy, cx);
        preview_editor.update(cx, |preview_editor, cx| {
            preview_editor.set_scroll_position(entry_scroll, window, cx);
            preview_editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges(entry_ranges)
            });
        });
        pane.update(cx, |pane, cx| {
            pane.set_content_override(Some(preview_editor.clone().into()), cx)
        });

        let window_handle = window.window_handle();
        let session = cx.new(|cx: &mut Context<Self>| {
            let subscriptions = vec![
                cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
                    if matches!(event, BufferEvent::Edited { .. }) {
                        this.close_with(CloseReason::OutsideEdit, cx);
                    }
                }),
                cx.observe_release(&buffer, |this, _, cx| {
                    this.close_with(CloseReason::Released, cx)
                }),
                cx.observe_release(editor, |this, _, cx| {
                    this.close_with(CloseReason::Released, cx)
                }),
                cx.observe_release(pane, |this, _, cx| {
                    this.close_with(CloseReason::Released, cx)
                }),
                cx.on_release(|this, cx| this.restore(cx)),
            ];
            Self {
                session_id: cx.entity_id(),
                buffer: buffer.downgrade(),
                buffer_id: buffer.entity_id(),
                copy,
                editor: editor.downgrade(),
                pane: pane.downgrade(),
                window: window_handle,
                preview_editor,
                saved_capability,
                entry,
                entry_pinned,
                entry_selections,
                entry_scroll,
                preview_node: entry,
                restored: false,
                closed: None,
                undo_operation_calls: 0,
                last_move_toggles: 0,
                _subscriptions: subscriptions,
            }
        });
        cx.default_global::<OpenPreviews>()
            .0
            .insert(buffer.entity_id(), session.downgrade());
        Ok(session)
    }

    /// The open preview of `buffer`, if any.
    pub fn for_buffer(buffer: &Entity<Buffer>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<OpenPreviews>()?
            .0
            .get(&buffer.entity_id())?
            .upgrade()
            .filter(|session| session.read(cx).closed.is_none())
    }

    /// Shows the state of `target` in the preview. The copy moves there with one undo
    /// operation, so its text changes at once; its syntax is reparsed in the background. The
    /// caret goes to the selections the node was left with, or for a node made elsewhere to
    /// the start of its first edit, and the view scrolls to it.
    pub fn move_to(
        &mut self,
        target: UndoNodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(self.closed.is_none(), "the undo tree preview is closed");
        let buffer = self
            .buffer
            .upgrade()
            .context("the previewed buffer is gone")?;
        let (counts, toggle_count, transaction) = {
            let tree = buffer.read(cx).undo_tree();
            let node = tree
                .node(target)
                .context("the undo state is not in the tree")?;
            let transaction = node.transaction().cloned();
            let toggles = tree.toggles(self.preview_node, target);
            let copy = self.copy.read(cx);
            let mut counts = HashMap::default();
            for id in &toggles {
                let Some(toggled) = tree.node(UndoNodeId(*id)).and_then(|n| n.transaction()) else {
                    continue;
                };
                for edit_id in &toggled.edit_ids {
                    counts.insert(*edit_id, copy.undo_count(*edit_id) + 1);
                }
            }
            (counts, toggles.len(), transaction)
        };
        if !counts.is_empty() {
            self.copy
                .update(cx, |copy, cx| copy.undo_operations(counts, cx));
            self.undo_operation_calls += 1;
        }
        self.last_move_toggles = toggle_count;
        self.preview_node = target;

        let ranges = self.selections_for(target, transaction.as_ref(), cx);
        self.preview_editor.update(cx, |preview_editor, cx| {
            preview_editor.change_selections(
                SelectionEffects::scroll(Autoscroll::newest()),
                window,
                cx,
                |s| s.select_ranges(ranges),
            );
        });
        cx.notify();
        Ok(())
    }

    /// Closes the preview: the pane draws its item again, the real buffer gets its capability
    /// back and the entry state's pin is released. Closing twice does nothing.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.close_with(CloseReason::Quit, cx);
    }

    /// The undo state the preview shows.
    pub fn preview_node(&self) -> UndoNodeId {
        self.preview_node
    }

    /// The undo state the real buffer was in when the preview opened.
    pub fn entry(&self) -> UndoNodeId {
        self.entry
    }

    /// The read-only editor over the copy.
    pub fn preview_editor(&self) -> &Entity<Editor> {
        &self.preview_editor
    }

    /// The detached copy the preview editor shows.
    pub fn copy(&self) -> &Entity<Buffer> {
        &self.copy
    }

    /// Why the preview closed, or `None` while it is open.
    pub fn closed_reason(&self) -> Option<CloseReason> {
        self.closed
    }

    /// The real editor's scroll position when the preview opened.
    pub fn entry_scroll(&self) -> Point<ScrollOffset> {
        self.entry_scroll
    }

    /// How many undo operations the copy has received over all moves; a move applies at most
    /// one.
    pub fn undo_operation_calls(&self) -> usize {
        self.undo_operation_calls
    }

    /// How many transactions the last move toggled.
    pub fn last_move_toggles(&self) -> usize {
        self.last_move_toggles
    }

    fn close_with(&mut self, reason: CloseReason, cx: &mut Context<Self>) {
        if self.closed.is_some() {
            return;
        }
        self.closed = Some(reason);
        self.restore(cx);
        if reason == CloseReason::OutsideEdit {
            let shown = self.window.update(cx, |_, window, cx| {
                MinibufferRouting::message(
                    window,
                    cx,
                    BUFFER_CHANGED_MESSAGE,
                    MinibufferMessageKind::Message,
                );
            });
            if shown.is_err() {
                MinibufferRouting::log_message(
                    BUFFER_CHANGED_MESSAGE,
                    MinibufferMessageKind::Message,
                );
            }
        }
        cx.emit(PreviewEvent::Closed(reason));
        cx.notify();
    }

    /// Undoes everything `open` did to things outside the preview. Runs once, on every close
    /// path, and when the session is dropped without closing.
    fn restore(&mut self, cx: &mut App) {
        if self.restored {
            return;
        }
        self.restored = true;
        let preview_id = self.preview_editor.entity_id();
        if let Some(pane) = self.pane.upgrade() {
            pane.update(cx, |pane, cx| {
                if pane
                    .content_override()
                    .is_some_and(|view| view.entity_id() == preview_id)
                {
                    pane.set_content_override(None, cx);
                }
            });
        }
        if let Some(buffer) = self.buffer.upgrade() {
            let capability = self.saved_capability;
            let entry = self.entry;
            let entry_pinned = self.entry_pinned;
            buffer.update(cx, |buffer, cx| {
                if entry_pinned {
                    buffer.unpin_undo_node(entry);
                }
                // Something else may have changed the capability while the preview was open
                // (a collaborator leaving makes it read-only for good, for instance); only the
                // read-only state the preview set is undone.
                if buffer.capability() == Capability::ReadOnly {
                    buffer.set_capability(capability, cx);
                }
            });
        }
        if let Some(open) = cx.try_global::<OpenPreviews>()
            && open
                .0
                .get(&self.buffer_id)
                .is_some_and(|session| session.entity_id() == self.session_id)
        {
            cx.global_mut::<OpenPreviews>().0.remove(&self.buffer_id);
        }
    }

    /// The selections to show for `target`, as offsets in the copy (a reversed selection has
    /// its start after its end).
    fn selections_for(
        &self,
        target: UndoNodeId,
        transaction: Option<&Transaction>,
        cx: &App,
    ) -> Vec<Range<MultiBufferOffset>> {
        let Some(transaction) = transaction.filter(|_| target != self.entry) else {
            return resolve_selections(&self.entry_selections, &self.copy, cx);
        };
        if let Some(editor) = self.editor.upgrade()
            && let Some((_, Some(after))) = editor.read(cx).transaction_selections(transaction.id)
        {
            return resolve_selections(&after, &self.copy, cx);
        }
        let start = self
            .copy
            .read(cx)
            .edited_ranges_for_transaction::<usize>(transaction)
            .next()
            .map_or(0, |range| range.start);
        vec![MultiBufferOffset(start)..MultiBufferOffset(start)]
    }
}

/// Resolves selections of the real editor in the copy, which shares the real buffer's id and
/// fragments, so the text anchors resolve there as they would in the real buffer.
fn resolve_selections(
    selections: &[Selection<Anchor>],
    copy: &Entity<Buffer>,
    cx: &App,
) -> Vec<Range<MultiBufferOffset>> {
    let snapshot = copy.read(cx).snapshot();
    selections
        .iter()
        .map(|selection| {
            let start = selection
                .start
                .text_anchor_in(&snapshot)
                .to_offset(&snapshot);
            let end = selection.end.text_anchor_in(&snapshot).to_offset(&snapshot);
            if selection.reversed {
                MultiBufferOffset(end)..MultiBufferOffset(start)
            } else {
                MultiBufferOffset(start)..MultiBufferOffset(end)
            }
        })
        .collect()
}
