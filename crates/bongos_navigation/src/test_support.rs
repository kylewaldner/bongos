//! Helpers for the tests of the navigation pickers.

use std::{ops::Range, path::PathBuf};

use editor::{Editor, scroll::ScrollOffset};
use gpui::{App, Entity, EntityId, Point as PixelPoint};
use language::Point;
use project::ProjectPath;
use workspace::Workspace;

/// Everything a picker must leave as it found it, to compare before it opens and after it ends.
#[derive(Clone, Debug, PartialEq)]
pub struct WorldSnapshot {
    /// Every pane that can hold items, in `Workspace::item_panes` order.
    pub panes: Vec<PaneSnapshot>,
    pub recent_navigation_history: Vec<(ProjectPath, Option<PathBuf>)>,
}

/// One pane.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSnapshot {
    pub pane: EntityId,
    pub items: Vec<EntityId>,
    pub active_index: usize,
    /// `(item, timestamp)` in the pane's activation order.
    pub activation_history: Vec<(EntityId, usize)>,
    /// Entries on the back and forward navigation stacks.
    pub nav_history: (usize, usize),
    pub tab_count: usize,
    /// The entity of the view in the pane's preview slot.
    pub preview: Option<EntityId>,
    pub editors: Vec<EditorSnapshot>,
}

/// One editor item.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorSnapshot {
    pub item: EntityId,
    pub selections: Vec<Range<Point>>,
    pub scroll_position: PixelPoint<ScrollOffset>,
}

impl WorldSnapshot {
    /// The same snapshot with every preview slot empty.
    pub fn without_previews(mut self) -> Self {
        for pane in &mut self.panes {
            pane.preview = None;
        }
        self
    }
}

/// Captures every pane of `workspace` and its recent navigation history.
pub fn world_snapshot(workspace: &Entity<Workspace>, cx: &mut App) -> WorldSnapshot {
    let panes = workspace.read(cx).item_panes(cx);
    let panes = panes
        .iter()
        .map(|pane| {
            let (mut snapshot, items) = {
                let pane_ref = pane.read(cx);
                let items = pane_ref
                    .items()
                    .map(|item| item.boxed_clone())
                    .collect::<Vec<_>>();
                let snapshot = PaneSnapshot {
                    pane: pane.entity_id(),
                    items: items.iter().map(|item| item.item_id()).collect(),
                    active_index: pane_ref.active_item_index(),
                    activation_history: pane_ref
                        .activation_history()
                        .iter()
                        .map(|entry| (entry.entity_id, entry.timestamp))
                        .collect(),
                    nav_history: pane_ref.nav_history().stack_depths(),
                    tab_count: pane_ref.items_len(),
                    preview: pane_ref.preview().map(|view| view.entity_id()),
                    editors: Vec::new(),
                };
                (snapshot, items)
            };
            snapshot.editors = items
                .iter()
                .filter_map(|item| {
                    let editor = item.act_as::<Editor>(cx)?;
                    Some(editor_snapshot(item.item_id(), &editor, cx))
                })
                .collect();
            snapshot
        })
        .collect();
    let recent_navigation_history = workspace.read(cx).recent_navigation_history(None, cx);
    WorldSnapshot {
        panes,
        recent_navigation_history,
    }
}

fn editor_snapshot(item: EntityId, editor: &Entity<Editor>, cx: &mut App) -> EditorSnapshot {
    editor.update(cx, |editor, cx| {
        let display_snapshot = editor.display_snapshot(cx);
        EditorSnapshot {
            item,
            selections: editor.selections.ranges::<Point>(&display_snapshot),
            scroll_position: editor.scroll_position(cx),
        }
    })
}
