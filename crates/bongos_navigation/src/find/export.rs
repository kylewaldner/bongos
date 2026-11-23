//! Exporting find in files results to a multibuffer.
use gpui::{Context, Window};
use workspace::Workspace;

/// Opens the results in a multibuffer.
pub fn to_multibuffer(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
