//! The Emacs commands on top of the editor: the `emacs` actions, the one path that applies a
//! command's plan, the kill sink, the last-command tracker and the C-g hooks. The text logic
//! lives in `bongos_emacs_model`.

pub mod actions;
pub mod apply;
mod kill;
pub mod kill_sink;
pub mod last_command;
mod motion;
pub mod quit;
pub mod region;
pub mod snapshot_text;
mod words;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod kill_sink_tests;
#[cfg(test)]
mod kill_tests;
#[cfg(test)]
mod last_command_tests;
#[cfg(test)]
mod motion_tests;
#[cfg(test)]
mod words_tests;

use editor::Editor;
use gpui::{Action, App, Entity, Window};

pub use kill_sink::{Kill, KillDirection, KillKind, set_kill_sink};
pub use last_command::last_command;

/// Registers the implemented `emacs` commands on every editor and starts the last-command
/// tracker. Actions without a handler stay undispatchable, so only working commands reach M-x.
pub fn init(cx: &mut App) {
    last_command::init(cx);
    cx.observe_new(|editor: &mut Editor, _window, cx| {
        let weak = cx.entity().downgrade();
        register(editor, &weak, motion::forward_word);
        register(editor, &weak, motion::backward_word);
        register(editor, &weak, words::kill_word_command);
        register(editor, &weak, words::backward_kill_word_command);
        register(editor, &weak, words::upcase_word_command);
        register(editor, &weak, words::downcase_word_command);
        register(editor, &weak, words::capitalize_word_command);
        register(editor, &weak, words::upcase_previous_word_command);
        register(editor, &weak, words::downcase_previous_word_command);
        register(editor, &weak, words::capitalize_previous_word_command);
        register(editor, &weak, kill::kill_line_command);
        register(editor, &weak, kill::kill_region_or_line_command);
        register(editor, &weak, kill::copy_region_or_line_command);
    })
    .detach();
}

/// Registers `handler` for `A` on `editor`. The editor's action listener gets the app, not the
/// editor, so the handler reaches the editor through its weak handle; each run records itself
/// as the window's last command, so a dispatch by name counts too.
fn register<A: Action>(
    editor: &mut Editor,
    weak: &gpui::WeakEntity<Editor>,
    handler: fn(&A, &Entity<Editor>, &mut Window, &mut App),
) {
    let weak = weak.clone();
    editor
        .register_action(move |action: &A, window, cx| {
            let Some(editor) = weak.upgrade() else {
                return;
            };
            handler(action, &editor, window, cx);
            last_command::record(window, Some(action.name()), cx);
        })
        .detach();
}
