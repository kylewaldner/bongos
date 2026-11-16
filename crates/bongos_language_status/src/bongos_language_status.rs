//! The language status line: one status bar item that says what is wrong with, or what is being
//! installed for, a language's server (a download with progress, a server that could not be
//! installed or started, a missing tool another crate reported), and `languages.log` with the
//! detail. It renders from state and never blocks.

mod log;
mod notices;
mod state;
mod status_item;

use std::path::Path;

use gpui::{App, actions};

pub use log::{Clock, LanguagesLog, MAX_LOG_BYTES, system_clock};
pub use notices::{LanguageNotices, NoticeList};
pub use state::{
    Command, Download, Failure, IGNORED_SERVER, Kind, Line, Notice, StatusState, Tone,
    language_for_server, languages_serving, menu_commands, status_line,
};
pub use status_item::{LanguageStatusItem, show_logs};

actions!(
    languages,
    [
        /// Opens `languages.log`, which has the downloads, failures and notices of language
        /// servers.
        ShowLogs,
        /// Installs and starts again the language server the status line reports as unavailable.
        RetryServer,
        /// Cancels the language server download in progress.
        CancelDownload,
    ]
);

/// Sets where `languages.log` lives. `logs_dir` comes from the caller, never from `paths`.
pub fn init(logs_dir: &Path, cx: &mut App) {
    init_with_clock(logs_dir, system_clock(), cx);
}

/// `init` with the clock the log's timestamps come from.
pub fn init_with_clock(logs_dir: &Path, clock: Clock, cx: &mut App) {
    cx.set_global(notices::LogGlobal(LanguagesLog::new(logs_dir, clock)));
    // Create the notice list so every status item observes the same one.
    LanguageNotices::list(cx);
}
