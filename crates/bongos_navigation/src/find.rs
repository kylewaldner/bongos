//! The find in files picker.

pub mod engine;
pub mod export;
pub mod picker;
pub mod replace;
pub mod toggles;

use gpui::{Context, Window};
use workspace::Workspace;

/// Opens the find in files picker; a prefix argument keeps the search in the active file's
/// directory.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let scoped = bongos_emacs::prefix_arg::take(window, cx).is_some();
    picker::open(workspace, scoped, window, cx);
}
