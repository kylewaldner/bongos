//! Word kills and word case commands. Every kill goes through `apply::run`, which hands the
//! killed words to the kill sink before the buffer changes.

use bongos_emacs_model::{
    Plan, backward_kill_word, capitalize_previous_word, capitalize_word, downcase_previous_word,
    downcase_word, kill_word, upcase_previous_word, upcase_word,
};
use editor::Editor;
use gpui::{Action as _, App, Entity, Window};

use crate::actions::{
    BackwardKillWord, CapitalizePreviousWord, CapitalizeWord, DowncasePreviousWord, DowncaseWord,
    KillWord, UpcasePreviousWord, UpcaseWord,
};
use crate::apply::{self, Command};

macro_rules! word_command {
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

word_command!(kill_word_command, KillWord, kill_word);
word_command!(
    backward_kill_word_command,
    BackwardKillWord,
    backward_kill_word
);
word_command!(upcase_word_command, UpcaseWord, upcase_word);
word_command!(downcase_word_command, DowncaseWord, downcase_word);
word_command!(capitalize_word_command, CapitalizeWord, capitalize_word);
word_command!(
    upcase_previous_word_command,
    UpcasePreviousWord,
    upcase_previous_word
);
word_command!(
    downcase_previous_word_command,
    DowncasePreviousWord,
    downcase_previous_word
);
word_command!(
    capitalize_previous_word_command,
    CapitalizePreviousWord,
    capitalize_previous_word
);
