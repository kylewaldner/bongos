//! What the status item shows and offers, as pure functions of a worktree's manager state. The view
//! (`status_item.rs`) only draws these values, so every state is tested without a window.

use settings::KotlinServerKind;
use ui::Color;

use crate::{
    intellij::EULA_NOT_ACCEPTED_REASON,
    jdk::{NO_JDK_REASON, SET_JDK_HOME_LABEL},
    manager::{EXPIRED_REASON, Fallback, FallbackReason, KotlinWorktreeState, ServerStatus},
    updates::DownloadProgress,
};

/// The longest status text drawn in the status bar; the tooltip has all of it.
pub const MAX_TEXT_CHARS: usize = 90;

/// Everything the text, color and menu depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// The server running or being started.
    pub effective: KotlinServerKind,
    pub status: ServerStatus,
    pub fallback: Option<Fallback>,
    pub gradle_error: Option<String>,
    /// The background download of a newer kotlin-lsp, if one runs.
    pub download: Option<DownloadProgress>,
    /// A manifest check or the planning of a download runs.
    pub checking: bool,
    /// The manifest check, for the menu's "Server updates" lines.
    pub updates: UpdatesInfo,
}

/// What the status menu says about the server-update manifest check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdatesInfo {
    /// `kotlin.manifest_url` is empty or refused.
    Off,
    /// Updates are on and no check has finished yet.
    NotChecked,
    Checked {
        /// The time of the last check, already formatted.
        when: String,
        /// The serial of the cached manifest, if one is cached.
        serial: Option<u64>,
        /// The last check's failure, if it failed.
        error: Option<String>,
    },
}

impl UpdatesInfo {
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Off => vec!["Server updates: off".into()],
            Self::NotChecked => vec!["Server updates: not checked yet".into()],
            Self::Checked {
                when,
                serial,
                error,
            } => {
                let mut text = format!("Server updates: checked {when}");
                if let Some(serial) = serial {
                    text.push_str(&format!(", serial {serial}"));
                }
                let mut lines = vec![text];
                if let Some(error) = error {
                    lines.push(format!("Last error: {error}"));
                }
                lines
            }
        }
    }
}

