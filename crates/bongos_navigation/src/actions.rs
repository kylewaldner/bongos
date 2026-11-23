//! The `navigation` actions. The seven openers start a picker; the others act on the open one
//! and say `No navigation picker` when there is none. Each routes to the module that owns it.

use bongos_emacs::prefix_arg::{Meaning, declare};
use gpui::{App, Context, Window, actions};
use workspace::Workspace;

use crate::{find, goto_file, lines, switcher, switcher_kill, target};

actions!(
    navigation,
    [
        /// Opens the buffer switcher.
        SwitchBuffer,
        /// Kills the highlighted buffer in the buffer switcher.
        KillBuffer,
        /// Opens the highlighted row in a pane chosen by letter.
        OpenInPane,
        /// Opens the go to file picker; a chosen file opens in the current pane.
        GoToFile,
        /// Opens the go to file picker; a chosen file opens in another pane.
        GoToFileOtherPane,
        /// Opens the go to file picker; a chosen file opens in a split to the right.
        GoToFileSplitRight,
        /// Opens the go to file picker; a chosen file opens in a split below.
        GoToFileSplitDown,
        /// Opens the find in files picker.
        FindInFiles,
        /// Switches regex matching in find in files.
        ToggleRegex,
        /// Switches case sensitivity in find in files.
        ToggleCaseSensitive,
        /// Switches whole word matching in find in files.
        ToggleWholeWord,
        /// Limits find in files to the open buffers.
        ScopeOpenBuffers,
        /// Limits find in files to a directory.
        ScopeDirectory,
        /// Shows or hides the replacement field of find in files.
        ToggleReplace,
        /// Replaces every match of find in files.
        ReplaceAll,
        /// Opens the results of find in files in a multibuffer.
        ExportToMultibuffer,
        /// Opens the picker over the lines of the current buffer.
        SearchBufferLines,
    ]
);

/// The actions that read a prefix argument.
pub const READS: [&str; 1] = ["navigation::FindInFiles"];

/// The actions that have no use for a prefix argument.
pub const IGNORED: [&str; 16] = [
    "navigation::SwitchBuffer",
    "navigation::KillBuffer",
    "navigation::OpenInPane",
    "navigation::GoToFile",
    "navigation::GoToFileOtherPane",
    "navigation::GoToFileSplitRight",
    "navigation::GoToFileSplitDown",
    "navigation::ToggleRegex",
    "navigation::ToggleCaseSensitive",
    "navigation::ToggleWholeWord",
    "navigation::ScopeOpenBuffers",
    "navigation::ScopeDirectory",
    "navigation::ToggleReplace",
    "navigation::ReplaceAll",
    "navigation::ExportToMultibuffer",
    "navigation::SearchBufferLines",
];

/// The actions that run in an open picker, which echo when none is open.
pub const IN_SESSION: [&str; 10] = [
    "navigation::KillBuffer",
    "navigation::OpenInPane",
    "navigation::ToggleRegex",
    "navigation::ToggleCaseSensitive",
    "navigation::ToggleWholeWord",
    "navigation::ScopeOpenBuffers",
    "navigation::ScopeDirectory",
    "navigation::ToggleReplace",
    "navigation::ReplaceAll",
    "navigation::ExportToMultibuffer",
];

type Handler = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// Declares the prefix argument of every action and registers the handlers of every workspace.
pub fn init(cx: &mut App) {
    for name in READS {
        declare(cx, name, Meaning::Reads);
    }
    for name in IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        macro_rules! route {
            ($($action:ident => $handler:path),* $(,)?) => {$(
                workspace.register_action(|workspace, _: &$action, window, cx| {
                    let handler: Handler = $handler;
                    handler(workspace, window, cx)
                });
            )*};
        }
        route! {
            SwitchBuffer => switcher::open,
            KillBuffer => switcher_kill::kill,
            OpenInPane => target::open_in_pane,
            GoToFile => goto_file::open_current,
            GoToFileOtherPane => goto_file::open_other_pane,
            GoToFileSplitRight => goto_file::open_split_right,
            GoToFileSplitDown => goto_file::open_split_down,
            FindInFiles => find::open,
            ToggleRegex => find::toggles::regex,
            ToggleCaseSensitive => find::toggles::case_sensitive,
            ToggleWholeWord => find::toggles::whole_word,
            ScopeOpenBuffers => find::engine::scope_open_buffers,
            ScopeDirectory => find::engine::scope_directory,
            ToggleReplace => find::replace::toggle,
            ReplaceAll => find::replace::replace_all,
            ExportToMultibuffer => find::export::to_multibuffer,
            SearchBufferLines => lines::open,
        }
    })
    .detach();
}

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;
