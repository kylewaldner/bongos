//! `string-rectangle`: asks for a string in the minibuffer, then replaces the text of every row
//! of the rectangle with it. The rectangle is the one between the mark and the point when the
//! prompt opened. The last string is offered as the default, as Emacs does.

use bongos_emacs_model::rectangle_ops::RectOp;
use bongos_minibuffer::{Outcome, Session};
use editor::Editor;
use gpui::{App, Entity, Focusable as _, Global, Window};

use crate::actions::StringRectangle;
use crate::rectangle::{apply_to, corners};
use crate::rectangle_mode;

/// The name the prompt is routed under (`minibuffer.enabled_for`) and keeps its history under.
const MINIBUFFER_NAME: &str = "string_rectangle";

/// The string the command was last confirmed with.
#[derive(Default)]
struct LastString(String);

impl Global for LastString {}

pub(crate) fn string_rectangle_command(
    _: &StringRectangle,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    if editor.read(cx).read_only(cx) {
        bongos_minibuffer::message(window, cx, "Buffer is read-only");
        return;
    }
    let Some(corners) = corners(editor, window, cx) else {
        return;
    };
    rectangle_mode::finish(editor, window, cx);
    let last = cx.default_global::<LastString>().0.clone();
    let prompt = if last.is_empty() {
        "String rectangle: ".to_string()
    } else {
        format!("String rectangle (default {last}): ")
    };
    let mut session = Session::string(MINIBUFFER_NAME, prompt).history(MINIBUFFER_NAME);
    if !last.is_empty() {
        session = session.default(last);
    }
    let on_confirm = editor.downgrade();
    let session = session.on_confirm(move |outcome, window, cx| {
        let Some(editor) = on_confirm.upgrade() else {
            return;
        };
        window.focus(&editor.focus_handle(cx), cx);
        let text = match outcome {
            Outcome::Input(text) => text,
            Outcome::Candidate(_) => return,
        };
        cx.set_global(LastString(text.clone()));
        apply_to(&editor, RectOp::String(text), corners, window, cx);
    });
    workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx);
    });
}
