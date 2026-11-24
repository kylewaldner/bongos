//! The preview of the highlighted row of a navigation picker.
//!
//! A [`PreviewSession`] lives as long as a picker and owns at most one pane slot (see
//! `Pane::set_preview`). The slot shows an open item's own view, or a transient read-only editor
//! over a file that is never a workspace item, or a one-line message. Nothing else in the
//! workspace changes: no item is activated, no navigation history is pushed and no real editor
//! scrolls or moves its selections. The slot is cleared when the picker confirms or cancels, when
//! the session is released, and when the pane holding it is removed.

use std::{ops::Range, rc::Rc};

use anyhow::Result;
use bongos_navigation_model::size;
use editor::{Editor, HighlightKey, SelectionEffects, scroll::Autoscroll};
use gpui::{AnyView, App, Entity, Subscription, Task, WeakEntity};
use language::{Buffer, Point};
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ItemHandle, Pane, Workspace, pane};

use crate::NavigationSettings;

/// The line a slot shows when a file cannot be opened.
pub const NO_PREVIEW: &str = "No preview";

/// What a picker tells its preview session.
pub enum Preview {
    /// A row is highlighted.
    Highlight(PreviewTarget),
    /// The picker has no row to highlight.
    NoCandidate,
    /// The picker confirmed its row; the slot is cleared before the caller acts.
    Confirmed,
    /// The picker was cancelled; the slot is cleared and the workspace is as before.
    Cancelled,
}

/// What a highlighted row previews.
pub enum PreviewTarget {
    /// An open item and the pane holding it.
    Item {
        pane: Entity<Pane>,
        item: Box<dyn ItemHandle>,
    },
    /// A file, with an optional row to center and match ranges to paint, in the pane the caller
    /// chose.
    Path {
        path: ProjectPath,
        row: Option<u32>,
        ranges: Vec<Range<Point>>,
        pane: Entity<Pane>,
    },
}

type LoadBuffer =
    Rc<dyn Fn(&Entity<Project>, ProjectPath, &mut App) -> Task<Result<Entity<Buffer>>>>;

struct Slot {
    pane: WeakEntity<Pane>,
    _subscriptions: [Subscription; 2],
}

struct PathEditor {
    path: ProjectPath,
    editor: Entity<Editor>,
}

/// The preview of one picker session.
pub struct PreviewSession {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    slot: Option<Slot>,
    path_editor: Option<PathEditor>,
    /// Bumped by every update, so a load that finishes for an older highlight is dropped.
    generation: usize,
    load: Option<Task<()>>,
    load_buffer: LoadBuffer,
}

