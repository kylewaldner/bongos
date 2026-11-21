//! Splitting the focused window (pane) and deleting it. Neither closes a tab or a buffer: a split
//! shows a clone of a recent item in the new window, and a deleted window hands its tabs to its
//! neighbor.

use bongos_emacs::prefix_arg;
use gpui::{Context, Entity, Focusable as _, Task, Window};
use util::ResultExt as _;
use workspace::{Pane, SplitDirection, Workspace};

use crate::actions::{DeleteWindow, SplitWindowBelow, SplitWindowRight};
use crate::targets::{clone_for_split, recent_other_item};

const CENTER_ONLY: &str = "Pane commands act on the center panes";
const SOLE_PANE: &str = "Sole pane";

/// Splits the focused center window, putting the new one below it.
pub(crate) fn split_window_below(
    workspace: &mut Workspace,
    _: &SplitWindowBelow,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    split_focused(workspace, SplitDirection::Down, window, cx);
}

/// Splits the focused center window, putting the new one to its right.
pub(crate) fn split_window_right(
    workspace: &mut Workspace,
    _: &SplitWindowRight,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    split_focused(workspace, SplitDirection::Right, window, cx);
}

/// Takes the prefix argument (with one, focus stays in the focused window), un-zooms, and splits
/// the focused center window. A window in a dock is not split; the echo area says so.
fn split_focused(
    workspace: &mut Workspace,
    direction: SplitDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let keep_focus = prefix_arg::take(window, cx).is_some();
    if workspace.focused_dock_position(window, cx).is_some() {
        bongos_minibuffer::message(window, cx, CENTER_ONLY);
        return;
    }
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
    }
    let pane = workspace.active_pane().clone();
    split_pane(workspace, pane, direction, keep_focus, window, cx).detach();
}

/// Splits the center pane `pane` in `direction`. The new pane shows a clone of
/// [`recent_other_item`]; the clone resolves before the pane is added, so no empty pane shows
/// meanwhile. When there is no item to clone the new pane is empty; when the clone yields
/// nothing, or `pane` left the center before it resolved, no pane is added. The new pane is
/// focused unless `keep_focus`, which leaves focus in `pane`. Resolves to the new pane.
pub(crate) fn split_pane(
    workspace: &mut Workspace,
    pane: Entity<Pane>,
    direction: SplitDirection,
    keep_focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Option<Entity<Pane>>> {
    let Some(item) = recent_other_item(workspace, &pane, cx) else {
        let new_pane = workspace.split_pane(pane.clone(), direction, window, cx);
        if keep_focus {
            window.focus(&pane.focus_handle(cx), cx);
        }
        return Task::ready(Some(new_pane));
    };
    let clone = clone_for_split(workspace, item.as_ref(), window, cx);
    let source = pane.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        let clone = clone.await?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let pane = source.upgrade()?;
                if !workspace.center_panes().contains(&pane) {
                    return None;
                }
                let nav_history = pane.read(cx).fork_nav_history();
                let new_pane = workspace.split_pane(pane.clone(), direction, window, cx);
                new_pane.update(cx, |new_pane, cx| {
                    new_pane.set_nav_history(nav_history, cx);
                    new_pane.add_item(clone, !keep_focus, !keep_focus, None, window, cx);
                });
                if keep_focus {
                    window.focus(&pane.focus_handle(cx), cx);
                }
                Some(new_pane)
            })
            .ok()
            .flatten()
    })
}

/// Un-zooms, then closes the focused window: a dock closes; a center window hands its tabs to
/// its neighbor, which gets focus, and its dedication ends with it. The only center window stays,
/// and the echo area says so.
pub(crate) fn delete_window(
    workspace: &mut Workspace,
    _: &DeleteWindow,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
    }
    if let Some(position) = workspace.focused_dock_position(window, cx) {
        workspace.toggle_dock(position, window, cx);
        return;
    }
    let current = workspace.active_pane().clone();
    let Some(sibling) = workspace
        .center_sibling(&current)
        .filter(|sibling| *sibling != current)
    else {
        bongos_minibuffer::message(window, cx, SOLE_PANE);
        return;
    };
    let closed = workspace
        .close_pane_keeping_tabs(&current, &sibling, window, cx)
        .log_err();
    if closed.is_some() {
        current.update(cx, |pane, cx| pane.set_dedicated(false, cx));
        window.focus(&sibling.focus_handle(cx), cx);
    }
}
