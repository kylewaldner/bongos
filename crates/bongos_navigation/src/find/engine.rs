//! The search behind find in files, and its scope.
use gpui::{Context, Window};
use workspace::Workspace;

/// Limits the search to the open buffers.
pub fn scope_open_buffers(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
/// Limits the search to a directory.
pub fn scope_directory(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