impl PreviewSession {
    pub fn new(workspace: &Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| this.clear_slot(cx)).detach();
        Self {
            workspace: workspace.downgrade(),
            project: workspace.read(cx).project().clone(),
            slot: None,
            path_editor: None,
            generation: 0,
            load: None,
            load_buffer: Rc::new(|project, path, cx| {
                project.update(cx, |project, cx| project.open_buffer(path, cx))
            }),
        }
    }

    pub fn update(&mut self, preview: Preview, window: &mut Window, cx: &mut Context<Self>) {
        match preview {
            Preview::Highlight(PreviewTarget::Item { pane, item }) => {
                self.highlight_item(pane, item, cx)
            }
            Preview::Highlight(PreviewTarget::Path {
                path,
                row,
                ranges,
                pane,
            }) => self.highlight_path(path, row, ranges, pane, window, cx),
            Preview::NoCandidate => {
                self.forget_load();
                self.clear_slot(cx);
            }
            Preview::Confirmed => self.confirmed(cx),
            Preview::Cancelled => self.cancelled(cx),
        }
    }

    /// Clears the slot; call before acting on the confirmed row.
    pub fn confirmed(&mut self, cx: &mut Context<Self>) {
        self.end(cx);
    }

    /// Clears the slot, leaving the workspace as it was before the picker opened.
    pub fn cancelled(&mut self, cx: &mut Context<Self>) {
        self.end(cx);
    }

    /// The pane whose slot this session holds.
    pub fn slot_pane(&self) -> Option<Entity<Pane>> {
        self.slot.as_ref().and_then(|slot| slot.pane.upgrade())
    }

    /// The transient editor of the last previewed file.
    pub fn path_editor(&self) -> Option<Entity<Editor>> {
        self.path_editor
            .as_ref()
            .map(|path_editor| path_editor.editor.clone())
    }

    #[cfg(test)]
    pub(crate) fn set_load_buffer(&mut self, load_buffer: LoadBuffer) {
        self.load_buffer = load_buffer;
    }

    fn end(&mut self, cx: &mut Context<Self>) {
        self.forget_load();
        self.clear_slot(cx);
        self.path_editor = None;
    }

    fn forget_load(&mut self) -> usize {
        self.generation += 1;
        self.load = None;
        self.generation
    }

    fn highlight_item(
        &mut self,
        holding_pane: Entity<Pane>,
        item: Box<dyn ItemHandle>,
        cx: &mut Context<Self>,
    ) {
        self.forget_load();
        let Some(workspace) = self.workspace.upgrade() else {
            self.clear_slot(cx);
            return;
        };
        if workspace.read(cx).pane_is_visible(&holding_pane, cx) {
            let is_active = holding_pane
                .read(cx)
                .active_item()
                .is_some_and(|active| active.item_id() == item.item_id());
            if is_active {
                self.clear_slot(cx);
            } else {
                self.show(&holding_pane, item.to_any_view(), cx);
            }
        } else if let Some(center_pane) = active_center_pane(&workspace, cx) {
            self.show(&center_pane, item.to_any_view(), cx);
        } else {
            self.clear_slot(cx);
        }
    }

    fn highlight_path(
        &mut self,
        path: ProjectPath,
        row: Option<u32>,
        ranges: Vec<Range<Point>>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.forget_load();
        if let Some(path_editor) = &self.path_editor
            && path_editor.path == path
        {
            let editor = path_editor.editor.clone();
            show_row(&editor, row, &ranges, window, cx);
            self.show(&pane, editor.into(), cx);
            return;
        }

        let limit = NavigationSettings::get_global(cx).preview_max_file_size;
        let (known_size, abs_path, fs) = {
            let project = self.project.read(cx);
            (
                project.entry_for_path(&path, cx).map(|entry| entry.size),
                project.absolute_path(&path, cx),
                project.fs().clone(),
            )
        };
        if let Some(size) = known_size
            && !size::preview_allowed(size, limit)
        {
            self.show_message(&pane, size::skipped_label(size), cx);
            return;
        }

        self.load = Some(cx.spawn_in(window, async move |this, cx| {
            let size = match (known_size, abs_path) {
                (Some(size), _) => Some(size),
                (None, Some(abs_path)) => fs
                    .metadata(&abs_path)
                    .await
                    .log_err()
                    .flatten()
                    .map(|metadata| metadata.len),
                (None, None) => None,
            };
            if let Some(size) = size
                && !size::preview_allowed(size, limit)
            {
                this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.show_message(&pane, size::skipped_label(size), cx);
                    }
                })
                .log_err();
                return;
            }

            let Some(load) = this
                .update(cx, |this, cx| {
                    let load_buffer = this.load_buffer.clone();
                    load_buffer(&this.project, path.clone(), cx)
                })
                .log_err()
            else {
                return;
            };
            let buffer = load.await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match buffer {
                    Ok(buffer) => {
                        let editor = cx.new(|cx| transient_editor(buffer, window, cx));
                        show_row(&editor, row, &ranges, window, cx);
                        this.path_editor = Some(PathEditor {
                            path,
                            editor: editor.clone(),
                        });
                        this.show(&pane, editor.into(), cx);
                    }
                    Err(error) => {
                        log::debug!("no preview of {path:?}: {error:#}");
                        this.show_message(&pane, NO_PREVIEW.to_string(), cx);
                    }
                }
            })
            .log_err();
        }));
    }

    fn show_message(&mut self, pane: &Entity<Pane>, text: String, cx: &mut Context<Self>) {
        let message = cx.new(|_| PreviewMessage { text: text.into() });
        self.show(pane, message.into(), cx);
    }

    /// Shows `view` in `pane`'s slot, first clearing the slot this session holds in another pane.
    fn show(&mut self, pane: &Entity<Pane>, view: AnyView, cx: &mut Context<Self>) {
        let holds_pane = self
            .slot
            .as_ref()
            .is_some_and(|slot| slot.pane == pane.downgrade());
        if !holds_pane {
            self.clear_slot(cx);
            let subscriptions = [
                cx.subscribe(pane, |this, _, event: &pane::Event, cx| {
                    if matches!(event, pane::Event::Remove { .. }) {
                        this.end(cx);
                    }
                }),
                cx.observe_release(pane, |this, _, _| {
                    this.forget_load();
                    this.slot = None;
                }),
            ];
            self.slot = Some(Slot {
                pane: pane.downgrade(),
                _subscriptions: subscriptions,
            });
        }
        pane.update(cx, |pane, cx| pane.set_preview(Some(view), cx));
    }

    fn clear_slot(&mut self, cx: &mut App) {
        if let Some(slot) = self.slot.take()
            && let Some(pane) = slot.pane.upgrade()
        {
            pane.update(cx, |pane, cx| pane.set_preview(None, cx));
        }
    }
}

/// The active pane when it is in the center, else the center pane activated most recently.
fn active_center_pane(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<Pane>> {
    let workspace = workspace.read(cx);
    let center_panes = workspace.center_panes();
    let active = workspace.active_pane();
    if center_panes.contains(active) {
        return Some(active.clone());
    }
    center_panes
        .into_iter()
        .max_by_key(|pane| pane.read(cx).last_activation_timestamp())
}

fn transient_editor(
    buffer: Entity<Buffer>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Editor {
    let mut editor = Editor::for_buffer(buffer, None, window, cx);
    editor.set_read_only(true);
    editor.set_input_enabled(false);
    editor
}

/// Paints `ranges` and centers `row` (or the first range's row) in the transient editor.
fn show_row(
    editor: &Entity<Editor>,
    row: Option<u32>,
    ranges: &[Range<Point>],
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchors = ranges
            .iter()
            .map(|range| snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end))
            .collect::<Vec<_>>();
        editor.clear_background_highlights(HighlightKey::PickerPreview, cx);
        if !anchors.is_empty() {
            editor.highlight_background(
                HighlightKey::PickerPreview,
                &anchors,
                |_, theme| theme.colors().search_match_background,
                cx,
            );
        }
        let row = row
            .or_else(|| ranges.first().map(|range| range.start.row))
            .unwrap_or(0)
            .min(snapshot.max_point().row);
        let cursor = Point::new(row, 0);
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::center()).nav_history(false),
            window,
            cx,
            |selections| selections.select_ranges([cursor..cursor]),
        );
    });
}

/// A one-line slot: `No preview` or `Preview skipped: <size>`.
pub struct PreviewMessage {
    text: SharedString,
}

impl PreviewMessage {
    pub fn text(&self) -> &SharedString {
        &self.text
    }
}

impl Render for PreviewMessage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(Label::new(self.text.clone()).color(Color::Muted))
    }
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
