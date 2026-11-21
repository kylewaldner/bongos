//! The build status line: a status bar item that shows what a language extension install is
//! doing (resolving, cloning, checking, building, loading), the first line of why one failed with
//! "Show log" and "Dismiss", and "<Name> installed" for five seconds after one succeeds. It
//! renders from the installer's state and never blocks. The builder reports no finer progress,
//! so the line is indeterminate: a phase and the elapsed time.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use bongos_extensions_model::LanguageList;
use editor::Editor;
use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, TaskExt as _, WeakEntity, Window,
};
use settings::Settings as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, Clickable as _, Color, Label, LabelCommon as _,
    LabelSize, h_flex,
};
use workspace::{ItemHandle, OpenOptions, StatusItemView, Workspace};

use crate::LanguageExtensionsSettings;
use crate::installer::{Failure, Installed, Installer, Operation, RequestKey};

/// How long "<Name> installed" stays.
pub const SUCCESS_VISIBLE: Duration = Duration::from_secs(5);

/// What the line says and which buttons go with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusLine {
    Working(String),
    Failed(String),
    Installed(String),
}

impl StatusLine {
    pub fn text(&self) -> &str {
        match self {
            StatusLine::Working(text) | StatusLine::Failed(text) | StatusLine::Installed(text) => {
                text
            }
        }
    }
}

/// The name shown for an extension: the list name for its id, else the id, else the last part of
/// the repository URL.
pub fn display_name(id: Option<&str>, key: &RequestKey, list: &LanguageList) -> String {
    if let Some(id) = id {
        return list
            .entries()
            .iter()
            .find(|entry| entry.id == id)
            .map_or_else(|| id.to_string(), |entry| entry.name.clone());
    }
    let url = key.url.as_str().trim_end_matches('/');
    let last = url.rsplit(['/', ':']).next().unwrap_or(url);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// "7s", "1m 05s", "1h 02m".
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}

/// The text of one running operation, "Swift extension: building (12s)".
pub fn operation_text(operation: &Operation, list: &LanguageList, now: SystemTime) -> String {
    let name = display_name(operation.id.as_deref(), &operation.key, list);
    let elapsed = now.duration_since(operation.started).unwrap_or_default();
    format!(
        "{name} extension: {} ({})",
        operation.phase.label(),
        format_elapsed(elapsed)
    )
}

/// "<Name> extension failed to build: <first line of the cause>".
pub fn failure_text(failure: &Failure, list: &LanguageList) -> String {
    let name = display_name(failure.id.as_deref(), &failure.key, list);
    let cause = failure.message.lines().next().unwrap_or_default();
    format!("{name} extension failed to build: {cause}")
}

/// Whether "<Name> installed" is still shown at `now`.
pub fn success_visible(installed_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(installed_at)
        .is_ok_and(|age| age < SUCCESS_VISIBLE)
}

/// The line for the installer's state, or `None` when nothing is shown. Running operations come
/// first (the oldest, with "(+N more)" for the rest), then a failure, then a recent success.
pub fn status_line(
    operations: &[Operation],
    failure: Option<&Failure>,
    installed: Option<(&Installed, SystemTime)>,
    list: &LanguageList,
    now: SystemTime,
) -> Option<StatusLine> {
    if let Some(first) = operations.first() {
        let mut text = operation_text(first, list, now);
        if operations.len() > 1 {
            text.push_str(&format!(" (+{} more)", operations.len() - 1));
        }
        return Some(StatusLine::Working(text));
    }
    if let Some(failure) = failure {
        return Some(StatusLine::Failed(failure_text(failure, list)));
    }
    let (installed, at) = installed?;
    if !success_visible(at, now) {
        return None;
    }
    let key_name = list
        .entries()
        .iter()
        .find(|entry| entry.id == installed.id)
        .map_or_else(|| installed.id.clone(), |entry| entry.name.clone());
    Some(StatusLine::Installed(format!("{key_name} installed")))
}

/// Opens `path` in the workspace as a read-only buffer.
pub fn open_read_only(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let task = workspace.open_abs_path(path, OpenOptions::default(), window, cx);
    cx.spawn_in(window, async move |_, cx| -> Result<()> {
        let item = task.await?;
        cx.update(|_, cx| {
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update(cx, |editor, _| editor.set_read_only(true));
            }
        })?;
        Ok(())
    })
}

/// The status bar item of the build status line.
pub struct ExtensionBuildStatus {
    workspace: WeakEntity<Workspace>,
    installer: Option<Entity<Installer>>,
    /// The finish time a clearing timer was already started for.
    timed: Option<SystemTime>,
    _observation: Option<Subscription>,
    _clear: Option<Task<()>>,
}

impl ExtensionBuildStatus {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let installer = Installer::try_global(cx);
        let observation = installer
            .as_ref()
            .map(|installer| cx.observe(installer, |this, _, cx| this.installer_changed(cx)));
        Self {
            workspace,
            installer,
            timed: None,
            _observation: observation,
            _clear: None,
        }
    }

    /// Starts the timer that clears a fresh success once, and repaints.
    fn installer_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(installer) = &self.installer {
            let at = installer.read(cx).last_installed_at();
            if at.is_some() && at != self.timed {
                self.timed = at;
                self._clear = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SUCCESS_VISIBLE).await;
                    this.update(cx, |_, cx| cx.notify()).ok();
                }));
            }
        }
        cx.notify();
    }

    /// The line to show, or `None` when hidden.
    pub fn line(&self, cx: &App) -> Option<StatusLine> {
        if !LanguageExtensionsSettings::get_global(cx).enabled {
            return None;
        }
        let installer = self.installer.as_ref()?.read(cx);
        let operations: Vec<Operation> = installer.operations().cloned().collect();
        let installed = installer
            .last_installed()
            .zip(installer.last_installed_at());
        status_line(
            &operations,
            installer.last_failure(),
            installed,
            installer.list(),
            installer.clock().now(),
        )
    }

    /// The build log path of the failure on display.
    pub fn failure_log(&self, cx: &App) -> Option<PathBuf> {
        let installer = self.installer.as_ref()?.read(cx);
        installer.last_failure()?.log_path.clone()
    }

    pub fn show_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.failure_log(cx) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                open_read_only(workspace, path, window, cx).detach_and_log_err(cx)
            })
            .ok();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(installer) = &self.installer {
            installer.update(cx, |installer, cx| installer.dismiss_failure(cx));
        }
    }
}

impl Render for ExtensionBuildStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(line) = self.line(cx) else {
            return h_flex().hidden().into_any_element();
        };
        let color = match line {
            StatusLine::Failed(_) => Color::Error,
            _ => Color::Muted,
        };
        let text: SharedString = line.text().to_string().into();
        let mut row = h_flex()
            .gap_1()
            .child(Label::new(text).size(LabelSize::Small).color(color));
        if matches!(line, StatusLine::Failed(_)) {
            if self.failure_log(cx).is_some() {
                row = row.child(
                    Button::new("extension-build-show-log", "Show log")
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(|this, _, window, cx| this.show_log(window, cx))),
                );
            }
            row = row.child(
                Button::new("extension-build-dismiss", "Dismiss")
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            );
        }
        row.into_any_element()
    }
}

impl StatusItemView for ExtensionBuildStatus {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only while `language_extensions.enabled` is true.
        None
    }
}

#[cfg(test)]
#[path = "status_item_tests.rs"]
mod tests;
