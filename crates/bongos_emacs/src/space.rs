//! Whitespace commands: delete-horizontal-space, just-one-space and delete-blank-lines. Every
//! one goes through `apply::run` and acts at each caret's point; a region is cleared.

use bongos_emacs_model::{Plan, delete_blank_lines, delete_horizontal_space, just_one_space};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{DeleteBlankLines, DeleteHorizontalSpace, JustOneSpace};
use crate::apply::{self, Command};

macro_rules! space_command {
    ($handler:ident, $action:ident, $model:ident) => {
        pub(crate) fn $handler(
            action: &$action,
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
                |text, carets| -> Plan { $model(text, carets, count) },
            );
        }
    };
}

space_command!(
    delete_horizontal_space_command,
    DeleteHorizontalSpace,
    delete_horizontal_space
);
space_command!(just_one_space_command, JustOneSpace, just_one_space);

pub(crate) fn delete_blank_lines_command(
    action: &DeleteBlankLines,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    apply::run(
        editor,
        window,
        cx,
        Command::edit(action.name()),
        |text, carets| -> Plan { delete_blank_lines(text, carets, None) },
    );
}
