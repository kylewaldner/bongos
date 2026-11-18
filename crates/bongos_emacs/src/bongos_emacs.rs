//! The Emacs commands on top of the editor: the `emacs` actions, the one path that applies a
//! command's plan, the kill sink and its kill ring writer, the last-command tracker and the C-g
//! hooks. The text logic lives in `bongos_emacs_model`.

pub mod actions;
pub mod apply;
mod browse_kill_ring;
mod comments;
mod count_adapters;
mod cursor;
mod global_mark;
mod goto_line;
mod intercept;
mod jumps;
mod kill;
pub mod kill_ring_sink;
pub mod kill_ring_store;
pub mod kill_sink;
pub mod last_command;
mod lines;
pub mod mark;
pub mod meanings;
mod motion;
pub mod prefix_arg;
mod prefix_commands;
pub mod quit;
pub mod recenter;
mod rectangle;
mod rectangle_mode;
pub mod region;
mod region_commands;
mod repeat;
pub mod snapshot_text;
mod space;
mod string_rectangle;
mod transpose;
mod words;
mod yank;
mod zap;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod browse_kill_ring_tests;
#[cfg(test)]
mod comments_tests;
#[cfg(test)]
mod count_adapters_tests;
#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod global_mark_tests;
#[cfg(test)]
mod goto_line_tests;
#[cfg(test)]
mod intercept_tests;
#[cfg(test)]
mod jumps_tests;
#[cfg(test)]
mod kill_ring_sink_tests;
#[cfg(test)]
mod kill_ring_store_tests;
#[cfg(test)]
mod kill_sink_tests;
#[cfg(test)]
mod kill_tests;
#[cfg(test)]
mod last_command_tests;
#[cfg(test)]
mod lines_tests;
#[cfg(test)]
mod mark_tests;
#[cfg(test)]
mod motion_tests;
#[cfg(test)]
mod prefix_arg_tests;
#[cfg(test)]
mod quit_tests;
#[cfg(test)]
mod recenter_tests;
#[cfg(test)]
mod rectangle_mode_tests;
#[cfg(test)]
mod rectangle_tests;
#[cfg(test)]
mod rectangle_yank_tests;
#[cfg(test)]
mod region_tests;
#[cfg(test)]
mod repeat_tests;
#[cfg(test)]
mod space_tests;
#[cfg(test)]
mod string_rectangle_tests;
#[cfg(test)]
mod transpose_tests;
#[cfg(test)]
mod words_tests;
#[cfg(test)]
mod yank_tests;
#[cfg(test)]
mod zap_tests;

use editor::Editor;
use gpui::{App, Entity, Window};

use crate::prefix_arg::CountArg;

pub use kill_sink::{Kill, KillDirection, KillKind, set_kill_sink};
pub use last_command::last_command;

