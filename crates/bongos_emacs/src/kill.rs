//! Line kills: kill-line, and the whole-line-or-region kill and copy. Every one goes through
//! `apply::run`, which hands the killed text to the kill sink before the buffer changes.

use bongos_emacs_model::{Plan, copy_region_or_line, kill_line, kill_region_or_line};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{CopyRegionOrLine, KillLine, KillRegionOrLine};
use crate::apply::{self, Command};

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
