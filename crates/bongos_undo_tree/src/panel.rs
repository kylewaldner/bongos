//! The undo tree panel: one row per undo state of a buffer, drawn beside it, with a cursor that
//! moves a live preview of the buffer between states.
//!
//! The panel is its own focusable view, not an editor, so no key types into it. It lives in a
//! dedicated pane of its own. Quitting it, by any path (its key, closing its tab or pane, the
//! previewed pane showing another item, the buffer or editor going away, or the panel being moved
//! into another pane), closes the preview and removes the panel.
//!
//! `d` shows the diff from the marked state (or the state the tree was opened at) to the state
//! under the cursor in the lower half of the panel; `RET` commits the state under the cursor to
//! the buffer as one undoable step and closes the panel.

use std::{ops::Range, rc::Rc, time::Instant};

use bongos_undo_tree_model::{
    NO_OTHER_BRANCH, Now, Row, backward, forward, layout, next, previous, relative_time,
};
use editor::Editor;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global, ScrollStrategy,
    SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use language::Buffer;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    MinibufferMessageKind, MinibufferRouting, Pane,
    item::{Item, ItemEvent},
    pane,
};

use crate::{
    actions::{
        VundoBackward, VundoConfirm, VundoDiff, VundoForward, VundoMark, VundoNext, VundoPrevious,
        VundoQuit, VundoUnmark,
    },
    adapter::{TreeShape, shape_of},
    commit::{CommitRequest, NOTHING_COMMITTED_MESSAGE},
    diff::DiffView,
    preview::{CloseReason, PreviewEvent, PreviewSession},
};

/// The key context of the panel; its keys are bound in this context only.
pub const KEY_CONTEXT: &str = "UndoTree";

struct SystemNow;

impl Now for SystemNow {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The clock the panel's relative times are read against. Without one installed the panel uses
/// the system clock; a test installs a fixed one.
pub struct UndoTreeClock(Rc<dyn Now>);

impl Global for UndoTreeClock {}

impl UndoTreeClock {
    /// Makes `now` the clock of every panel's next refresh.
    pub fn set(cx: &mut App, now: Rc<dyn Now>) {
        cx.set_global(Self(now));
    }

    /// The current instant on the installed clock.
    pub fn now(cx: &App) -> Instant {
        cx.try_global::<Self>()
            .map_or_else(|| SystemNow.now(), |clock| clock.0.now())
    }
}

/// Events of an [`UndoTreePanel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoTreePanelEvent {
    /// The panel quit; its tab closes wherever it is.
    Close,
}

#[derive(Clone, Copy)]
enum Step {
    Forward,
    Backward,
    Next,
    Previous,
}

/// The undo tree of one buffer, beside the pane that previews it.
pub struct UndoTreePanel {
    focus_handle: FocusHandle,
    session: Entity<PreviewSession>,
    buffer: WeakEntity<Buffer>,
    buffer_id: EntityId,
    home_pane_id: EntityId,
    title: SharedString,
    tree: TreeShape,
    cursor: usize,
    rows: Vec<Row>,
    now: Instant,
    layout_passes: usize,
    quitting: bool,
    scroll_handle: UniformListScrollHandle,
    diff: Option<DiffView>,
    commit: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl UndoTreePanel {
    /// A panel over `session`, the preview of `editor`'s `buffer` in `editor_pane`. The panel is
    /// meant for the pane `home_pane_id`; added to any other pane it quits.
    pub fn new(
        session: Entity<PreviewSession>,
        editor: &Entity<Editor>,
        buffer: &Entity<Buffer>,
        editor_pane: &Entity<Pane>,
        home_pane_id: EntityId,
        cx: &mut Context<Self>,
    ) -> Self {
        let now = UndoTreeClock::now(cx);
        let (entry, preview_node) = {
            let session = session.read(cx);
            (session.entry(), session.preview_node())
        };
        let buffer_ref = buffer.read(cx);
        let tree = shape_of(buffer_ref.undo_tree(), entry, now);
        let file_name = buffer_ref
            .file()
            .map(|file| file.file_name(cx).to_string())
            .unwrap_or_else(|| "untitled".to_string());
        let cursor = tree.index(preview_node).unwrap_or(tree.shape.entry);
        let editor_id = editor.entity_id();
        let subscriptions = vec![
            cx.subscribe(&session, |this, _, event: &PreviewEvent, cx| match event {
                // A commit closes the preview first; the panel closes once it lands or aborts.
                PreviewEvent::Closed(CloseReason::Commit) => {}
                PreviewEvent::Closed(_) => this.quit(cx),
            }),
            cx.subscribe(editor_pane, move |this, pane, event: &pane::Event, cx| {
                let leaves_editor = match event {
                    pane::Event::ActivateItem { .. } => !pane
                        .read(cx)
                        .active_item()
                        .is_some_and(|item| item.item_id() == editor_id),
                    pane::Event::RemovedItem { item } => item.item_id() == editor_id,
                    _ => false,
                };
                if leaves_editor {
                    this.quit(cx);
                }
            }),
            cx.on_release(|this, cx| this.session.update(cx, |session, cx| session.close(cx))),
        ];
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            session,
            buffer: buffer.downgrade(),
            buffer_id: buffer.entity_id(),
            home_pane_id,
            title: format!("Undo tree: {file_name}").into(),
            tree,
            cursor,
            rows: Vec::new(),
            now,
            layout_passes: 0,
            quitting: false,
            scroll_handle: UniformListScrollHandle::new(),
            diff: None,
            commit: None,
            _subscriptions: subscriptions,
        };
        panel.refresh(cx);
        panel
    }

