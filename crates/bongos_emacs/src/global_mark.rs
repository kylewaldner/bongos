//! pop-global-mark: back to the newest earlier location in another file, read from the active
//! pane's navigation history. Going back puts the location left on the forward stack, so
//! go-forward returns to it.

use gpui::{App, Context, TaskExt as _, Window};
use workspace::Workspace;

use crate::actions::PopGlobalMark;
use crate::last_command;

const NO_EARLIER_LOCATION: &str = "No earlier location in another file";

fn pop_global_mark(
    workspace: &mut Workspace,
    _: &PopGlobalMark,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    last_command::record(window, Some("emacs::PopGlobalMark"), cx);
    let pane = workspace.active_pane().clone();
    let (active_id, active_path) = pane
        .read(cx)
        .active_item()
        .map(|item| (Some(item.item_id()), item.project_path(cx)))
        .unwrap_or((None, None));
    let found = pane.read(cx).nav_history().last_backward_where(
        &mut |entry, path| {
            Some(entry.item.id()) != active_id
                && (active_path.is_none() || path != active_path.as_ref())
        },
        cx,
    );
    if !found {
        bongos_minibuffer::message(window, cx, NO_EARLIER_LOCATION);
        return;
    }
    workspace
        .go_back_to_other_item(pane.downgrade(), window, cx)
        .detach_and_log_err(cx);
}

/// Registers the `Workspace` handler on every workspace.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(pop_global_mark);
    })
    .detach();
}
