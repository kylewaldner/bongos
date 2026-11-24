//! The toggles and the scope of find in files: regex, case, whole word, the open buffers and a
//! directory. Each acts on the open find picker and does nothing when another picker is open.

use std::rc::Rc;

use bongos_minibuffer::{Candidate, Outcome, Session, Source};
use bongos_navigation_model::flags::Flags;
use gpui::{App, Context, Window};
use workspace::Workspace;

use crate::find::picker::{self, FindPicker, Start};
use crate::registry::{self, SessionHandle};

/// The name the directory prompt is routed by.
pub const DIRECTORY_HOST: &str = "find_directory";

/// Switches regex matching.
pub fn regex(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    change(workspace, window, cx, |flags| flags.regex = !flags.regex)
}

/// Cycles the case mode: smart, sensitive, insensitive.
pub fn case_sensitive(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    change(workspace, window, cx, Flags::cycle_case)
}

/// Switches whole word matching.
pub fn whole_word(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    change(workspace, window, cx, |flags| {
        flags.whole_word = !flags.whole_word
    })
}

/// Limits the search to the buffers of the open items, or lifts the limit.
pub fn scope_open_buffers(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    change(workspace, window, cx, |flags| {
        flags.open_buffers = !flags.open_buffers
    })
}

/// Closes the picker, asks for a directory and opens the picker again with the directory after
/// ` -- ` in the input.
pub fn scope_directory(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    registry::in_session(window, cx, |session, window, cx| {
        let Some(picker) = find_picker(&session) else {
            return;
        };
        let input = picker.input();
        let flags = picker.flags();
        let base_dir = picker.base_dir().map(str::to_string);
        let replace = picker.replace_mode();
        if let Some(handle) = picker.handle() {
            handle.close(window, cx);
        }
        let this = cx.entity();
        let session = Session::completing_read(DIRECTORY_HOST, "Directory: ")
            .no_history()
            .source(Source::Fixed(directories(workspace, cx)))
            .on_confirm(move |outcome, window, cx| {
                let Outcome::Candidate(value) = outcome else {
                    return;
                };
                let Some(dir) = value.downcast_ref::<String>() else {
                    return;
                };
                let start = Start {
                    input: scoped_input(&input, dir),
                    select_input: false,
                    flags,
                    base_dir,
                    replace,
                };
                this.update(cx, |workspace, cx| {
                    picker::open_with(workspace, start, window, cx);
                });
            });
        bongos_minibuffer::open(workspace, session, window, cx);
    })
}

/// `input` with `dir` after ` -- `, added to the scope it already has.
fn scoped_input(input: &str, dir: &str) -> String {
    if input.contains(" -- ") {
        format!("{input} {dir}")
    } else {
        format!("{input} -- {dir}")
    }
}

/// The directories of the visible worktrees, as the scope names them.
fn directories(workspace: &Workspace, cx: &App) -> Vec<Candidate> {
    let several = workspace.visible_worktrees(cx).nth(1).is_some();
    let mut dirs: Vec<String> = Vec::new();
    for worktree in workspace.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.root_name_str().to_string();
        for entry in worktree.snapshot().entries(false, 0) {
            let path = entry.path.as_unix_str();
            if !entry.is_dir() || path.is_empty() {
                continue;
            }
            dirs.push(if several {
                format!("{root}/{path}")
            } else {
                path.to_string()
            });
        }
    }
    dirs.sort();
    dirs.into_iter()
        .map(|dir| Candidate::new(dir.clone(), dir))
        .collect()
}

fn find_picker(session: &Rc<dyn SessionHandle>) -> Option<&FindPicker> {
    session.as_any().downcast_ref::<FindPicker>()
}

/// Applies `edit` to the flags of the open find picker, shows them and searches again.
fn change(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    edit: impl FnOnce(&mut Flags),
) {
    registry::in_session(window, cx, |session, window, cx| {
        let Some(picker) = find_picker(&session) else {
            return;
        };
        let mut flags = picker.flags();
        edit(&mut flags);
        let buffers = if flags.open_buffers {
            picker::open_buffers(workspace, cx)
        } else {
            Vec::new()
        };
        picker.set_flags(flags, buffers, window, cx);
    })
}

#[cfg(test)]
#[path = "toggles_tests.rs"]
mod tests;