    /// The buffer whose undo tree this is.
    pub fn buffer_id(&self) -> EntityId {
        self.buffer_id
    }

    /// The preview the panel drives.
    pub fn session(&self) -> &Entity<PreviewSession> {
        &self.session
    }

    /// The rows the panel shows, top to bottom.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The state under the cursor, as an index into the tree's shape.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The marked state, as an index into the tree's shape.
    pub fn mark(&self) -> Option<usize> {
        self.tree.shape.mark
    }

    /// The undo tree the panel shows.
    pub fn tree(&self) -> &TreeShape {
        &self.tree
    }

    /// How many times the rows were laid out; each lay-out is one pass over the states.
    pub fn layout_passes(&self) -> usize {
        self.layout_passes
    }

    /// The diff the panel shows, if `d` was pressed.
    pub fn diff_text(&self, cx: &App) -> Option<String> {
        self.diff.as_ref().map(|diff| diff.text(cx))
    }

    /// Whether a commit is being prepared.
    pub fn is_committing(&self) -> bool {
        self.commit.is_some()
    }

    /// The text of the row at `index`: the branch columns and the mark, then the relative time;
    /// the note row is its note alone and the root has no time.
    pub fn row_text(&self, index: usize) -> Option<(String, Option<String>)> {
        let row = self.rows.get(index)?;
        if let Some(note) = &row.note {
            return Some((note.clone(), None));
        }
        let lead = format!("{}{}", row.glyphs, row.mark.symbol());
        let time = row
            .last_edit_at
            .filter(|_| row.node != Some(0))
            .map(|then| relative_time(then, self.now));
        Some((lead, time))
    }

    /// Closes the preview and the panel. Quitting twice does nothing.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.session.update(cx, |session, cx| session.close(cx));
        cx.emit(UndoTreePanelEvent::Close);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.now = UndoTreeClock::now(cx);
        self.tree.shape.current = self.cursor;
        self.rows = layout(&self.tree.shape);
        self.layout_passes += 1;
        if let Some(row) = self
            .rows
            .iter()
            .position(|row| row.node == Some(self.cursor))
        {
            self.scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        if self.commit.is_some() {
            return;
        }
        // `forward` follows the path to the shape's current state; while the preview is open
        // that is the state the buffer is really in, the entry state.
        self.tree.shape.current = self.tree.shape.entry;
        let shape = &self.tree.shape;
        let target = match step {
            Step::Forward => forward(shape, self.cursor),
            Step::Backward => backward(shape, self.cursor),
            Step::Next => next(shape, self.cursor),
            Step::Previous => previous(shape, self.cursor),
        };
        self.tree.shape.current = self.cursor;
        let Some(target) = target else {
            if matches!(step, Step::Next | Step::Previous) {
                MinibufferRouting::message(
                    window,
                    cx,
                    NO_OTHER_BRANCH,
                    MinibufferMessageKind::Message,
                );
            }
            return;
        };
        let Some(id) = self.tree.id(target) else {
            return;
        };
        let moved = self
            .session
            .update(cx, |session, cx| session.move_to(id, window, cx))
            .log_err();
        if moved.is_some() {
            self.cursor = target;
            self.refresh(cx);
        }
    }

    fn set_mark(&mut self, _: &VundoMark, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.shape.mark = Some(self.cursor);
        self.refresh(cx);
    }

    fn clear_mark(&mut self, _: &VundoUnmark, _: &mut Window, cx: &mut Context<Self>) {
        if self.tree.shape.mark == Some(self.cursor) {
            self.tree.shape.mark = None;
            self.refresh(cx);
        }
    }

    /// Shows the diff from the marked state, or the entry state when nothing is marked, to the
    /// state under the cursor. Each `d` computes it again.
    fn show_diff(&mut self, _: &VundoDiff, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting || self.commit.is_some() {
            return;
        }
        let Some(buffer) = self.buffer.upgrade() else {
            return;
        };
        let from = self.tree.shape.mark.unwrap_or(self.tree.shape.entry);
        let (Some(from), Some(to)) = (self.tree.id(from), self.tree.id(self.cursor)) else {
            return;
        };
        if self.diff.is_none() {
            let current = buffer.read(cx).undo_tree().current();
            self.diff = Some(DiffView::new(&buffer, current, window, cx));
        }
        if let Some(diff) = self.diff.as_mut() {
            diff.show(&buffer, from, to, cx).log_err();
        }
        cx.notify();
    }

