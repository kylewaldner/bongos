//! The Slack status bar item and the text of `slack: status`.
//!
//! The item shows nothing while Slack is off or not connected, `Slack: <workspace>` while
//! connected, `Slack: waiting` during a rate-limit wait (its tooltip counts the seconds down,
//! repainted by a timer that runs only while waiting) and `Slack: reconnect` once Slack rejected
//! the token. A click runs `slack: status`, or `slack: connect` when a reconnect is needed.

use std::time::Duration;

use bongos_slack_model::AllowedList;
use bongos_slack_model::token::{HIDDEN, redact};
use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, Subscription, Task, Window, div,
};
use ui::{Button, ButtonCommon as _, Clickable as _, LabelSize, Tooltip, prelude::*};
use workspace::{ItemHandle, StatusItemView, Workspace};

use crate::actions::slack;
use crate::connection::{self, Connection, StoredKind};
use crate::service::{SlackService, SlackState};
use crate::token_prompt::open_text_buffer;

/// How often the item repaints while waiting.
const TICK: Duration = Duration::from_secs(1);

/// What the item shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    Nothing,
    Workspace(String),
    /// A rate-limit wait, with the time left.
    Waiting(Duration),
    Reconnect,
}

impl Shown {
    /// What the item shows now.
    pub fn of(cx: &App) -> Self {
        if !SlackService::is_enabled(cx) {
            return Self::Nothing;
        }
        let Some(service) = SlackService::global(cx) else {
            return Self::Nothing;
        };
        let service = service.read(cx);
        match service.state(cx) {
            SlackState::NotConnected => Self::Nothing,
            SlackState::Reconnect(_) => Self::Reconnect,
            SlackState::Connected(connection) => match service.wait_left() {
                Some(left) => Self::Waiting(left),
                None => Self::Workspace(connection.workspace),
            },
        }
    }

    /// The chip text, or `None` when the item shows nothing.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Nothing => None,
            Self::Workspace(workspace) => Some(format!("Slack: {workspace}")),
            Self::Waiting(_) => Some("Slack: waiting".to_string()),
            Self::Reconnect => Some("Slack: reconnect".to_string()),
        }
    }

    /// The tooltip text.
    pub fn tooltip(&self) -> String {
        match self {
            Self::Waiting(left) => format!("Slack asked to wait, {} s left", whole_seconds(*left)),
            Self::Reconnect => "Slack rejected the token: slack: connect".to_string(),
            Self::Nothing | Self::Workspace(_) => "Slack status".to_string(),
        }
    }
}

/// `left` in seconds, rounded up, so the count never shows 0 while still waiting.
fn whole_seconds(left: Duration) -> u64 {
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

/// The status bar item. One per workspace; all show the one service.
pub struct SlackStatusItem {
    /// Repaints while a wait runs; `None` otherwise.
    ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SlackStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions =
            vec![cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify())];
        if let Some(service) = SlackService::global(cx) {
            subscriptions.push(cx.observe(&service, |this, _, cx| {
                this.update_ticker(cx);
                cx.notify();
            }));
        }
        let mut item = Self {
            ticker: None,
            _subscriptions: subscriptions,
        };
        item.update_ticker(cx);
        item
    }

    /// Whether the repaint timer runs.
    pub fn is_ticking(&self) -> bool {
        self.ticker.is_some()
    }

    /// Starts the repaint timer while waiting and stops it otherwise.
    fn update_ticker(&mut self, cx: &mut Context<Self>) {
        let waiting = matches!(Shown::of(cx), Shown::Waiting(_));
        if !waiting {
            self.ticker = None;
            return;
        }
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let still_waiting = this.update(cx, |_, cx| {
                    cx.notify();
                    matches!(Shown::of(cx), Shown::Waiting(_))
                });
                if !matches!(still_waiting, Ok(true)) {
                    break;
                }
            }
            this.update(cx, |this, _| this.ticker = None).ok();
        }));
    }
}

impl Render for SlackStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = Shown::of(cx);
        let Some(text) = shown.text() else {
            return div().hidden().into_any_element();
        };
        let tooltip = shown.tooltip();
        let reconnect = shown == Shown::Reconnect;
        div()
            .child(
                Button::new("slack-status", text)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(move |_, window, cx| {
                        if reconnect {
                            window.dispatch_action(Box::new(slack::Connect), cx);
                        } else {
                            window.dispatch_action(Box::new(slack::Status), cx);
                        }
                    }),
            )
            .into_any_element()
    }
}

