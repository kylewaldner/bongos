//! Where a chosen row opens: the current pane or another one.
use gpui::{Context, Window};
use workspace::Workspace;

/// Opens the highlighted row in a chosen pane.
pub fn open_in_pane(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
