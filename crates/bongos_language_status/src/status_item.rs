//! The status bar item. It follows its project's language server events into a `StatusState` and
//! the global notices, and renders the state's line.

use anyhow::Result;
use collections::HashSet;
use editor::{Editor, actions::MoveToEnd};
use gpui::{
    Action as _, Anchor, App, Context, Entity, IntoElement, Render, Subscription, TaskExt as _,
    WeakEntity, Window, div,
};
use language::{BinaryStatus, LanguageName};
use lsp::{LanguageServerName, LanguageServerSelector};
use project::{LspStoreEvent, Project};
use settings::WorktreeId;
use ui::{
    Button, Color, ContextMenu, ContextMenuEntry, IconButton, IconName, IconSize, LabelSize,
    ParentElement as _, PopoverMenu, PopoverMenuHandle, Styled as _, Tooltip, prelude::*,
};
use util::truncate_and_trailoff;
use workspace::{OpenOptions, StatusItemView, Workspace};

use crate::{
    CancelDownload, LanguageNotices, NoticeList, RetryServer, ShowLogs,
    notices::LogGlobal,
    state::{
        self, Command, Kind, Line, Notice, StatusState, Tone, cause_without_stderr, is_ignored,
    },
};

/// Zed's language server log viewer, which has the protocol messages.
const PROTOCOL_LOG_ACTION: &str = "dev::OpenLanguageServerLogs";

const MAX_TEXT_CHARS: usize = 100;

pub struct LanguageStatusItem {
    project: WeakEntity<Project>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    state: StatusState,
    notices: Entity<NoticeList>,
    /// The stderr of each server's last start, logged if that start failed.
    startup_stderr: Vec<(LanguageServerName, String)>,
    /// The language of the active buffer, which names a server that serves several languages.
    active_language: Option<LanguageName>,
    _subscriptions: Vec<Subscription>,
}

