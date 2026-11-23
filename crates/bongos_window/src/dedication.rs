//! Window dedication: a dedicated window (pane) keeps showing what it shows. Opens that would
//! land in it go to another window instead (`Workspace::pane_for_open`). The mark lives in memory
//! only and goes away with its pane.

use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::ToggleWindowDedication;

/// Dedicates the focused window, docks included, or ends its dedication, and says which in the
/// echo area.
pub(crate) fn toggle_dedication(
    workspace: &mut Workspace,
    _: &ToggleWindowDedication,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = workspace.focused_pane(window, cx);
    let dedicated = !pane.read(cx).is_dedicated();
    pane.update(cx, |pane, cx| pane.set_dedicated(dedicated, cx));
    let message = if dedicated {
        "Window dedicated"
    } else {
        "Window no longer dedicated"
    };
    bongos_minibuffer::message(window, cx, message);
}