    /// Commits the state under the cursor to the buffer and closes the panel. The buffer gets
    /// its capability back first; the new text is diffed against it in the background and
    /// applied as one transaction when the buffer has not changed since the panel opened. On the
    /// entry state this closes the panel and changes nothing.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting || self.commit.is_some() {
            return;
        }
        let target = self.tree.id(self.cursor);
        let request = {
            let session = self.session.read(cx);
            let editor = session.editor().upgrade();
            match (target, self.buffer.upgrade(), editor) {
                (Some(target), Some(buffer), Some(editor))
                    if target != session.entry() && session.closed_reason().is_none() =>
                {
                    debug_assert_eq!(target, session.preview_node());
                    let text = session.copy().read(cx).text();
                    let request = CommitRequest {
                        buffer,
                        editor,
                        entry_version: session.entry_version().clone(),
                        selections: session.node_selections(target, cx),
                    };
                    Some((request, text))
                }
                _ => None,
            }
        };
        let Some((request, text)) = request else {
            self.quit(cx);
            return;
        };
        self.session
            .update(cx, |session, cx| session.close_for_commit(cx));
        let diff = request.buffer.read(cx).diff(text, cx);
        self.commit = Some(cx.spawn_in(window, async move |this, cx| {
            let diff = diff.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_commit(request, diff, window, cx)
            })
            .ok();
        }));
    }

    fn finish_commit(
        &mut self,
        request: CommitRequest,
        diff: language::Diff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting {
            return;
        }
        if request.is_current(cx) {
            request.apply(diff, UndoTreeClock::now(cx), window, cx);
        } else {
            MinibufferRouting::message(
                window,
                cx,
                NOTHING_COMMITTED_MESSAGE,
                MinibufferMessageKind::Message,
            );
        }
        self.quit(cx);
    }

    fn render_rows(&self, range: Range<usize>, cx: &App) -> Vec<AnyElement> {
        let colors = cx.theme().colors();
        range
            .filter_map(|index| {
                let row = self.rows.get(index)?;
                let (lead, time) = self.row_text(index)?;
                let is_cursor = row.node == Some(self.cursor);
                let lead_color = if row.note.is_some() {
                    colors.text_muted
                } else {
                    colors.text
                };
                Some(
                    h_flex()
                        .id(index)
                        .w_full()
                        .px_2()
                        .gap_2()
                        .when(is_cursor, |this| this.bg(colors.element_selected))
                        .child(div().text_color(lead_color).child(lead))
                        .when_some(time, |this, time| {
                            this.child(div().text_color(colors.text_muted).child(time))
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl EventEmitter<UndoTreePanelEvent> for UndoTreePanel {}

impl Focusable for UndoTreePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UndoTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &VundoForward, window, cx| {
                this.step(Step::Forward, window, cx)
            }))
            .on_action(cx.listener(|this, _: &VundoBackward, window, cx| {
                this.step(Step::Backward, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &VundoNext, window, cx| this.step(Step::Next, window, cx)),
            )
            .on_action(cx.listener(|this, _: &VundoPrevious, window, cx| {
                this.step(Step::Previous, window, cx)
            }))
            .on_action(cx.listener(Self::set_mark))
            .on_action(cx.listener(Self::clear_mark))
            .on_action(cx.listener(|this, _: &VundoQuit, _, cx| this.quit(cx)))
            .on_action(cx.listener(Self::show_diff))
            .on_action(cx.listener(|this, _: &VundoConfirm, window, cx| this.confirm(window, cx)))
            .child(
                div().flex_1().min_h_0().w_full().child(
                    uniform_list(
                        "undo-tree-rows",
                        self.rows.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            this.render_rows(range, cx)
                        }),
                    )
                    .size_full()
                    .font_buffer(cx)
                    .track_scroll(&self.scroll_handle),
                ),
            )
            .when_some(self.diff.as_ref(), |this, diff| {
                this.child(
                    div()
                        .h_1_2()
                        .w_full()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(diff.editor().clone()),
                )
            })
    }
}

impl Item for UndoTreePanel {
    type Event = UndoTreePanelEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            UndoTreePanelEvent::Close => f(ItemEvent::CloseItem),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn include_in_nav_history() -> bool {
        false
    }

    /// The panel belongs beside its buffer only: moved into another pane (deleting its window
    /// hands its tabs to the neighbor) it quits, so no orphan panel tab is left behind.
    fn pane_changed(&mut self, new_pane_id: EntityId, cx: &mut Context<Self>) {
        if new_pane_id != self.home_pane_id {
            self.quit(cx);
        }
    }
}
