//! Line kills: kill-line, and the whole-line-or-region kill and copy. Every one goes through
//! `apply::run`, which hands the killed text to the kill sink before the buffer changes.

use bongos_emacs_model::{
    Plan, copy_region_or_line, kill_line, kill_region_or_line, kill_whole_line,
};
use bongos_minibuffer::message;
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{AppendNextKill, CopyRegionOrLine, KillLine, KillRegionOrLine, KillWholeLine};
use crate::apply::{self, Command};
use crate::kill_ring_sink::APPEND_NEXT_KILL;
use crate::last_command::last_command;

pub(crate) fn kill_line_command(
    action: &KillLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()),
        |text, carets| -> Plan { kill_line(text, carets, count) },
    );
}

pub(crate) fn kill_region_or_line_command(
    action: &KillRegionOrLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()).with_whole_lines(),
        |text, carets| -> Plan { kill_region_or_line(text, carets, count) },
    );
}

pub(crate) fn copy_region_or_line_command(
    action: &CopyRegionOrLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::copy(action.name()).with_whole_lines(),
        |text, carets| -> Plan { copy_region_or_line(text, carets, count) },
    );
}

pub(crate) fn kill_whole_line_command(
    action: &KillWholeLine,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = action.count;
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()).with_whole_lines(),
        |text, carets| -> Plan { kill_whole_line(text, carets, count) },
    );
}

/// `append-next-kill`: records no edit; the kill sink sees it as the last command and joins
/// the next kill onto the newest entry. Pressed again while armed, it stays quiet.
pub(crate) fn append_next_kill_command(
    _: &AppendNextKill,
    _: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if last_command(window, cx) != Some(APPEND_NEXT_KILL) {
        message(window, cx, "If the next command is a kill, it will append");
    }
}