impl LanguageStatusItem {
    pub fn new(
        project: Entity<Project>,
        menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> Self {
        let notices = LanguageNotices::list(cx);
        let lsp_store = project.read(cx).lsp_store();
        let subscriptions = vec![
            cx.subscribe(&lsp_store, |this, _, event, cx| {
                this.on_lsp_event(event, cx)
            }),
            cx.observe(&notices, |_, _, cx| cx.notify()),
        ];
        Self {
            project: project.downgrade(),
            menu_handle,
            state: StatusState::default(),
            notices,
            startup_stderr: Vec::new(),
            active_language: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn state(&self) -> &StatusState {
        &self.state
    }

    /// The line this item shows now.
    pub fn line(&self, cx: &App) -> Option<Line> {
        state::status_line(&self.state, &self.visible_notices(cx))
    }

    /// The notices of worktrees this item's project has.
    fn visible_notices(&self, cx: &App) -> Vec<Notice> {
        let Some(project) = self.project.upgrade() else {
            return Vec::new();
        };
        let project = project.read(cx);
        self.notices
            .read(cx)
            .notices()
            .iter()
            .filter(|notice| project.worktree_for_id(notice.worktree_id, cx).is_some())
            .cloned()
            .collect()
    }

    fn language_of(&self, server: &LanguageServerName, cx: &App) -> String {
        match self.project.upgrade() {
            Some(project) => state::language_for_server(
                project.read(cx).languages(),
                server,
                self.active_language.as_ref(),
            ),
            None => server.to_string(),
        }
    }

    fn write_log(&self, text: &str, cx: &App) {
        if let Some(log) = cx.try_global::<LogGlobal>() {
            log.0.line(text);
        }
    }

    fn on_lsp_event(&mut self, event: &LspStoreEvent, cx: &mut Context<Self>) {
        match event {
            LspStoreEvent::LanguageServerUpdate {
                name: Some(name),
                message: proto::update_language_server::Variant::StatusUpdate(update),
                ..
            } => {
                let Some(proto::status_update::Status::Binary(status)) = &update.status else {
                    return;
                };
                let Some(status) = proto::ServerBinaryStatus::from_i32(*status) else {
                    return;
                };
                let status = match status {
                    proto::ServerBinaryStatus::Failed => BinaryStatus::Failed {
                        error: update.message.clone().unwrap_or_default(),
                    },
                    proto::ServerBinaryStatus::Downloading => BinaryStatus::Downloading,
                    proto::ServerBinaryStatus::Stopped | proto::ServerBinaryStatus::Stopping => {
                        BinaryStatus::Stopped
                    }
                    proto::ServerBinaryStatus::None
                    | proto::ServerBinaryStatus::CheckingForUpdate
                    | proto::ServerBinaryStatus::Starting => BinaryStatus::None,
                };
                self.on_binary_status(name, status, cx);
            }
            LspStoreEvent::LanguageServerDownloadProgress {
                name, done, total, ..
            } => {
                if is_ignored(name) {
                    return;
                }
                let language = self.language_of(name, cx);
                if let Some(line) = self.state.progress(name, &language, *done, *total) {
                    self.write_log(&line, cx);
                }
                cx.notify();
            }
            LspStoreEvent::LanguageServerStartupStderr { name, text, .. } => {
                if is_ignored(name) {
                    return;
                }
                self.startup_stderr.retain(|(server, _)| server != name);
                self.startup_stderr.push((name.clone(), text.clone()));
            }
            LspStoreEvent::LanguageServerStartFailed { name, error, .. } => {
                if is_ignored(name) {
                    return;
                }
                let language = self.language_of(name, cx);
                let mut text = format!("{language}: {name} failed to start: {error}");
                if let Some(index) = self.startup_stderr.iter().position(|(s, _)| s == name) {
                    let (_, stderr) = self.startup_stderr.remove(index);
                    for line in stderr.lines() {
                        text.push_str("\nstderr ");
                        text.push_str(line);
                    }
                }
                self.write_log(&text, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn on_binary_status(
        &mut self,
        name: &LanguageServerName,
        status: BinaryStatus,
        cx: &mut Context<Self>,
    ) {
        if is_ignored(name) {
            return;
        }
        let language = self.language_of(name, cx);
        let line = match status {
            BinaryStatus::Failed { error } => {
                // Stderr of a failed start is logged with the start failure; do not repeat it.
                self.state
                    .fail(name, &language, cause_without_stderr(&error))
                    .into()
            }
            BinaryStatus::Downloading => self.state.start_download(name, &language),
            BinaryStatus::Stopped | BinaryStatus::Stopping => {
                self.state.stopped(name);
                None
            }
            BinaryStatus::None | BinaryStatus::CheckingForUpdate | BinaryStatus::Starting => {
                self.state.succeeded(name)
            }
        };
        if let Some(line) = line {
            self.write_log(&line, cx);
        }
        cx.notify();
    }

    /// `languages: retry server`: installs and starts the first failed server again, restarting
    /// only that server.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let Some(failure) = self.state.failures.first().cloned() else {
            return;
        };
        self.write_log(
            &format!("{}: retrying {}", failure.language, failure.server),
            cx,
        );
        retry_server(&project, &failure.server, cx);
    }

    /// `languages: cancel download`: cancels every download this item shows. A download that
    /// ended already is not an error.
    pub fn cancel_downloads(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let lsp_store = project.read(cx).lsp_store();
        let servers: Vec<_> = self
            .state
            .downloads
            .iter()
            .map(|download| (download.server.clone(), download.language.clone()))
            .collect();
        for (server, language) in servers {
            let cancelled = lsp_store.read(cx).cancel_server_download(&server);
            let note = if cancelled {
                format!("{language}: cancelling the download of {server}")
            } else {
                format!("{language}: the download of {server} had ended already")
            };
            self.write_log(&note, cx);
        }
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.line(cx) else {
            return;
        };
        if line.kind == Kind::Notice {
            if let Some(notice) = self.visible_notices(cx).first().cloned() {
                self.notices
                    .update(cx, |notices, cx| notices.dismiss(&notice, cx));
            }
        } else {
            self.state.dismiss(&line);
        }
        cx.notify();
    }
}

/// Restarts `server` for the project's buffers whose language it serves, and no other server.
pub fn retry_server(project: &Entity<Project>, server: &LanguageServerName, cx: &mut App) {
    let registry = project.read(cx).languages().clone();
    let serving = state::languages_serving(&registry, server);
    let buffers: Vec<_> = project
        .read(cx)
        .opened_buffers(cx)
        .into_iter()
        .filter(|buffer| {
            buffer
                .read(cx)
                .language()
                .is_some_and(|language| serving.contains(&language.name()))
        })
        .collect();
    if buffers.is_empty() {
        return;
    }
    let mut only = HashSet::default();
    only.insert(LanguageServerSelector::Name(server.clone()));
    project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
        lsp_store.restart_language_servers_for_buffers(buffers, only, true, cx);
    });
}

impl StatusItemView for LanguageStatusItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_language = active_pane_item
            .and_then(|item| item.project_path(cx))
            .zip(self.project.upgrade())
            .and_then(|(path, project)| project.read(cx).get_open_buffer(&path, cx))
            .and_then(|buffer| buffer.read(cx).language().map(|language| language.name()));
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

impl Render for LanguageStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(line) = self.line(cx) else {
            return div().hidden().into_any_element();
        };
        let color = match line.tone {
            Tone::Warning => Color::Warning,
            Tone::Normal => Color::Default,
        };
        let this = cx.weak_entity();
        div()
            .flex()
            .items_center()
            .child(
                Button::new(
                    "language-status",
                    truncate_and_trailoff(&line.text, MAX_TEXT_CHARS),
                )
                .label_size(LabelSize::Small)
                .color(color)
                .loading(line.kind == Kind::Downloading)
                .tooltip(Tooltip::text(line.text.clone()))
                .on_click(|_, window, cx| {
                    window.dispatch_action(ShowLogs.boxed_clone(), cx);
                }),
            )
            .child(
                PopoverMenu::new("language-status-menu")
                    .menu(move |window, cx| {
                        let (state, line) = this
                            .read_with(cx, |this, cx| (this.state.clone(), this.line(cx)))
                            .ok()?;
                        Some(build_menu(&state, &line?, this.clone(), window, cx))
                    })
                    .anchor(Anchor::BottomLeft)
                    .with_handle(self.menu_handle.clone())
                    .trigger_with_tooltip(
                        IconButton::new("language-status-menu-button", IconName::ChevronDown)
                            .icon_size(IconSize::XSmall),
                        Tooltip::text("Language status menu"),
                    ),
            )
            .into_any_element()
    }
}

fn action_for(command: Command, cx: &App) -> Option<Box<dyn gpui::Action>> {
    match command {
        Command::ShowLogs => Some(ShowLogs.boxed_clone()),
        Command::Retry => Some(RetryServer.boxed_clone()),
        Command::OpenProtocolLog => cx.build_action(PROTOCOL_LOG_ACTION, None).ok(),
        Command::CancelDownload => Some(CancelDownload.boxed_clone()),
        // Dismiss has no action of its own: the item handles it when the entry is clicked.
        Command::Dismiss => None,
    }
}

fn label(command: Command) -> &'static str {
    match command {
        Command::ShowLogs => "Show logs",
        Command::Retry => "Retry",
        Command::OpenProtocolLog => "Open protocol log",
        Command::CancelDownload => "Cancel download",
        Command::Dismiss => "Dismiss",
    }
}

