//! The regex, case and whole word toggles of find in files.
use gpui::{Context, Window};
use workspace::Workspace;

/// Switches regex matching.
pub fn regex(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
/// Switches case sensitivity.
pub fn case_sensitive(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
/// Switches whole word matching.
pub fn whole_word(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
