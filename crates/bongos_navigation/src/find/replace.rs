//! Replacing from find in files.
use gpui::{Context, Window};
use workspace::Workspace;

/// Shows or hides the replacement field.
pub fn toggle(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
/// Replaces every match.
pub fn replace_all(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
