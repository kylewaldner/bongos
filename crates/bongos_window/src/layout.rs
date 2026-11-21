//! Balancing the window (pane) sizes, swapping two windows and moving the current tab to another
//! window. None of them closes a tab or a buffer, except that a moved tab whose file the chosen
//! window already shows gives way to that window's tab, which shares the same buffer.

use gpui::{App, Context, Entity, EntityId, Focusable as _, Window};
use util::ResultExt as _;
use workspace::{Pane, Workspace, item::ItemHandle};

use crate::actions::{BalancePanes, MoveBufferToPane, SwapPanes};
use crate::picker::pick_pane;
use crate::targets::{clone_for_split, recent_other_item, targets};

const CENTER_ONLY: &str = "Pane commands act on the center panes";

/// Un-zooms, then gives every center window in a split the same share of it.
pub(crate) fn balance_panes(
    workspace: &mut Workspace,
    _: &BalancePanes,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
    }
    workspace.reset_pane_sizes(cx);
}

/// Un-zooms, then swaps the focused center window with the next one in tree order (wrapping),
/// tabs and all. Focus stays at the same place on screen, in the window now there. With one
/// center window nothing changes. A window in a dock changes nothing; the echo area says so.
pub(crate) fn swap_panes(
    workspace: &mut Workspace,
    _: &SwapPanes,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.focused_dock_position(window, cx).is_some() {
        bongos_minibuffer::message(window, cx, CENTER_ONLY);
        return;
    }
    let current = workspace.active_pane().clone();
    let panes = workspace.center_panes();
    let Some(position) = panes.iter().position(|pane| *pane == current) else {
        return;
    };
    if panes.len() < 2 {
        return;
    }
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
    }
    let next = panes[(position + 1) % panes.len()].clone();
    if workspace
        .swap_center_panes(&current, &next, cx)
        .log_err()
        .is_some()
    {
        window.focus(&next.focus_handle(cx), cx);
    }
}

/// Picks another window and moves the focused center window's current tab into it, focused.
/// The source window stays: it shows its most recently shown remaining tab, or when it has none,
/// a clone of the workspace's most recent other item, else nothing. Picking the source window,
/// having no current tab, or a picked window that closed before the pick does nothing. A window
/// in a dock changes nothing; the echo area says so.
pub(crate) fn move_buffer_to_pane(
    workspace: &mut Workspace,
    _: &MoveBufferToPane,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.focused_dock_position(window, cx).is_some() {
        bongos_minibuffer::message(window, cx, CENTER_ONLY);
        return;
    }
    let source = workspace.active_pane().clone();
    let Some(item_id) = source.read(cx).active_item().map(|item| item.item_id()) else {
        return;
    };
    let handle = cx.entity().downgrade();
    pick_pane(workspace, window, cx, move |destination, window, cx| {
        // The pick may arrive while the workspace is being updated.
        window.defer(cx, move |window, cx| {
            handle
                .update(cx, |workspace, cx| {
                    move_item(workspace, &source, &destination, item_id, window, cx)
                })
                .ok();
        });
    });
}

/// Moves `item_id` from the center pane `source` into `destination` and focuses it, keeping
/// `source` as described on [`move_buffer_to_pane`].
fn move_item(
    workspace: &mut Workspace,
    source: &Entity<Pane>,
    destination: &Entity<Pane>,
    item_id: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if source == destination
        || !workspace.center_panes().contains(source)
        || !targets(workspace, window, cx).contains(destination)
    {
        return;
    }
    let Some(item) = source
        .read(cx)
        .items()
        .find(|item| item.item_id() == item_id)
        .map(|item| item.boxed_clone())
    else {
        return;
    };
    // Chosen while the moving tab is still the source's current one, so it is not the pick.
    let replacement = recent_other_item(workspace, source, cx);
    if !workspace.move_item_keeping_pane(source, destination, item_id, true, window, cx) {
        return;
    }
    if destination.read(cx).index_for_item(item.as_ref()).is_none() {
        focus_same_entry(destination, item.as_ref(), window, cx);
    }
    keep_source(workspace, source, replacement, window, cx);
}

/// Activates and focuses `destination`'s tab for the file `item` shows, which took its place.
fn focus_same_entry(
    destination: &Entity<Pane>,
    item: &dyn ItemHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let entry = item.project_entry_ids(cx).first().copied();
    let index = entry.and_then(|entry| {
        destination
            .read(cx)
            .items()
            .position(|existing| existing.project_entry_ids(cx).first() == Some(&entry))
    });
    match index {
        Some(index) => destination.update(cx, |destination, cx| {
            destination.activate_item(index, true, true, window, cx)
        }),
        None => window.focus(&destination.focus_handle(cx), cx),
    }
}

/// Shows `source`'s most recently shown tab, or when it has none, a clone of `replacement`.
fn keep_source(
    workspace: &mut Workspace,
    source: &Entity<Pane>,
    replacement: Option<Box<dyn ItemHandle>>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let recent = {
        let pane = source.read(cx);
        pane.activation_history()
            .iter()
            .filter_map(|entry| {
                pane.items()
                    .position(|item| item.item_id() == entry.entity_id)
                    .map(|index| (entry.timestamp, index))
            })
            .max()
            .map(|(_, index)| index)
            .or_else(|| (pane.items_len() > 0).then_some(0))
    };
    if let Some(index) = recent {
        if source.read(cx).active_item_index() != index {
            source.update(cx, |source, cx| {
                source.activate_item(index, false, false, window, cx)
            });
        }
        return;
    }
    let Some(replacement) = replacement else {
        return;
    };
    let clone = clone_for_split(workspace, replacement.as_ref(), window, cx);
    let source = source.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        let clone = clone.await?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let source = source.upgrade()?;
                if !workspace.center_panes().contains(&source) || source.read(cx).items_len() > 0 {
                    return None;
                }
                source.update(cx, |source, cx| {
                    source.add_item(clone, false, false, None, window, cx)
                });
                Some(())
            })
            .ok()
            .flatten()
    })
    .detach();
}
