//! Killing the highlighted buffer from the switcher.
use gpui::{Context, Window};
use workspace::Workspace;

/// Kills the highlighted buffer.
pub fn kill(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