/// Registers the implemented `emacs` commands on every editor and starts the last-command
/// tracker. Actions without a handler stay undispatchable, so only working commands reach M-x.
pub fn init(cx: &mut App) {
    last_command::init(cx);
    kill_ring_store::init(cx);
    kill_ring_sink::init(cx);
    quit::init(cx);
    prefix_arg::init(cx);
    count_adapters::init(cx);
    cursor::init(cx);
    global_mark::init(cx);
    rectangle_mode::init(cx);
    cx.observe_new(|editor: &mut Editor, mut window, cx| {
        let weak = cx.entity().downgrade();
        register(editor, &weak, cx, motion::forward_word);
        register(editor, &weak, cx, motion::backward_word);
        register(editor, &weak, cx, words::kill_word_command);
        register(editor, &weak, cx, words::backward_kill_word_command);
        register(editor, &weak, cx, words::upcase_word_command);
        register(editor, &weak, cx, words::downcase_word_command);
        register(editor, &weak, cx, words::capitalize_word_command);
        register(editor, &weak, cx, words::upcase_previous_word_command);
        register(editor, &weak, cx, words::downcase_previous_word_command);
        register(editor, &weak, cx, words::capitalize_previous_word_command);
        register(editor, &weak, cx, kill::kill_line_command);
        register(editor, &weak, cx, kill::kill_region_or_line_command);
        register(editor, &weak, cx, kill::copy_region_or_line_command);
        register(editor, &weak, cx, kill::kill_whole_line_command);
        register(editor, &weak, cx, kill::append_next_kill_command);
        register(editor, &weak, cx, yank::yank_command);
        register(editor, &weak, cx, yank::yank_pop_command);
        register(
            editor,
            &weak,
            cx,
            browse_kill_ring::browse_kill_ring_command,
        );
        register(editor, &weak, cx, space::delete_horizontal_space_command);
        register(editor, &weak, cx, space::just_one_space_command);
        register(editor, &weak, cx, space::delete_blank_lines_command);
        register(editor, &weak, cx, transpose::transpose_chars_command);
        register(editor, &weak, cx, transpose::transpose_words_command);
        register(
            editor,
            &weak,
            cx,
            transpose::transpose_words_reverse_command,
        );
        register(editor, &weak, cx, transpose::transpose_lines_command);
        register(editor, &weak, cx, lines::move_line_up_command);
        register(editor, &weak, cx, lines::move_line_down_command);
        register(editor, &weak, cx, lines::duplicate_line_command);
        register(editor, &weak, cx, lines::open_line_command);
        register(editor, &weak, cx, lines::copy_from_above_command);
        register(editor, &weak, cx, comments::comment_dwim_command);
        register(editor, &weak, cx, comments::duplicate_and_comment_command);
        register(editor, &weak, cx, zap::zap_to_char_command);
        register(editor, &weak, cx, zap::zap_up_to_char_command);
        register(editor, &weak, cx, zap::zap_back_to_char_command);
        register(editor, &weak, cx, zap::zap_back_up_to_char_command);
        register(editor, &weak, cx, recenter::recenter_command);
        register(editor, &weak, cx, recenter::reposition_command);
        register(editor, &weak, cx, goto_line::goto_line_command);
        register(editor, &weak, cx, region_commands::expand_region_command);
        register(editor, &weak, cx, region_commands::contract_region_command);
        register(editor, &weak, cx, quit::keyboard_quit_command);
        register(editor, &weak, cx, mark::set_mark_command);
        register(editor, &weak, cx, mark::exchange_point_and_mark_command);
        register(editor, &weak, cx, mark::pop_mark_command);
        register(editor, &weak, cx, jumps::beginning_of_buffer_command);
        register(editor, &weak, cx, jumps::end_of_buffer_command);
        register(editor, &weak, cx, jumps::xref_find_definitions_command);
        register(editor, &weak, cx, jumps::xref_find_implementations_command);
        register(editor, &weak, cx, rectangle::rectangle_mark_mode_command);
        register(editor, &weak, cx, rectangle::copy_rectangle_as_kill_command);
        register(editor, &weak, cx, rectangle::kill_rectangle_command);
        register(editor, &weak, cx, rectangle::delete_rectangle_command);
        register(editor, &weak, cx, rectangle::open_rectangle_command);
        register(editor, &weak, cx, rectangle::clear_rectangle_command);
        register(
            editor,
            &weak,
            cx,
            string_rectangle::string_rectangle_command,
        );
        register(editor, &weak, cx, rectangle::yank_rectangle_command);
        register(editor, &weak, cx, rectangle::keep_rectangle_command);
        register(
            editor,
            &weak,
            cx,
            prefix_commands::universal_argument_command,
        );
        register(editor, &weak, cx, prefix_commands::digit_argument_command);
        register(
            editor,
            &weak,
            cx,
            prefix_commands::negative_argument_command,
        );
        prefix_arg::init_editor(editor, window.as_deref_mut(), cx);
        recenter::init_editor(cx);
        kill_ring_sink::init_editor(cx);
        rectangle_mode::init_editor(editor, window.as_deref_mut(), cx);
        mark::init_editor(window, cx);
    })
    .detach();
}

/// Registers `handler` for `A` on `editor`. The editor's action listener gets the app, not the
/// editor, so the handler reaches the editor through its weak handle; each run records itself
/// as the window's last command, so a dispatch by name counts too. The prefix argument handed
/// to the command is taken here, so it is gone after one run; it becomes the action's count
/// unless the action already carries one. `A` is declared as reading the argument when it has a
/// count and as ignoring it otherwise.
fn register<A: CountArg>(
    editor: &mut Editor,
    weak: &gpui::WeakEntity<Editor>,
    cx: &mut App,
    handler: fn(&A, &Entity<Editor>, &mut Window, &mut App),
) {
    let meaning = if A::READS || A::BUILDS {
        prefix_arg::Meaning::Reads
    } else {
        prefix_arg::Meaning::Ignored
    };
    prefix_arg::declare(cx, A::name_for_type(), meaning);
    let weak = weak.clone();
    editor
        .register_action(move |action: &A, window, cx| {
            let Some(editor) = weak.upgrade() else {
                return;
            };
            if A::BUILDS {
                handler(action, &editor, window, cx);
                return;
            }
            let handed = prefix_arg::take(window, cx);
            let clamped =
                A::INSERTS && handed.is_some_and(|arg| arg.value() > prefix_arg::MAX_INSERT);
            match prefix_arg::counted(action, handed) {
                Some(counted) => handler(&counted, &editor, window, cx),
                None => handler(action, &editor, window, cx),
            }
            if clamped {
                prefix_arg::show_limited(window, cx);
            }
            last_command::record(window, Some(action.name()), cx);
        })
        .detach();
}
