//! Emacs-style window management: the `window` actions and the list of windows (panes) they
//! work on, in reading order. An Emacs window is a pane here; a frame is the app window.

pub mod actions;
pub mod targets;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod targets_tests;

use bongos_emacs::prefix_arg::{Meaning, declare};
use gpui::App;

/// Declares what each `window` action does with a prefix argument: the two splits read it, the
/// rest ignore it.
pub fn init(cx: &mut App) {
    for name in actions::READS {
        declare(cx, name, Meaning::Reads);
    }
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
}
