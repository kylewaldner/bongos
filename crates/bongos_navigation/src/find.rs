//! The find in files picker.

pub mod engine;
pub mod export;
pub mod picker;
pub mod replace;
pub mod toggles;

use gpui::{Context, Window};
use workspace::Workspace;

/// Opens the find in files picker.
pub fn open(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
