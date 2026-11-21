//! Laying two windows (panes) out side by side or stacked. With one window this splits it; with
//! more than two, the focused window and the next one stay and every other window hands its tabs
//! to that next one. No tab or buffer is closed.

use gpui::{Axis, Context, Entity, Focusable as _, Window};
use util::ResultExt as _;
use workspace::{Pane, SplitDirection, Workspace};

use crate::actions::{SplitWindowHorizontallyInstead, SplitWindowVerticallyInstead};
use crate::split::split_pane;

const CENTER_ONLY: &str = "Pane commands act on the center panes";

/// Puts two center windows side by side.
pub(crate) fn split_window_horizontally_instead(
    workspace: &mut Workspace,
    _: &SplitWindowHorizontallyInstead,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    arrange(workspace, Axis::Horizontal, window, cx);
}

/// Stacks two center windows.
pub(crate) fn split_window_vertically_instead(
    workspace: &mut Workspace,
    _: &SplitWindowVerticallyInstead,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    arrange(workspace, Axis::Vertical, window, cx);
}

/// Un-zooms, then lays the center out as two windows along `axis`. One window is split as
/// `split-window-right` or `split-window-below` would. Of more than two, the focused window and
/// the next one in tree order (wrapping) stay, the others hand their tabs to that next window one
/// by one, and focus stays in the focused window. A window in a dock changes nothing; the echo
/// area says so.
fn arrange(
    workspace: &mut Workspace,
    axis: Axis,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.focused_dock_position(window, cx).is_some() {
        bongos_minibuffer::message(window, cx, CENTER_ONLY);
        return;
    }
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
    }
    let current = workspace.active_pane().clone();
    let panes = workspace.center_panes();
    let Some(position) = panes.iter().position(|pane| *pane == current) else {
        return;
    };
    if panes.len() == 1 {
        let direction = match axis {
            Axis::Horizontal => SplitDirection::Right,
            Axis::Vertical => SplitDirection::Down,
        };
        split_pane(workspace, current, direction, false, window, cx).detach();
        return;
    }
    let next = panes[(position + 1) % panes.len()].clone();
    for other in panes {
        if other == current || other == next {
            continue;
        }
        // A pane that left the center meanwhile is skipped.
        if !workspace.center_panes().contains(&other) {
            continue;
        }
        let closed = workspace
            .close_pane_keeping_tabs(&other, &next, window, cx)
            .log_err();
        if closed.is_some() {
            other.update(cx, |pane, cx| pane.set_dedicated(false, cx));
        }
    }
    if let Some((first, second)) = pair_in_tree_order(workspace, &current, &next) {
        workspace
            .arrange_center_pair(&first, &second, axis, cx)
            .log_err();
    }
    window.focus(&current.focus_handle(cx), cx);
}

/// `a` and `b` in the center's tree order, read again after the merges; `None` when the center
/// is not exactly these two panes.
fn pair_in_tree_order(
    workspace: &Workspace,
    a: &Entity<Pane>,
    b: &Entity<Pane>,
) -> Option<(Entity<Pane>, Entity<Pane>)> {
    match workspace.center_panes().as_slice() {
        [first, second] if (first == a && second == b) || (first == b && second == a) => {
            Some((first.clone(), second.clone()))
        }
        _ => None,
    }
}
