//! The C-g hooks: later features register what a quit must also cancel (a pending prefix,
//! jump labels). `keyboard-quit` runs them once per keystroke.

use std::rc::Rc;

use editor::Editor;
use gpui::{App, Entity, Global, Window};
use text::SelectionGoal;
use workspace::Workspace;

use crate::actions::KeyboardQuit;
use crate::last_command;

type Hook = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
struct QuitHooks(Vec<Hook>);

impl Global for QuitHooks {}

/// Set while one keystroke's quit handlers run, so the hooks run once even when several
/// bindings of the key reach a handler.
#[derive(Default)]
struct QuitState {
    hooks_ran: bool,
}

impl Global for QuitState {}

/// Adds a hook that every C-g runs before cancelling anything else.
pub fn register(cx: &mut App, hook: impl Fn(&mut Window, &mut App) + 'static) {
    cx.default_global::<QuitHooks>().0.push(Rc::new(hook));
}

/// Runs every registered hook unless they already ran for this keystroke (or this dispatch).
/// The guard clears once the current update finishes, before the next keystroke.
pub fn run_hooks_once(window: &mut Window, cx: &mut App) {
    if cx.default_global::<QuitState>().hooks_ran {
        return;
    }
    cx.default_global::<QuitState>().hooks_ran = true;
    cx.defer(|cx| cx.default_global::<QuitState>().hooks_ran = false);
    let hooks = cx
        .try_global::<QuitHooks>()
        .map(|hooks| hooks.0.clone())
        .unwrap_or_default();
    for hook in hooks {
        hook(window, cx);
    }
}

/// C-g in an editor. Never edits text. Runs the hooks, then does the first thing that applies
/// (as `Editor::cancel` does, one per press). Ending mark mode always happens and the press goes
/// on: it dismisses menus and popups, collapses expanded diff hunks, hides the blame gutter,
/// clears the region on every caret (full editors only), or collapses several carets to the
/// oldest. The key is consumed when mark mode was on or one of those applied; otherwise it
/// propagates, so a picker's query editor reaches `menu::Cancel`.
pub(crate) fn keyboard_quit_command(
    _: &KeyboardQuit,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    run_hooks_once(window, cx);
    let cancelled = editor.update(cx, |editor, cx| {
        let mark_was_on = editor.selection_mark_mode();
        editor.set_selection_mark_mode(false, cx);
        if editor.dismiss_menus_and_popups(true, window, cx)
            || editor.collapse_expanded_diff_hunks(cx)
            || editor.hide_git_blame_gutter(cx)
        {
            cx.notify();
            return true;
        }
        if editor.mode().is_full() {
            let has_region = editor
                .selections
                .disjoint_anchors()
                .iter()
                .any(|selection| selection.start != selection.end);
            let changed = if has_region {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.move_with(&mut |_, selection| {
                        if selection.start != selection.end {
                            selection.collapse_to(selection.head(), SelectionGoal::None);
                        }
                    });
                });
                true
            } else {
                editor.change_selections(Default::default(), window, cx, |s| s.try_cancel())
            };
            if changed {
                cx.notify();
                return true;
            }
        }
        mark_was_on
    });
    if !cancelled {
        cx.propagate();
    }
}

/// C-g at `Workspace` depth (focus in a panel): hides the active modal unless focus is inside
/// it, and propagates otherwise, so the modal's own `menu::Cancel` and any other panel's key
/// handling still run.
fn workspace_quit(
    workspace: &mut Workspace,
    _: &KeyboardQuit,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    run_hooks_once(window, cx);
    last_command::record(window, Some("emacs::KeyboardQuit"), cx);
    if workspace.has_active_modal(window, cx) && !workspace.focus_in_modal(window, cx) {
        workspace.hide_modal(window, cx);
    } else {
        cx.propagate();
    }
}

/// Registers the `Workspace` handler on every workspace.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(workspace_quit);
    })
    .detach();
}
