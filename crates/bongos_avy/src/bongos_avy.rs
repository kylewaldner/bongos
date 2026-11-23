//! Emacs avy: jump to a visible character, word or line by typing a short label. This crate holds
//! the `avy` actions and the collection of jump candidates from every visible editor; the labels,
//! matching and ranking are in `bongos_avy_model`.

pub mod actions;
pub mod jump;
pub mod overlay;
pub mod session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod visible;

#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod jump_tests;
#[cfg(test)]
mod overlay_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod visible_tests;

use bongos_emacs::prefix_arg::{Meaning, declare};
use editor::Editor;
use gpui::{App, Context, Entity, Focusable as _, Window};
use workspace::Workspace;

use crate::actions::{GotoCharTimer, GotoLine, GotoWord1};
use crate::session::Outcome;
use crate::visible::Mode;

/// Declares that the `avy` actions ignore a prefix argument, lets C-g cancel a running jump, and
/// registers the actions' handlers in every workspace.
pub fn init(cx: &mut App) {
    session::init(cx);
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &GotoCharTimer, window, cx| {
            run(Mode::CharTimer, workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &GotoWord1, window, cx| {
            run(Mode::Word, workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &GotoLine, window, cx| {
            run(Mode::Line, workspace, window, cx);
        });
    })
    .detach();
}

/// The full editor in a pane of `workspace` that holds the focus, if any.
fn focused_editor(workspace: &Workspace, window: &Window, cx: &App) -> Option<Entity<Editor>> {
    workspace.panes().iter().find_map(|pane| {
        let editor = pane.read(cx).active_item()?.downcast::<Editor>()?;
        let focused = editor.focus_handle(cx).is_focused(window);
        (focused && editor.read(cx).mode().is_full()).then_some(editor)
    })
}

/// Starts a jump of `mode` from the focused editor and acts on its outcome. Does nothing when the
/// focus is not in an editor of a pane.
fn run(mode: Mode, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(origin) = focused_editor(workspace, window, cx) else {
        return;
    };
    let from = origin.clone();
    session::start(
        mode,
        origin,
        workspace,
        window,
        cx,
        move |outcome, window, cx| match outcome {
            Outcome::Jump(candidate) => jump::jump(from, candidate, window, cx),
            Outcome::NoMatches(query) if query.is_empty() => {
                bongos_minibuffer::message(window, cx, "No matches");
            }
            Outcome::NoMatches(query) => {
                bongos_minibuffer::message(window, cx, format!("No matches for '{query}'"));
            }
        },
    );
}
