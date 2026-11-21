//! The zoom toggle: zooms the focused window (pane) to fill the frame, or restores the layout.
//! Nothing is closed, moved or resized; the other windows are only hidden while the zoom lasts.

use gpui::{Context, Window};
use workspace::{Workspace, ZoomIn};

use crate::actions::ToggleDeleteOtherWindows;
use crate::targets::targets;

const CANNOT_ZOOM: &str = "This pane cannot be zoomed";

/// With a zoom on, un-zooms whatever is zoomed, even when another pane has focus. Otherwise, with
/// fewer than two target windows, does nothing; else zooms the focused window through the pane's
/// own zoom, so a dock panel reports it to the workspace as it does for any other zoom. A window
/// that cannot be zoomed says so in the echo area.
pub(crate) fn toggle_zoom(
    workspace: &mut Workspace,
    _: &ToggleDeleteOtherWindows,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.zoomed_item().is_some() {
        workspace.unzoom(window, cx);
        return;
    }
    if targets(workspace, window, cx).len() < 2 {
        return;
    }
    let pane = workspace.focused_pane(window, cx);
    if !pane.read(cx).can_toggle_zoom() || pane.read(cx).items_len() == 0 {
        bongos_minibuffer::message(window, cx, CANNOT_ZOOM);
        return;
    }
    pane.update(cx, |pane, cx| pane.zoom_in(&ZoomIn, window, cx));
    // The pane's zoom reaches the workspace through events, which run once this update ends, so
    // whether anything zoomed is known only then.
    cx.spawn_in(window, async move |workspace, cx| {
        workspace
            .update_in(cx, |workspace, window, cx| {
                if workspace.zoomed_item().is_none() {
                    bongos_minibuffer::message(window, cx, CANNOT_ZOOM);
                }
            })
            .ok();
    })
    .detach();
}
