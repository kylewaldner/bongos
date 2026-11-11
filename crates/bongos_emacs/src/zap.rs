//! Zap commands: zap-to-char, zap-up-to-char and their backward forms. Without a character the
//! command asks for one with a read-key that any printable character answers, then dispatches
//! the same action with the character set; with one it kills through `apply::run`, so the text
//! reaches the kill sink and the clipboard before it goes. The ranges come from
//! `bongos_emacs_model`. When no caret finds the character the buffer, the clipboard and the
//! carets stay as they were and the echo area says so.

use bongos_emacs_model::{
    Caret, Plan, zap_back_to_char, zap_back_up_to_char, zap_to_char, zap_up_to_char,
};
use bongos_minibuffer::ReadKey;
use editor::Editor;
use gpui::{Action, App, Entity, Focusable as _, Window};
use workspace::{MinibufferMessageKind, MinibufferRouting};

use crate::actions::{ZapBackToChar, ZapBackUpToChar, ZapToChar, ZapUpToChar};
use crate::apply::{self, Command};
use crate::snapshot_text::SnapshotText;

/// The name the prompt is routed under (`minibuffer.enabled_for`).
const MINIBUFFER_NAME: &str = "zap";

/// The model's plan for one zap form.
type Compute = fn(&SnapshotText, &[Caret], Option<i32>, char) -> Plan;

/// What differs between the four forms.
struct Form<A> {
    prompt: &'static str,
    compute: Compute,
    /// The same action, with the character that answered.
    with_char: fn(Option<i32>, char) -> A,
}

fn run<A: Action>(
    action: &A,
    count: Option<i32>,
    char: Option<char>,
    form: Form<A>,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    match char {
        Some(char) => kill(action.name(), count, char, form.compute, editor, window, cx),
        None => ask(count, form, editor, window, cx),
    }
}

/// Opens the prompt; its answer runs the zap again with the character in the action.
fn ask<A: Action>(
    count: Option<i32>,
    form: Form<A>,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let weak = editor.downgrade();
    let with_char = form.with_char;
    let prompt = ReadKey::new(MINIBUFFER_NAME, form.prompt)
        .any_char()
        .on_answer(move |char, window, cx| {
            let Some(editor) = weak.upgrade() else {
                return;
            };
            // The prompt has closed; the zap goes to the editor that asked, wherever focus is.
            window.focus(&editor.focus_handle(cx), cx);
            window.dispatch_action(with_char(count, char).boxed_clone(), cx);
        });
    workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::read_key(workspace, prompt, window, cx);
    });
}

/// Kills for every caret at once. Nothing is killed (and the echo area says so) when no caret
/// finds the character; a count of 0 and a read-only buffer do nothing at all.
fn kill(
    name: &'static str,
    count: Option<i32>,
    char: char,
    compute: Compute,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if count == Some(0) || editor.read(cx).read_only(cx) {
        return;
    }
    let killed = apply::run(editor, window, cx, Command::edit(name), |text, carets| {
        compute(text, carets, count, char)
    });
    if !killed {
        MinibufferRouting::message(
            window,
            cx,
            &format!("Search failed: \"{char}\""),
            MinibufferMessageKind::Message,
        );
    }
}

pub(crate) fn zap_to_char_command(
    action: &ZapToChar,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = Form {
        prompt: "Zap to char: ",
        compute: |text, carets, count, char| zap_to_char(text, carets, count, char),
        with_char: |count, char| ZapToChar {
            count,
            char: Some(char),
        },
    };
    run(action, action.count, action.char, form, editor, window, cx);
}

pub(crate) fn zap_up_to_char_command(
    action: &ZapUpToChar,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = Form {
        prompt: "Zap up to char: ",
        compute: |text, carets, count, char| zap_up_to_char(text, carets, count, char),
        with_char: |count, char| ZapUpToChar {
            count,
            char: Some(char),
        },
    };
    run(action, action.count, action.char, form, editor, window, cx);
}

pub(crate) fn zap_back_to_char_command(
    action: &ZapBackToChar,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = Form {
        prompt: "Zap back to char: ",
        compute: |text, carets, count, char| zap_back_to_char(text, carets, count, char),
        with_char: |count, char| ZapBackToChar {
            count,
            char: Some(char),
        },
    };
    run(action, action.count, action.char, form, editor, window, cx);
}

pub(crate) fn zap_back_up_to_char_command(
    action: &ZapBackUpToChar,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = Form {
        prompt: "Zap back up to char: ",
        compute: |text, carets, count, char| zap_back_up_to_char(text, carets, count, char),
        with_char: |count, char| ZapBackUpToChar {
            count,
            char: Some(char),
        },
    };
    run(action, action.count, action.char, form, editor, window, cx);
}
