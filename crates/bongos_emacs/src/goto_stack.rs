//! The goto stack: back and forward walk the active pane's navigation history, and push records
//! the caret's place in it without moving. Going back puts the place left on the forward stack,
//! so going forward returns to it; a push clears the forward stack.

use editor::Editor;
use gpui::{Action as _, App, Context, Entity, Window};
use workspace::Workspace;

use crate::actions::{GotoStackBack, GotoStackForward, GotoStackPush};
use crate::last_command;
use crate::prefix_arg;

pub(crate) const NO_EARLIER_LOCATION: &str = "No earlier location";
pub(crate) const NO_LATER_LOCATION: &str = "No later location";

fn goto_stack_back(
    workspace: &mut Workspace,
    _: &GotoStackBack,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    last_command::record(window, Some(GotoStackBack::name_for_type()), cx);
    let pane = workspace.active_pane().clone();
    if !pane.read(cx).can_navigate_backward() {
        bongos_minibuffer::message(window, cx, NO_EARLIER_LOCATION);
        return;
    }
    let walk = workspace.go_back_reporting(pane.downgrade(), window, cx);
    cx.spawn_in(window, async move |_, cx| {
        match walk.await {
            Ok(false) => {
                cx.update(|window, cx| bongos_minibuffer::message(window, cx, NO_EARLIER_LOCATION))
                    .ok();
            }
            Ok(true) => {}
            Err(error) => log::error!("goto stack walk failed: {error:#}"),
        }
    })
    .detach();
}

fn goto_stack_forward(
    workspace: &mut Workspace,
    _: &GotoStackForward,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    last_command::record(window, Some(GotoStackForward::name_for_type()), cx);
    let pane = workspace.active_pane().clone();
    if !pane.read(cx).can_navigate_forward() {
        bongos_minibuffer::message(window, cx, NO_LATER_LOCATION);
        return;
    }
    let walk = workspace.go_forward_reporting(pane.downgrade(), window, cx);
    cx.spawn_in(window, async move |_, cx| {
        match walk.await {
            Ok(false) => {
                cx.update(|window, cx| bongos_minibuffer::message(window, cx, NO_LATER_LOCATION))
                    .ok();
            }
            Ok(true) => {}
            Err(error) => log::error!("goto stack walk failed: {error:#}"),
        }
    })
    .detach();
}

/// Records the caret's place as the newest entry of the editor's pane history. The caret does
/// not move; a push at a place already newest stays one entry.
pub(crate) fn goto_stack_push_command(
    _: &GotoStackPush,
    editor: &Entity<Editor>,
    _: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| editor.create_nav_history_entry(cx));
}

/// Declares the commands' prefix argument as ignored and registers back and forward on every
/// workspace. Push is an editor command, registered with the others in `crate::init`.
pub(crate) fn init(cx: &mut App) {
    for name in [
        GotoStackBack::name_for_type(),
        GotoStackForward::name_for_type(),
        GotoStackPush::name_for_type(),
    ] {
        prefix_arg::declare(cx, name, prefix_arg::Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(goto_stack_back);
        workspace.register_action(goto_stack_forward);
    })
    .detach();
}
