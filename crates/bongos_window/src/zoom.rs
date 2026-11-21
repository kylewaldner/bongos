//! The zoom toggle: zooms the focused window (pane) to fill the frame, or restores the layout.
//! Nothing is closed, moved or resized; the other windows are only hidden while the zoom lasts.

use gpui::{Context, Entity, Window};
use workspace::{Pane, Workspace, ZoomIn};

use crate::actions::ToggleDeleteOtherWindows;
use crate::targets::targets;

const CANNOT_ZOOM: &str = "This pane cannot be zoomed";

/// With a zoom on, un-zooms whatever is zoomed, even when another pane has focus. Otherwise, with
/// fewer than two target windows, does nothing; else zooms the focused window: a center pane
/// directly, a dock pane through its panel when the panel can zoom. A window that cannot be zoomed
/// says so in the echo area.
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
    if workspace.center_panes().contains(&pane) {
        zoom_center_pane(&pane, window, cx);
        return;
    }
    let panel = workspace.all_docks().into_iter().find_map(|dock| {
        let panel = dock.read(cx).visible_panel()?;
        panel
            .panes(cx)
            .contains(&pane)
            .then(|| (dock.clone(), panel.clone()))
    });
    let Some((dock, panel)) = panel else {
        bongos_minibuffer::message(window, cx, CANNOT_ZOOM);
        return;
    };
    // Deferred: zooming a panel updates the workspace, which is being updated now.
    window.defer(cx, move |window, cx| {
        dock.update(cx, |dock, cx| {
            dock.set_panel_zoomed(&panel.to_any(), true, window, cx)
        });
        // A panel that does not zoom leaves itself as it was.
        if !panel.is_zoomed(window, cx) {
            bongos_minibuffer::message(window, cx, CANNOT_ZOOM);
        }
    });
}

fn zoom_center_pane(pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Workspace>) {
    if pane.read(cx).items_len() == 0 {
        bongos_minibuffer::message(window, cx, CANNOT_ZOOM);
        return;
    }
    pane.update(cx, |pane, cx| pane.zoom_in(&ZoomIn, window, cx));
}