impl StatusItemView for SlackStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // The item is hidden by `slack.enabled`.
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

/// What `slack: status` shows.
pub struct StatusFacts<'a> {
    pub enabled: bool,
    pub state: &'a SlackState,
    /// The time left of the rate-limit wait and its local end time.
    pub wait: Option<(Duration, String)>,
    /// The id of the current project, when there is one.
    pub project: Option<&'a str>,
    pub allowed: &'a AllowedList,
}

fn kind_text(kind: StoredKind) -> &'static str {
    match kind {
        StoredKind::User => "user token",
        StoredKind::Bot => "bot token",
    }
}

fn connection_lines(connection: &Connection, out: &mut String) {
    out.push_str(&format!(
        "Workspace: {} ({})\n",
        connection.workspace, connection.workspace_url
    ));
    out.push_str(&format!(
        "User: {} ({})\n",
        connection.user, connection.user_id
    ));
    out.push_str(&format!(
        "Token: {}, {HIDDEN}\n",
        kind_text(connection.token_kind)
    ));
    let scopes = if connection.scopes.is_empty() {
        "none recorded".to_string()
    } else {
        connection.scopes.join(", ")
    };
    out.push_str(&format!("Granted scopes: {scopes}\n"));
}

/// The text of the `slack: status` buffer. It never holds the token: the token line says
/// `slack token (hidden)`, and the whole text passes through [`redact`].
pub fn status_text(facts: &StatusFacts<'_>) -> String {
    let mut out = String::from("Slack status\n\n");
    if !facts.enabled {
        out.push_str("Slack is off (slack.enabled)\n");
    }
    match facts.state {
        SlackState::NotConnected => {
            out.push_str("State: not connected (slack: connect)\n");
            return redact(&out);
        }
        SlackState::Connected(connection) => {
            out.push_str("State: connected\n");
            connection_lines(connection, &mut out);
        }
        SlackState::Reconnect(connection) => {
            out.push_str("State: Slack rejected the token; reconnect Slack (slack: connect)\n");
            connection_lines(connection, &mut out);
        }
    }
    match facts.project {
        Some(project) if facts.allowed.is_empty() => {
            out.push_str(&format!("Allowed channels of project {project}: none\n"));
        }
        Some(project) => {
            let ids: Vec<&str> = facts.allowed.iter().map(|id| id.as_str()).collect();
            out.push_str(&format!(
                "Allowed channels of project {project}: {}\n",
                ids.join(", ")
            ));
        }
        None => out.push_str("Allowed channels: no current project (focus a task)\n"),
    }
    match &facts.wait {
        Some((left, at)) => out.push_str(&format!(
            "Rate limit: Slack asked to wait, {} s left (until {at})\n",
            whole_seconds(*left)
        )),
        None => out.push_str("Rate limit: no wait\n"),
    }
    redact(&out)
}

/// The facts of `slack: status` now, for `project`.
pub fn current_status_text(project: Option<&str>, cx: &App) -> String {
    let service: Option<Entity<SlackService>> = SlackService::global(cx);
    let (state, wait) = match &service {
        Some(service) => {
            let service = service.read(cx);
            let wait = service.wait_left().zip(service.wait_ends_at());
            (service.state(cx), wait)
        }
        None => (SlackState::NotConnected, None),
    };
    let allowed = match project {
        Some(project) => crate::allowed::load_in(&connection::installed_dir(cx), project),
        None => AllowedList::default(),
    };
    status_text(&StatusFacts {
        enabled: SlackService::is_enabled(cx),
        state: &state,
        wait,
        project,
        allowed: &allowed,
    })
}

/// The id of the project of the focused task, when there is one.
fn current_project(cx: &App) -> Option<String> {
    let task_id = bongos_work::Focus::global(cx)?
        .read(cx)
        .task_id()?
        .to_string();
    let tree = bongos_work::WorkStore::global(cx)?.read(cx).snapshot()?;
    let task = tree.task_by_id(&task_id)?;
    Some(
        bongos_work::project_repo_of(task, cx)?
            .location()
            .id
            .clone(),
    )
}

/// `slack: status`: opens the status in a read-only buffer.
pub(crate) fn show_status(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = current_project(cx);
    let text = current_status_text(project.as_deref(), cx);
    open_text_buffer("Slack status".to_string(), text, workspace, window, cx);
}