impl Snapshot {
    /// `None` while no server has started for the worktree: the item is hidden.
    pub fn of(state: &KotlinWorktreeState) -> Option<Self> {
        Some(Self {
            effective: state.effective,
            status: state.status.clone()?,
            fallback: state.fallback.clone(),
            gradle_error: state.gradle_error.clone(),
            download: None,
            checking: false,
            updates: UpdatesInfo::Off,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatusLine {
    pub text: String,
    pub color: Color,
    /// Work is in progress: the item shows a spinner.
    pub busy: bool,
}

fn display_name(server: KotlinServerKind) -> &'static str {
    match server {
        KotlinServerKind::KotlinLsp => "kotlin-lsp",
        KotlinServerKind::Intellij => "IntelliJ",
    }
}

fn megabytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn percent(done: u64, total: u64) -> u64 {
    ((done.saturating_mul(100) + total / 2) / total).min(100)
}

/// A server stopped by an expired build, while a newer build may be on its way: the text is the
/// work in progress, which never says "expired". Once the work ends the item shows the expired
/// status again.
fn expired_progress(snapshot: &Snapshot) -> Option<StatusLine> {
    let expired =
        matches!(&snapshot.status, ServerStatus::Failed { reason } if reason == EXPIRED_REASON);
    if !expired || snapshot.fallback.is_some() {
        return None;
    }
    let text = if let Some(download) = &snapshot.download {
        let mut text = format!("Kotlin: downloading kotlin-lsp {}", download.version);
        match download.total {
            Some(total) if total > 0 => {
                text.push_str(&format!(" ({}%)", percent(download.done, total)))
            }
            _ => {}
        }
        text
    } else if snapshot.checking {
        "Kotlin: checking for a newer Kotlin server".to_string()
    } else {
        return None;
    };
    Some(StatusLine {
        text,
        color: Color::Default,
        busy: true,
    })
}

pub fn status_line(snapshot: &Snapshot) -> StatusLine {
    if let Some(line) = expired_progress(snapshot) {
        return line;
    }
    let name = display_name(snapshot.effective);
    let mut text = format!("Kotlin: {name}");
    if let Some(fallback) = &snapshot.fallback {
        text.push_str(&format!(" (fallback: {})", fallback.reason));
    }
    let mut color = if snapshot.fallback.is_some() {
        Color::Warning
    } else {
        Color::Default
    };
    let mut busy = false;
    match &snapshot.status {
        ServerStatus::Installing { done, total } => {
            busy = true;
            text.push_str(", installing");
            match total {
                Some(total) if *total > 0 => text.push_str(&format!(
                    " {}% ({}/{} MB)",
                    percent(*done, *total),
                    megabytes(*done),
                    megabytes(*total)
                )),
                _ if megabytes(*done) > 0 => text.push_str(&format!(" {} MB", megabytes(*done))),
                _ => {}
            }
        }
        ServerStatus::Starting => {
            busy = true;
            text.push_str(", starting");
        }
        ServerStatus::Indexing { label } => {
            busy = true;
            text.push_str(&format!(", indexing {label}"));
        }
        ServerStatus::Ready => {
            if snapshot.fallback.is_none() {
                text.push_str(", ready");
            }
        }
        ServerStatus::Failed { reason } => {
            color = Color::Warning;
            if reason == EXPIRED_REASON {
                text.push_str(&format!(", {reason}"));
            } else {
                text.push_str(&format!(", failed: {reason}"));
            }
        }
        ServerStatus::BothFailed {
            intellij,
            kotlin_lsp,
        } => {
            color = Color::Warning;
            text = format!("Kotlin: no server (IntelliJ: {intellij}; kotlin-lsp: {kotlin_lsp})");
        }
    }
    if let Some(download) = &snapshot.download
        && !matches!(snapshot.status, ServerStatus::BothFailed { .. })
    {
        text.push_str(&format!(", downloading {}", download.version));
        match download.total {
            Some(total) if total > 0 => {
                text.push_str(&format!(" ({}%)", percent(download.done, total)))
            }
            _ => {}
        }
    }
    StatusLine { text, color, busy }
}

/// What a menu entry does. The view maps each to an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    SwitchServer,
    RestartServer,
    ReimportProject,
    ShowLogs,
    OpenProtocolLog,
    LicenseStatus,
    LicenseActivate,
    LicenseTrial,
    CancelInstall,
    CheckForServerUpdates,
    ReviewEula,
    /// Opens the settings file, where `kotlin.jdk_home` is set.
    SetJdkHome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Entry {
        label: String,
        command: Command,
    },
    /// A line of text that does nothing, such as the last Gradle error.
    Note(String),
    Separator,
}

fn entry(label: &str, command: Command) -> MenuItem {
    MenuItem::Entry {
        label: label.into(),
        command,
    }
}

pub fn menu_items(snapshot: &Snapshot) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if matches!(snapshot.status, ServerStatus::Installing { .. }) {
        items.push(entry("Cancel install", Command::CancelInstall));
        items.push(MenuItem::Separator);
    }
    items.push(entry("Switch server...", Command::SwitchServer));
    items.push(entry("Restart server", Command::RestartServer));
    items.push(entry("Reimport project", Command::ReimportProject));
    items.push(MenuItem::Separator);
    items.push(entry("Show logs", Command::ShowLogs));
    items.push(entry("Open protocol log", Command::OpenProtocolLog));
    items.push(entry("License status", Command::LicenseStatus));
    items.push(entry("Activate license...", Command::LicenseActivate));
    items.push(entry("Start license trial", Command::LicenseTrial));
    items.push(MenuItem::Separator);
    for line in snapshot.updates.lines() {
        items.push(MenuItem::Note(line));
    }
    items.push(entry(
        "Check for server updates",
        Command::CheckForServerUpdates,
    ));

    let failure = match &snapshot.status {
        ServerStatus::Failed { reason } => Some(reason.as_str()),
        _ => None,
    };
    // A fallback runs kotlin-lsp, but the EULA is still why IntelliJ is not running.
    let eula = (snapshot.effective == KotlinServerKind::Intellij
        && failure == Some(EULA_NOT_ACCEPTED_REASON))
        || snapshot
            .fallback
            .as_ref()
            .is_some_and(|fallback| fallback.kind == FallbackReason::EulaNotAccepted);
    let no_jdk = failure.is_some_and(|reason| reason.starts_with(NO_JDK_REASON));
    let has_gradle_error = snapshot.gradle_error.is_some();
    if eula || no_jdk || has_gradle_error {
        items.push(MenuItem::Separator);
    }
    if eula {
        items.push(entry("Review IntelliJ EULA", Command::ReviewEula));
    }
    if no_jdk {
        items.push(entry(SET_JDK_HOME_LABEL, Command::SetJdkHome));
    }
    if has_gradle_error && let Some(error) = &snapshot.gradle_error {
        items.push(MenuItem::Note(format!("Gradle: {error}")));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(effective: KotlinServerKind, status: ServerStatus) -> Snapshot {
        Snapshot {
            effective,
            status,
            fallback: None,
            gradle_error: None,
            download: None,
            checking: false,
            updates: UpdatesInfo::Off,
        }
    }

    #[test]
    fn test_check_command_menu_labels_for_updates_off_checked_and_error() {
        let notes = |info: UpdatesInfo| {
            let mut snapshot = lsp(ServerStatus::Ready);
            snapshot.updates = info;
            let items = menu_items(&snapshot);
            assert!(items.contains(&entry(
                "Check for server updates",
                Command::CheckForServerUpdates
            )));
            items
                .into_iter()
                .filter_map(|item| match item {
                    MenuItem::Note(text) => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(notes(UpdatesInfo::Off), ["Server updates: off"]);
        assert_eq!(
            notes(UpdatesInfo::NotChecked),
            ["Server updates: not checked yet"]
        );
        let checked = |error: Option<&str>| UpdatesInfo::Checked {
            when: "2026-11-12 09:30".into(),
            serial: Some(7),
            error: error.map(str::to_string),
        };
        assert_eq!(
            notes(checked(None)),
            ["Server updates: checked 2026-11-12 09:30, serial 7"]
        );
        assert_eq!(
            notes(checked(Some("rejected: bad signature"))),
            [
                "Server updates: checked 2026-11-12 09:30, serial 7",
                "Last error: rejected: bad signature"
            ]
        );
    }

    fn lsp(status: ServerStatus) -> Snapshot {
        snapshot(KotlinServerKind::KotlinLsp, status)
    }

    fn intellij(status: ServerStatus) -> Snapshot {
        snapshot(KotlinServerKind::Intellij, status)
    }

    fn failed(reason: &str) -> ServerStatus {
        ServerStatus::Failed {
            reason: reason.into(),
        }
    }

    fn mb(count: u64) -> u64 {
        count * 1024 * 1024
    }

    fn line(snapshot: &Snapshot) -> (String, Color, bool) {
        let line = status_line(snapshot);
        (line.text, line.color, line.busy)
    }

    fn labels(items: &[MenuItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Entry { label, .. } => Some(label.as_str()),
                MenuItem::Note(text) => Some(text.as_str()),
                MenuItem::Separator => None,
            })
            .collect()
    }

    const BASE: [&str; 8] = [
        "Switch server...",
        "Restart server",
        "Reimport project",
        "Show logs",
        "Open protocol log",
        "License status",
        "Activate license...",
        "Start license trial",
    ];

    #[test]
    fn test_installing_shows_percent_and_megabytes() {
        let status = ServerStatus::Installing {
            done: mb(52),
            total: Some(mb(137)),
        };
        assert_eq!(
            line(&lsp(status)),
            (
                "Kotlin: kotlin-lsp, installing 38% (52/137 MB)".into(),
                Color::Default,
                true
            )
        );
        let status = ServerStatus::Installing {
            done: mb(137),
            total: Some(mb(137)),
        };
        assert_eq!(
            status_line(&lsp(status)).text,
            "Kotlin: kotlin-lsp, installing 100% (137/137 MB)"
        );
    }

    #[test]
    fn test_installing_without_a_total_or_a_byte() {
        let status = ServerStatus::Installing {
            done: mb(7),
            total: None,
        };
        assert_eq!(
            status_line(&intellij(status)).text,
            "Kotlin: IntelliJ, installing 7 MB"
        );
        let status = ServerStatus::Installing {
            done: 0,
            total: None,
        };
        assert_eq!(
            status_line(&intellij(status)).text,
            "Kotlin: IntelliJ, installing"
        );
        let status = ServerStatus::Installing {
            done: 9,
            total: Some(0),
        };
        assert_eq!(
            status_line(&intellij(status)).text,
            "Kotlin: IntelliJ, installing"
        );
    }

    #[test]
    fn test_starting_indexing_and_ready() {
        assert_eq!(
            line(&intellij(ServerStatus::Starting)),
            ("Kotlin: IntelliJ, starting".into(), Color::Default, true)
        );
        assert_eq!(
            line(&intellij(ServerStatus::Indexing {
                label: "12/57 modules".into()
            })),
            (
                "Kotlin: IntelliJ, indexing 12/57 modules".into(),
                Color::Default,
                true
            )
        );
        assert_eq!(
            line(&lsp(ServerStatus::Ready)),
            ("Kotlin: kotlin-lsp, ready".into(), Color::Default, false)
        );
    }

    #[test]
    fn test_failures_are_warnings() {
        assert_eq!(
            line(&intellij(failed("IntelliJ unlicensed"))),
            (
                "Kotlin: IntelliJ, failed: IntelliJ unlicensed".into(),
                Color::Warning,
                false
            )
        );
        assert_eq!(
            line(&lsp(failed(EXPIRED_REASON))),
            (
                "Kotlin: kotlin-lsp, server expired, update bongos".into(),
                Color::Warning,
                false
            )
        );
        assert_eq!(
            line(&lsp(ServerStatus::BothFailed {
                intellij: "not installed".into(),
                kotlin_lsp: "install failed".into()
            })),
            (
                "Kotlin: no server (IntelliJ: not installed; kotlin-lsp: install failed)".into(),
                Color::Warning,
                false
            )
        );
    }

    #[test]
    fn test_a_fallback_is_named_and_warns() {
        let mut snapshot = lsp(ServerStatus::Ready);
        snapshot.fallback = Some(Fallback::new(FallbackReason::Unlicensed));
        assert_eq!(
            line(&snapshot),
            (
                "Kotlin: kotlin-lsp (fallback: IntelliJ unlicensed)".into(),
                Color::Warning,
                false
            )
        );
        snapshot.status = ServerStatus::Starting;
        assert_eq!(
            line(&snapshot),
            (
                "Kotlin: kotlin-lsp (fallback: IntelliJ unlicensed), starting".into(),
                Color::Warning,
                true
            )
        );
    }

    #[test]
    fn test_the_base_menu_has_the_eight_entries_in_order() {
        for status in [
            ServerStatus::Starting,
            ServerStatus::Ready,
            ServerStatus::Indexing { label: "x".into() },
            failed("something"),
        ] {
            assert_eq!(labels(&menu_items(&lsp(status))), BASE);
        }
    }

    #[test]
    fn test_cancel_install_is_first_and_only_while_installing() {
        let installing = lsp(ServerStatus::Installing {
            done: 1,
            total: None,
        });
        let items = menu_items(&installing);
        assert_eq!(
            items[0],
            MenuItem::Entry {
                label: "Cancel install".into(),
                command: Command::CancelInstall
            }
        );
        assert_eq!(labels(&items)[1..], BASE);
        assert!(!labels(&menu_items(&lsp(ServerStatus::Ready))).contains(&"Cancel install"));
    }

    #[test]
    fn test_unmet_toolchain_offers_set_jdk_home() {
        let snapshot = lsp(failed("no JDK for Gradle import (toolchain 21)"));
        let items = menu_items(&snapshot);
        assert!(items.contains(&MenuItem::Entry {
            label: "Set kotlin.jdk_home".into(),
            command: Command::SetJdkHome
        }));
        assert!(
            !menu_items(&lsp(failed("something else")))
                .iter()
                .any(|item| matches!(
                    item,
                    MenuItem::Entry {
                        command: Command::SetJdkHome,
                        ..
                    }
                ))
        );
    }

    #[test]
    fn test_eula_review_is_offered_for_intellij_only() {
        let eula = failed(EULA_NOT_ACCEPTED_REASON);
        assert!(labels(&menu_items(&intellij(eula.clone()))).contains(&"Review IntelliJ EULA"));
        assert!(!labels(&menu_items(&lsp(eula))).contains(&"Review IntelliJ EULA"));
        let mut fallback = lsp(ServerStatus::Ready);
        fallback.fallback = Some(Fallback::new(FallbackReason::EulaNotAccepted));
        // The effective server is kotlin-lsp, but the EULA is why IntelliJ is not running.
        assert!(labels(&menu_items(&fallback)).contains(&"Review IntelliJ EULA"));
        fallback.fallback = Some(Fallback::new(FallbackReason::Unlicensed));
        assert!(!labels(&menu_items(&fallback)).contains(&"Review IntelliJ EULA"));
    }

    #[test]
    fn test_the_last_gradle_error_shows_whenever_it_is_set() {
        let mut snapshot = lsp(failed("server exited unexpectedly (exit status: 1)"));
        snapshot.gradle_error = Some("Gradle import failed: Could not resolve x".into());
        assert_eq!(
            menu_items(&snapshot).last(),
            Some(&MenuItem::Note(
                "Gradle: Gradle import failed: Could not resolve x".into()
            ))
        );
        snapshot.status = ServerStatus::Ready;
        assert!(
            menu_items(&snapshot)
                .iter()
                .any(|item| matches!(item, MenuItem::Note(_)))
        );
        snapshot.gradle_error = None;
        assert!(
            !menu_items(&snapshot)
                .iter()
                .any(|item| matches!(item, MenuItem::Note(_)))
        );
    }

    #[test]
    fn test_a_background_download_appends_its_progress() {
        let mut snapshot = lsp(ServerStatus::Ready);
        snapshot.download = Some(DownloadProgress {
            version: "263.3.0".into(),
            done: 50,
            total: Some(200),
        });
        assert_eq!(
            status_line(&snapshot).text,
            "Kotlin: kotlin-lsp, ready, downloading 263.3.0 (25%)"
        );
        snapshot.download.as_mut().unwrap().total = None;
        assert_eq!(
            status_line(&snapshot).text,
            "Kotlin: kotlin-lsp, ready, downloading 263.3.0"
        );
        snapshot.download = None;
        assert_eq!(status_line(&snapshot).text, "Kotlin: kotlin-lsp, ready");
    }

    /// A worktree stopped by an expired build shows the check and the download without saying
    /// "expired", and the expired text again once neither runs.
    #[test]
    fn test_an_expired_worktree_shows_its_refresh_and_download() {
        let expired = || {
            lsp(ServerStatus::Failed {
                reason: EXPIRED_REASON.into(),
            })
        };
        let text = |snapshot: &Snapshot| status_line(snapshot).text;
        let mut snapshot = expired();
        let idle = text(&snapshot);
        assert_eq!(idle, "Kotlin: kotlin-lsp, server expired, update bongos");

        snapshot.checking = true;
        assert_eq!(
            text(&snapshot),
            "Kotlin: checking for a newer Kotlin server"
        );

        snapshot.checking = false;
        snapshot.download = Some(DownloadProgress {
            version: "263.3.0".into(),
            done: 25,
            total: Some(100),
        });
        assert_eq!(
            text(&snapshot),
            "Kotlin: downloading kotlin-lsp 263.3.0 (25%)"
        );
        assert!(!text(&snapshot).to_lowercase().contains("expired"));

        snapshot.download = None;
        assert_eq!(text(&snapshot), idle);
        // A running server shows the check or download the usual way.
        let mut ready = lsp(ServerStatus::Ready);
        ready.checking = true;
        assert_eq!(text(&ready), "Kotlin: kotlin-lsp, ready");
    }
}
