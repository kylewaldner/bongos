//! What the status line says, as plain data and pure functions: which entry is shown, its text and
//! its tone, the menu, and the log lines each change produces. The item only renders this.

use language::{LanguageName, LanguageRegistry};
use lsp::LanguageServerName;
use settings::WorktreeId;

/// The Kotlin server reports itself; the status line never shows it.
pub const IGNORED_SERVER: &str = "kotlin";

/// Progress is logged at every quarter.
const MILESTONE_PERCENT: u64 = 25;

pub fn is_ignored(server: &LanguageServerName) -> bool {
    server.0.as_ref() == IGNORED_SERVER
}

/// A server that could not be installed or started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub server: LanguageServerName,
    pub language: String,
    /// The first line of the cause; the log has all of it.
    pub reason: String,
}

/// A download or install in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub server: LanguageServerName,
    pub language: String,
    /// `None` until the first progress report: the install has no byte counts.
    pub progress: Option<(u64, Option<u64>)>,
    /// The highest quarter (0, 25, 50, 75, 100) already logged.
    pub logged_milestone: u64,
}

/// A precondition that code reported through `LanguageNotices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub worktree_id: WorktreeId,
    pub language: String,
    pub key: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Unavailable,
    Notice,
    Downloading,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
    pub tone: Tone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ShowLogs,
    Retry,
    OpenProtocolLog,
    CancelDownload,
    Dismiss,
}

#[derive(Clone, Debug, Default)]
pub struct StatusState {
    pub failures: Vec<Failure>,
    pub downloads: Vec<Download>,
}

impl StatusState {
    /// A server failed. Replaces an earlier failure of the same server and ends its download.
    /// Returns the log line.
    pub fn fail(&mut self, server: &LanguageServerName, language: &str, error: &str) -> String {
        self.downloads.retain(|download| download.server != *server);
        self.failures.retain(|failure| failure.server != *server);
        let reason = error
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("unknown error")
            .to_string();
        self.failures.push(Failure {
            server: server.clone(),
            language: language.to_string(),
            reason,
        });
        format!(
            "{language}: {server} failed: {}",
            cause_without_stderr(error).trim()
        )
    }

    /// A download or install started (or a new attempt began). Returns the log line when it is new.
    pub fn start_download(
        &mut self,
        server: &LanguageServerName,
        language: &str,
    ) -> Option<String> {
        self.failures.retain(|failure| failure.server != *server);
        if self
            .downloads
            .iter()
            .any(|download| download.server == *server)
        {
            return None;
        }
        self.downloads.push(Download {
            server: server.clone(),
            language: language.to_string(),
            progress: None,
            logged_milestone: 0,
        });
        Some(format!("{language}: downloading {server}"))
    }

    /// Byte counts for a download. Returns a log line when a quarter was crossed.
    pub fn progress(
        &mut self,
        server: &LanguageServerName,
        language: &str,
        done: u64,
        total: Option<u64>,
    ) -> Option<String> {
        self.start_download(server, language);
        let download = self
            .downloads
            .iter_mut()
            .find(|download| download.server == *server)?;
        download.progress = Some((done, total));
        let total = total.filter(|total| *total > 0)?;
        let percent = percent(done, total);
        let milestone = percent / MILESTONE_PERCENT * MILESTONE_PERCENT;
        if milestone <= download.logged_milestone {
            return None;
        }
        download.logged_milestone = milestone;
        Some(format!(
            "{}: download {milestone}% ({} of {})",
            download.language,
            megabytes(done),
            megabytes(total)
        ))
    }

    /// The server started or is checking for updates: a failure of it is over, and so is its
    /// download. Returns the log line when something was cleared.
    pub fn succeeded(&mut self, server: &LanguageServerName) -> Option<String> {
        let failures = self.failures.len();
        self.failures.retain(|failure| failure.server != *server);
        let download = self
            .downloads
            .iter()
            .position(|download| download.server == *server)
            .map(|index| self.downloads.remove(index));
        match download {
            Some(download) => Some(format!("{}: installed {server}", download.language)),
            None if self.failures.len() != failures => Some(format!("{server}: available again")),
            None => None,
        }
    }

    /// The server stopped: its download is over, a failure stays.
    pub fn stopped(&mut self, server: &LanguageServerName) {
        self.downloads.retain(|download| download.server != *server);
    }

    /// Removes the entry `line` shows. Notices live outside this state; the caller removes those.
    pub fn dismiss(&mut self, line: &Line) {
        match line.kind {
            Kind::Unavailable => {
                if !self.failures.is_empty() {
                    self.failures.remove(0);
                }
            }
            Kind::Downloading => {
                if !self.downloads.is_empty() {
                    self.downloads.remove(0);
                }
            }
            Kind::Notice => {}
        }
    }
}

/// The part of a failure's cause before the captured stderr, which is logged on its own.
pub fn cause_without_stderr(error: &str) -> &str {
    error.split("\n-- stderr --\n").next().unwrap_or(error)
}

fn percent(done: u64, total: u64) -> u64 {
    ((done.saturating_mul(100) + total / 2) / total).min(100)
}

