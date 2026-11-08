//! What the status item shows and offers, as pure functions of a worktree's manager state. The view
//! (`status_item.rs`) only draws these values, so every state is tested without a window.

use settings::KotlinServerKind;
use ui::Color;

use crate::{
    intellij::EULA_NOT_ACCEPTED_REASON,
    jdk::{NO_JDK_REASON, SET_JDK_HOME_LABEL},
    manager::{EXPIRED_REASON, Fallback, FallbackReason, KotlinWorktreeState, ServerStatus},
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
}

impl Snapshot {
    /// `None` while no server has started for the worktree: the item is hidden.
    pub fn of(state: &KotlinWorktreeState) -> Option<Self> {
        Some(Self {
            effective: state.effective,
            status: state.status.clone()?,
            fallback: state.fallback.clone(),
            gradle_error: state.gradle_error.clone(),
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

pub fn status_line(snapshot: &Snapshot) -> StatusLine {
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
                    ((done.saturating_mul(100) + total / 2) / total).min(100),
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
        }
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
}