fn build_menu(
    state: &StatusState,
    line: &Line,
    this: WeakEntity<LanguageStatusItem>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let commands = state::menu_commands(state, line);
    ContextMenu::build(window, cx, move |mut menu, _, cx| {
        for command in &commands {
            menu = match (command, action_for(*command, cx)) {
                (_, Some(action)) => menu.action(label(*command), action),
                (Command::Dismiss, None) => {
                    let this = this.clone();
                    menu.entry(label(*command), None, move |_, cx| {
                        this.update(cx, |this, cx| this.dismiss(cx)).ok();
                    })
                }
                (_, None) => menu.item(ContextMenuEntry::new(label(*command)).disabled(true)),
            };
        }
        menu
    })
}

/// `languages: show logs`: opens `languages.log` read-only, at its end.
pub fn show_logs(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(log) = cx.try_global::<LogGlobal>().map(|global| global.0.clone()) else {
        return;
    };
    if let Err(error) = log.ensure_exists() {
        ::log::error!("cannot create the languages log: {error}");
        return;
    }
    let task =
        workspace.open_abs_path(log.path().to_path_buf(), OpenOptions::default(), window, cx);
    cx.spawn_in(window, async move |_, cx| -> Result<()> {
        let item = task.await?;
        cx.update(|window, cx| {
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update(cx, |editor, cx| {
                    editor.set_read_only(true);
                    editor.move_to_end(&MoveToEnd, window, cx);
                });
            }
        })
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use chrono::{TimeZone as _, Utc};
    use fs::{FakeFs, Fs as _};
    use futures::StreamExt as _;
    use gpui::{AppContext as _, TestAppContext};
    use language::{FakeLspAdapter, LspAdapterDelegate as _, rust_lang};
    use lsp::LanguageServerName;
    use project::lsp_store::LocalLspAdapterDelegate;
    use serde_json::json;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    use super::*;
    use crate::{init_with_clock, state::Kind};

    fn init_test(cx: &mut TestAppContext, logs_dir: &Path) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            let clock: crate::Clock =
                Arc::new(|| Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap());
            init_with_clock(logs_dir, clock, cx);
        });
    }

    fn name(name: &'static str) -> LanguageServerName {
        LanguageServerName::new_static(name)
    }

    fn read_log(logs_dir: &Path) -> String {
        std::fs::read_to_string(logs_dir.join("languages.log")).unwrap_or_default()
    }

    async fn project_and_item(
        cx: &mut TestAppContext,
        fake_rust_server: bool,
    ) -> (Entity<Project>, Entity<LanguageStatusItem>) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({ "a.rs": "fn main() {}", "b.rs": "fn b() {}" }),
        )
        .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(rust_lang());
        if fake_rust_server {
            registry.register_fake_lsp_adapter(
                "Rust",
                FakeLspAdapter {
                    name: "fake-rust-server",
                    ..Default::default()
                },
            );
        }
        let item =
            cx.new(|cx| LanguageStatusItem::new(project.clone(), PopoverMenuHandle::default(), cx));
        (project, item)
    }

    fn text(item: &Entity<LanguageStatusItem>, cx: &mut TestAppContext) -> Option<String> {
        item.read_with(cx, |item, cx| item.line(cx).map(|line| line.text))
    }

    #[gpui::test]
    async fn test_failure_download_and_success_follow_the_binary_status(cx: &mut TestAppContext) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, true).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        let server = name("fake-rust-server");

        assert_eq!(text(&item, cx), None);
        registry.update_lsp_binary_status(server.clone(), BinaryStatus::Downloading);
        cx.run_until_parked();
        assert_eq!(
            text(&item, cx).unwrap(),
            "Rust: installing fake-rust-server"
        );

        registry.update_lsp_binary_status(
            server.clone(),
            BinaryStatus::Failed {
                error: "could not download fake-rust-server\ncaused by: no network".into(),
            },
        );
        cx.run_until_parked();
        let line = item.read_with(cx, |item, cx| item.line(cx)).unwrap();
        assert_eq!(
            line.text,
            "Rust server unavailable: could not download fake-rust-server"
        );
        assert_eq!(line.kind, Kind::Unavailable);
        assert_eq!(line.tone, Tone::Warning);

        // A new attempt and its success clear the line.
        registry.update_lsp_binary_status(server.clone(), BinaryStatus::Downloading);
        registry.update_lsp_binary_status(server.clone(), BinaryStatus::Starting);
        cx.run_until_parked();
        assert_eq!(text(&item, cx), None);

        let log = read_log(logs.path());
        assert!(log.contains("2026-03-04T05:06:07.000Z Rust: downloading fake-rust-server"));
        assert!(log.contains("caused by: no network"), "{log}");
        assert!(log.contains("Rust: installed fake-rust-server"), "{log}");
    }

    #[gpui::test]
    async fn test_the_kotlin_server_is_ignored(cx: &mut TestAppContext) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, true).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.update_lsp_binary_status(
            name("kotlin"),
            BinaryStatus::Failed {
                error: "no jdk".into(),
            },
        );
        registry.update_lsp_binary_status(name("kotlin"), BinaryStatus::Downloading);
        cx.run_until_parked();
        assert_eq!(text(&item, cx), None);
        assert_eq!(read_log(logs.path()), "");
    }

    #[gpui::test]
    async fn test_download_progress_names_the_language_and_logs_milestones(
        cx: &mut TestAppContext,
    ) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, true).await;
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        let delegate = lsp_store.update(cx, |lsp_store, cx| {
            let local = lsp_store.as_local().unwrap();
            let worktree = local.worktree_store.read(cx).worktrees().next().unwrap();
            LocalLspAdapterDelegate::from_local_lsp(local, &worktree, cx)
        });
        let server = name("fake-rust-server");
        const MB: u64 = 1024 * 1024;
        delegate.report_download_progress(server.clone(), 6 * MB, Some(16 * MB));
        cx.run_until_parked();
        assert_eq!(
            text(&item, cx).unwrap(),
            "Rust: downloading fake-rust-server 38% (6/16 MB)"
        );
        delegate.report_download_progress(server.clone(), 7 * MB, Some(16 * MB));
        delegate.report_download_progress(server.clone(), 8 * MB, Some(16 * MB));
        cx.run_until_parked();
        let log = read_log(logs.path());
        assert_eq!(log.matches("download 25%").count(), 1, "{log}");
        assert_eq!(log.matches("download 50%").count(), 1, "{log}");
    }

    #[gpui::test]
    async fn test_cancel_sets_the_flag_and_tolerates_an_ended_download(cx: &mut TestAppContext) {
        use std::sync::atomic::Ordering;

        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, true).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        let delegate = lsp_store.update(cx, |lsp_store, cx| {
            let local = lsp_store.as_local().unwrap();
            let worktree = local.worktree_store.read(cx).worktrees().next().unwrap();
            LocalLspAdapterDelegate::from_local_lsp(local, &worktree, cx)
        });
        let server = name("fake-rust-server");
        registry.update_lsp_binary_status(server.clone(), BinaryStatus::Downloading);
        cx.run_until_parked();

        let flag = delegate.download_cancel_flag(server.clone());
        item.update(cx, |item, cx| item.cancel_downloads(cx));
        assert!(flag.load(Ordering::SeqCst));
        assert!(read_log(logs.path()).contains("cancelling the download of fake-rust-server"));

        // The download ended: nothing to cancel, and no error.
        drop(flag);
        item.update(cx, |item, cx| item.cancel_downloads(cx));
        assert!(read_log(logs.path()).contains("had ended already"));
    }

    #[gpui::test]
    async fn test_retry_restarts_only_the_failed_server(cx: &mut TestAppContext) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, false).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        let mut servers_a = registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                name: "server-a",
                ..Default::default()
            },
        );
        let mut servers_b = registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                name: "server-b",
                ..Default::default()
            },
        );
        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
            })
            .await
            .unwrap();
        let first_a = servers_a.next().await.unwrap();
        let first_b = servers_b.next().await.unwrap();
        cx.run_until_parked();

        registry.update_lsp_binary_status(
            name("server-a"),
            BinaryStatus::Failed {
                error: "boom".into(),
            },
        );
        cx.run_until_parked();
        item.update(cx, |item, cx| item.retry(cx));
        cx.run_until_parked();

        let second_a = servers_a.next().await.unwrap();
        assert_ne!(first_a.server.server_id(), second_a.server.server_id());
        assert!(
            servers_b.try_next().is_err(),
            "server-b must not start again"
        );
        assert!(read_log(logs.path()).contains("retrying server-a"));
        drop(first_b);
    }

    #[gpui::test]
    async fn test_notices_report_show_and_clear(cx: &mut TestAppContext) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, item) = project_and_item(cx, true).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let message =
            "no terraform or tofu on PATH; provider schemas and formatting are unavailable";
        cx.update(|cx| {
            LanguageNotices::report(worktree_id, "Terraform", "tool", message, cx);
            LanguageNotices::report(worktree_id, "Terraform", "tool", message, cx);
        });
        cx.run_until_parked();
        assert_eq!(text(&item, cx).unwrap(), format!("Terraform: {message}"));
        assert_eq!(read_log(logs.path()).matches("notice:").count(), 1);

        // A notice of a worktree this project does not have stays out of its line.
        cx.update(|cx| {
            LanguageNotices::clear(worktree_id, "Terraform", "tool", cx);
        });
        cx.run_until_parked();
        assert_eq!(text(&item, cx), None);
        assert!(read_log(logs.path()).contains("Terraform: notice cleared (tool)"));
    }

    #[gpui::test]
    async fn test_show_logs_opens_the_log_read_only_at_its_end(cx: &mut TestAppContext) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, _item) = project_and_item(cx, false).await;
        cx.update(|cx| LanguageNotices::report(WorktreeId::from_usize(9), "Go", "k", "text", cx));
        let log_path = logs.path().join("languages.log");
        // Put a line in the log (on the real disk) and mirror it into the project's fake disk.
        cx.update(|cx| cx.global::<LogGlobal>().0.line("first line\nlast line"));
        let content = std::fs::read_to_string(&log_path).unwrap();
        let fake = project.read_with(cx, |project, _| project.fs().clone());
        let fake = fake.as_fake();
        fake.create_dir(logs.path()).await.unwrap();
        fake.insert_file(&log_path, content.clone().into_bytes())
            .await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| show_logs(workspace, window, cx));
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            let editor = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("the log is open in an editor");
            editor.update(cx, |editor, cx| {
                assert!(editor.read_only(cx));
                assert_eq!(editor.text(cx), content);
                let end = editor.buffer().read(cx).snapshot(cx).max_point();
                let head = editor
                    .selections
                    .newest::<language::Point>(&editor.display_snapshot(cx))
                    .head();
                assert_eq!(head, end);
            });
        });
    }

    #[gpui::test]
    async fn test_language_is_named_by_the_buffer_that_started_a_shared_server(
        cx: &mut TestAppContext,
    ) {
        let logs = tempfile::tempdir().unwrap();
        init_test(cx, logs.path());
        let (project, _item) = project_and_item(cx, true).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Svelte".into(),
                ..Default::default()
            },
            None,
        )));
        for language in ["Rust", "Svelte"] {
            registry.register_fake_lsp_adapter(
                language,
                FakeLspAdapter {
                    name: "tailwind",
                    ..Default::default()
                },
            );
        }
        let server = name("tailwind");
        assert_eq!(state::language_for_server(&registry, &server, None), "Rust");
        assert_eq!(
            state::language_for_server(&registry, &server, Some(&"Svelte".into())),
            "Svelte"
        );
        assert_eq!(
            state::language_for_server(&registry, &name("unknown"), None),
            "unknown"
        );
    }

    #[gpui::test]
    async fn test_actions_dispatch_by_name(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for name in [
                "languages::ShowLogs",
                "languages::RetryServer",
                "languages::CancelDownload",
            ] {
                assert!(cx.build_action(name, None).is_ok(), "{name}");
            }
        });
    }
}
