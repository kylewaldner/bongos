//! Emacs-style window management: the `window` actions and the list of windows (panes) they
//! work on, in reading order. An Emacs window is a pane here; a frame is the app window.

pub mod actions;
mod layout;
mod orientation;
pub mod picker;
mod split;
pub mod targets;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod zoom;

#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod orientation_tests;
#[cfg(test)]
mod picker_tests;
#[cfg(test)]
mod split_tests;
#[cfg(test)]
mod targets_tests;
#[cfg(test)]
mod zoom_tests;

use bongos_emacs::prefix_arg::{Meaning, declare};
use gpui::{App, Context, Window};
use workspace::Workspace;

/// Declares what each `window` action does with a prefix argument (the two splits read it, the
/// rest ignore it), and registers the handlers of every workspace.
pub fn init(cx: &mut App) {
    for name in actions::READS {
        declare(cx, name, Meaning::Reads);
    }
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(switch_window);
        workspace.register_action(zoom::toggle_zoom);
        workspace.register_action(split::split_window_below);
        workspace.register_action(split::split_window_right);
        workspace.register_action(split::delete_window);
        workspace.register_action(orientation::split_window_horizontally_instead);
        workspace.register_action(orientation::split_window_vertically_instead);
        workspace.register_action(layout::balance_panes);
        workspace.register_action(layout::swap_panes);
        workspace.register_action(layout::move_buffer_to_pane);
    })
    .detach();
}

/// Picks another window by a letter and focuses it. A prefix argument changes nothing.
fn switch_window(
    workspace: &mut Workspace,
    _: &actions::SwitchWindow,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    picker::pick_pane(workspace, window, cx, picker::focus_pane);
}
