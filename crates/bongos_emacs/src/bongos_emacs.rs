//! The Emacs commands on top of the editor: the `emacs` actions, the one path that applies a
//! command's plan, the kill sink, the last-command tracker and the C-g hooks. The text logic
//! lives in `bongos_emacs_model`.

pub mod actions;
pub mod apply;
mod comments;
mod cursor;
mod goto_line;
mod kill;
pub mod kill_sink;
pub mod last_command;
mod lines;
mod motion;
pub mod quit;
mod recenter;
pub mod region;
mod region_commands;
pub mod snapshot_text;
mod space;
mod transpose;
mod words;
mod zap;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod comments_tests;
#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod goto_line_tests;
#[cfg(test)]
mod kill_sink_tests;
#[cfg(test)]
mod kill_tests;
#[cfg(test)]
mod last_command_tests;
#[cfg(test)]
mod lines_tests;
#[cfg(test)]
mod motion_tests;
#[cfg(test)]
mod quit_tests;
#[cfg(test)]
mod recenter_tests;
#[cfg(test)]
mod region_tests;
#[cfg(test)]
mod space_tests;
#[cfg(test)]
mod transpose_tests;
#[cfg(test)]
mod words_tests;
#[cfg(test)]
mod zap_tests;

use editor::Editor;
use gpui::{Action, App, Entity, Window};

pub use kill_sink::{Kill, KillDirection, KillKind, set_kill_sink};
pub use last_command::last_command;

/// Registers the implemented `emacs` commands on every editor and starts the last-command
/// tracker. Actions without a handler stay undispatchable, so only working commands reach M-x.
pub fn init(cx: &mut App) {
    last_command::init(cx);
    quit::init(cx);
    cursor::init(cx);
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
        register(editor, &weak, space::delete_horizontal_space_command);
        register(editor, &weak, space::just_one_space_command);
        register(editor, &weak, space::delete_blank_lines_command);
        register(editor, &weak, transpose::transpose_chars_command);
        register(editor, &weak, transpose::transpose_words_command);
        register(editor, &weak, transpose::transpose_words_reverse_command);
        register(editor, &weak, transpose::transpose_lines_command);
        register(editor, &weak, lines::move_line_up_command);
        register(editor, &weak, lines::move_line_down_command);
        register(editor, &weak, lines::duplicate_line_command);
        register(editor, &weak, lines::copy_from_above_command);
        register(editor, &weak, comments::comment_dwim_command);
        register(editor, &weak, comments::duplicate_and_comment_command);
        register(editor, &weak, zap::zap_to_char_command);
        register(editor, &weak, zap::zap_up_to_char_command);
        register(editor, &weak, zap::zap_back_to_char_command);
        register(editor, &weak, zap::zap_back_up_to_char_command);
        register(editor, &weak, recenter::recenter_command);
        register(editor, &weak, recenter::reposition_command);
        register(editor, &weak, goto_line::goto_line_command);
        register(editor, &weak, region_commands::expand_region_command);
        register(editor, &weak, region_commands::contract_region_command);
        register(editor, &weak, quit::keyboard_quit_command);
        recenter::init_editor(cx);
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