fn mb(bytes: u64) -> u64 {
    (bytes + 512 * 1024) / (1024 * 1024)
}

fn megabytes(bytes: u64) -> String {
    format!("{} MB", mb(bytes))
}

/// The line to show, most urgent first: unavailable, notice, downloading. `None` hides the item.
pub fn status_line(state: &StatusState, notices: &[Notice]) -> Option<Line> {
    if let Some(first) = state.failures.first() {
        let text = format!(
            "{} server unavailable: {}{}",
            first.language,
            first.reason,
            more(state.failures.len())
        );
        return Some(Line {
            kind: Kind::Unavailable,
            text,
            tone: Tone::Warning,
        });
    }
    if let Some(first) = notices.first() {
        let text = format!("{}: {}{}", first.language, first.text, more(notices.len()));
        return Some(Line {
            kind: Kind::Notice,
            text,
            tone: Tone::Warning,
        });
    }
    let first = state.downloads.first()?;
    let text = match first.progress {
        Some((done, Some(total))) if total > 0 => format!(
            "{}: downloading {} {}% ({}/{} MB)",
            first.language,
            first.server,
            percent(done, total),
            mb(done),
            mb(total)
        ),
        Some((done, _)) => format!(
            "{}: downloading {} ({} MB)",
            first.language,
            first.server,
            mb(done)
        ),
        None => format!("{}: installing {}", first.language, first.server),
    };
    Some(Line {
        kind: Kind::Downloading,
        text: format!("{text}{}", more(state.downloads.len())),
        tone: Tone::Normal,
    })
}

fn more(count: usize) -> String {
    match count.saturating_sub(1) {
        0 => String::new(),
        others => format!(" (+{others} more)"),
    }
}

/// The menu for the shown line: logs always, retry for a failure, protocol log, cancel while a
/// download runs, dismiss.
pub fn menu_commands(state: &StatusState, line: &Line) -> Vec<Command> {
    let mut commands = vec![Command::ShowLogs];
    if line.kind == Kind::Unavailable {
        commands.push(Command::Retry);
    }
    commands.push(Command::OpenProtocolLog);
    if !state.downloads.is_empty() {
        commands.push(Command::CancelDownload);
    }
    commands.push(Command::Dismiss);
    commands
}

/// The names of the languages whose adapters include `server`.
pub fn languages_serving(
    registry: &LanguageRegistry,
    server: &LanguageServerName,
) -> Vec<LanguageName> {
    registry
        .language_names()
        .into_iter()
        .filter(|name| {
            registry
                .lsp_adapters(name)
                .iter()
                .any(|adapter| adapter.name == *server)
        })
        .collect()
}

