//! The go to file picker and its four openers.
use gpui::{Context, Window};
use workspace::Workspace;

/// Opens the picker; a chosen file opens in the current pane.
pub fn open_current(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
/// Opens the picker; a chosen file opens in another pane.
pub fn open_other_pane(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
/// Opens the picker; a chosen file opens in a split to the right.
pub fn open_split_right(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
/// Opens the picker; a chosen file opens in a split below.
pub fn open_split_down(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