/// The language to name for `server`: the one that started it (`hint`) when it serves several,
/// else the first. Falls back to the server's own name.
pub fn language_for_server(
    registry: &LanguageRegistry,
    server: &LanguageServerName,
    hint: Option<&LanguageName>,
) -> String {
    let serving = languages_serving(registry, server);
    hint.filter(|hint| serving.contains(hint))
        .or(serving.first())
        .map_or_else(|| server.to_string(), |name| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn server(name: &'static str) -> LanguageServerName {
        LanguageServerName::new_static(name)
    }

    fn notice(language: &str, text: &str) -> Notice {
        Notice {
            worktree_id: WorktreeId::from_usize(1),
            language: language.into(),
            key: "key".into(),
            text: text.into(),
        }
    }

    #[test]
    fn test_nothing_to_say_hides_the_line() {
        assert_eq!(status_line(&StatusState::default(), &[]), None);
    }

    #[test]
    fn test_unavailable_text_and_color() {
        let mut state = StatusState::default();
        state.fail(
            &server("jdtls"),
            "Java",
            "could not download jdtls\nmore detail",
        );
        let line = status_line(&state, &[]).unwrap();
        assert_eq!(
            line.text,
            "Java server unavailable: could not download jdtls"
        );
        assert_eq!(line.kind, Kind::Unavailable);
        assert_eq!(line.tone, Tone::Warning);
    }

    #[test]
    fn test_notice_text_and_color() {
        let text = "no terraform or tofu on PATH; provider schemas and formatting are unavailable";
        let line = status_line(&StatusState::default(), &[notice("Terraform", text)]).unwrap();
        assert_eq!(line.text, format!("Terraform: {text}"));
        assert_eq!(line.kind, Kind::Notice);
        assert_eq!(line.tone, Tone::Warning);
    }

    #[test]
    fn test_downloading_text_with_and_without_byte_counts() {
        let mut state = StatusState::default();
        state.start_download(&server("rust-analyzer"), "Rust");
        assert_eq!(
            status_line(&state, &[]).unwrap().text,
            "Rust: installing rust-analyzer"
        );
        state.progress(
            &server("rust-analyzer"),
            "Rust",
            6 * MB + 1000,
            Some(16 * MB),
        );
        let line = status_line(&state, &[]).unwrap();
        assert_eq!(line.text, "Rust: downloading rust-analyzer 38% (6/16 MB)");
        assert_eq!(line.kind, Kind::Downloading);
        assert_eq!(line.tone, Tone::Normal);
        state.progress(&server("rust-analyzer"), "Rust", 3 * MB, None);
        assert_eq!(
            status_line(&state, &[]).unwrap().text,
            "Rust: downloading rust-analyzer (3 MB)"
        );
    }

    #[test]
    fn test_most_urgent_state_wins() {
        let mut state = StatusState::default();
        state.start_download(&server("a"), "A");
        assert_eq!(status_line(&state, &[]).unwrap().kind, Kind::Downloading);
        let notices = [notice("T", "x")];
        assert_eq!(status_line(&state, &notices).unwrap().kind, Kind::Notice);
        state.fail(&server("b"), "B", "broken");
        assert_eq!(
            status_line(&state, &notices).unwrap().kind,
            Kind::Unavailable
        );
    }

    #[test]
    fn test_more_than_one_failed_language_adds_a_count() {
        let mut state = StatusState::default();
        state.fail(&server("jdtls"), "Java", "no network");
        state.fail(&server("ruff"), "Python", "no network");
        state.fail(&server("gopls"), "Go", "no network");
        assert_eq!(
            status_line(&state, &[]).unwrap().text,
            "Java server unavailable: no network (+2 more)"
        );
        let notices = [notice("A", "one"), notice("B", "two")];
        assert_eq!(
            status_line(&StatusState::default(), &notices).unwrap().text,
            "A: one (+1 more)"
        );
    }

    #[test]
    fn test_progress_logs_every_quarter_once() {
        let mut state = StatusState::default();
        let s = server("rust-analyzer");
        assert_eq!(
            state.start_download(&s, "Rust").unwrap(),
            "Rust: downloading rust-analyzer"
        );
        assert!(state.start_download(&s, "Rust").is_none());
        assert!(
            state
                .progress(&s, "Rust", 10 * MB / 10, Some(10 * MB))
                .is_none()
        );
        assert_eq!(
            state.progress(&s, "Rust", 3 * MB, Some(10 * MB)).unwrap(),
            "Rust: download 25% (3 MB of 10 MB)"
        );
        assert!(state.progress(&s, "Rust", 4 * MB, Some(10 * MB)).is_none());
        assert!(
            state
                .progress(&s, "Rust", 5 * MB, Some(10 * MB))
                .unwrap()
                .contains("50%")
        );
        assert!(
            state
                .progress(&s, "Rust", 10 * MB, Some(10 * MB))
                .unwrap()
                .contains("100%")
        );
    }

    #[test]
    fn test_success_clears_the_entry_and_failure_clears_on_retry() {
        let mut state = StatusState::default();
        let s = server("jdtls");
        state.fail(&s, "Java", "boom");
        // The retry starts with a download, which clears the failure.
        state.start_download(&s, "Java");
        assert!(state.failures.is_empty());
        assert_eq!(state.succeeded(&s).unwrap(), "Java: installed jdtls");
        assert_eq!(status_line(&state, &[]), None);
        state.fail(&s, "Java", "boom");
        assert_eq!(state.succeeded(&s).unwrap(), "jdtls: available again");
        assert!(state.succeeded(&s).is_none());
    }

    #[test]
    fn test_stopped_ends_a_download_but_keeps_a_failure() {
        let mut state = StatusState::default();
        state.start_download(&server("a"), "A");
        state.fail(&server("b"), "B", "x");
        state.stopped(&server("a"));
        state.stopped(&server("b"));
        assert!(state.downloads.is_empty());
        assert_eq!(state.failures.len(), 1);
    }

    #[test]
    fn test_kotlin_is_ignored() {
        assert!(is_ignored(&server("kotlin")));
        assert!(!is_ignored(&server("jdtls")));
    }

    #[test]
    fn test_menu_has_cancel_only_while_a_download_runs() {
        let mut state = StatusState::default();
        state.fail(&server("a"), "A", "x");
        let line = status_line(&state, &[]).unwrap();
        assert_eq!(
            menu_commands(&state, &line),
            [
                Command::ShowLogs,
                Command::Retry,
                Command::OpenProtocolLog,
                Command::Dismiss
            ]
        );
        state.start_download(&server("b"), "B");
        let line = status_line(&state, &[]).unwrap();
        assert!(menu_commands(&state, &line).contains(&Command::CancelDownload));
    }

    #[test]
    fn test_dismiss_removes_the_shown_entry() {
        let mut state = StatusState::default();
        state.fail(&server("a"), "A", "x");
        state.fail(&server("b"), "B", "y");
        let line = status_line(&state, &[]).unwrap();
        state.dismiss(&line);
        assert_eq!(state.failures.len(), 1);
        assert_eq!(state.failures[0].language, "B");
    }

    #[test]
    fn test_stderr_section_is_split_from_the_cause() {
        assert_eq!(
            cause_without_stderr("spawn failed\n-- stderr --\nboom"),
            "spawn failed"
        );
        assert_eq!(cause_without_stderr("plain"), "plain");
    }
}
