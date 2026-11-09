//! The Chats panel (design D20): the right dock panel that shows either the list of chats (see
//! [`crate::list_view`]) or one chat's terminal and prompts panes (see [`crate::session_view`]).
//! It exists only while `chats.enabled` is true: [`init_panel`] adds and removes it as the setting
//! changes, and one panel belongs to each workspace while the runtime and the records are global
//! (D25).
//!
//! The panel holds one session view per chat it has shown, so an unsaved prompt survives showing
//! another chat. Opening the panel focuses the panel, never the chat's terminal; `chat: focus
//! terminal` does that. The view shown (the list, or a chat) is saved under the `view` key, and a
//! new panel shows it again once the index lists the chat (see [`crate::view_state`]).

use std::sync::Arc;

use bongos_work::WorkStore;
use bongos_work_model::layout;
use collections::HashMap;
use editor::Editor;
use fs::{CreateOptions, Fs};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, WeakEntity, Window, px,
};
use project::Project;
use settings::{Settings as _, SettingsStore};
use ui::{IconName, prelude::*, v_flex};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use crate::actions::chat;
use crate::list_view::ChatListView;
use crate::send::show_toast;
use crate::session_view::{SessionView, SessionViewEvent};
use crate::view_state::{self, LIST_VIEW, VIEW_KEY};
use crate::{ChatRuntime, ChatStore, ChatStoreEvent, ChatsSettings, SendError};

const PANEL_KEY: &str = "ChatPanel";

/// What the panel says while no chat is shown.
pub(crate) const NO_CHAT: &str = "No chat is shown";

/// The panel. One per workspace; every one shows the same global runtime and records.
pub struct ChatPanel {
    workspace: WeakEntity<Workspace>,
    /// The workspace's project, kept so a chat can be shown while the workspace is being updated
    /// (by a command).
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    list: Entity<ChatListView>,
    sessions: HashMap<String, Entity<SessionView>>,
    shown: Option<String>,
    /// The chat the saved `view` names, until the index lists it and it is shown.
    restoring: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for ChatPanel {}

impl ChatPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(runtime) = ChatRuntime::global(cx) {
            let store = runtime.read(cx).store().clone();
            subscriptions.push(cx.subscribe_in(
                &store,
                window,
                |this, store, _: &ChatStoreEvent, window, cx| {
                    this.forget_deleted(store, cx);
                    this.restore_view(window, cx);
                },
            ));
        }
        let list = cx.new({
            let workspace = workspace.clone();
            |cx| ChatListView::new(workspace, cx)
        });
        subscriptions.push(cx.observe(&list, |_, _, cx| cx.notify()));
        let restoring = view_state::read(VIEW_KEY, cx).filter(|view| view != LIST_VIEW);
        let mut this = Self {
            workspace,
            project,
            fs,
            focus_handle: cx.focus_handle(),
            list,
            sessions: HashMap::default(),
            shown: None,
            restoring,
            _subscriptions: subscriptions,
        };
        this.restore_view(window, cx);
        this
    }

    /// Shows the chat the saved `view` names, once the index lists it. Nothing moves focus.
    fn restore_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.restoring.clone() else {
            return;
        };
        if self.show_chat(&id, window, cx) {
            self.restoring = None;
        }
    }

    /// The list of chats.
    pub fn list(&self) -> &Entity<ChatListView> {
        &self.list
    }

    /// The text of the view shown: the list, or the header line of the chat.
    pub fn view_text(&self, cx: &App) -> String {
        match self.session() {
            Some(session) => format!("Chat\n{}\n", session.read(cx).header_text(cx)),
            None => self.list.read(cx).view_text(cx),
        }
    }

    /// Drops the session views of chats whose record is gone (deleted, or cleaned up after a chat
    /// that was never prompted), and stops showing such a chat.
    fn forget_deleted(&mut self, store: &Entity<ChatStore>, cx: &mut Context<Self>) {
        let store = store.read(cx);
        if !store.is_enabled() {
            return;
        }
        self.sessions.retain(|id, _| store.find(id).is_some());
        if self
            .shown
            .as_deref()
            .is_some_and(|id| !self.sessions.contains_key(id))
        {
            self.shown = None;
            view_state::write(VIEW_KEY, Some(LIST_VIEW.to_string()), cx);
        }
        cx.notify();
    }

    /// Shows the chat `chat_id`. Returns false, showing nothing new, while chats are off or when
    /// no record has the id.
    pub fn show_chat(
        &mut self,
        chat_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !ChatsSettings::get_global(cx).enabled {
            return false;
        }
        let known = ChatRuntime::global(cx)
            .is_some_and(|runtime| runtime.read(cx).store().read(cx).find(chat_id).is_some());
        if !known {
            return false;
        }
        let project = self.project.clone();
        if !self.sessions.contains_key(chat_id) {
            let workspace = self.workspace.clone();
            let fs = self.fs.clone();
            let id = chat_id.to_string();
            let session = cx.new(|cx| SessionView::new(id, workspace, project, fs, window, cx));
            self._subscriptions.push(cx.subscribe(
                &session,
                |this, _, event: &SessionViewEvent, cx| match event {
                    SessionViewEvent::ShowList => this.show_list(cx),
                },
            ));
            self.sessions.insert(chat_id.to_string(), session);
        }
        self.shown = Some(chat_id.to_string());
        self.restoring = None;
        view_state::write(VIEW_KEY, Some(chat_id.to_string()), cx);
        cx.notify();
        true
    }

    /// Stops showing a chat and shows the list; the choice is saved.
    pub fn show_list(&mut self, cx: &mut Context<Self>) {
        self.shown = None;
        self.restoring = None;
        view_state::write(VIEW_KEY, Some(LIST_VIEW.to_string()), cx);
        cx.notify();
    }

    /// `chat: show archive`: the list, with Archived expanded.
    pub fn show_archive(&mut self, cx: &mut Context<Self>) {
        self.list
            .update(cx, |list, cx| list.set_archived_expanded(true, cx));
        self.show_list(cx);
    }

    /// The id of the chat shown.
    pub fn shown_chat(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// The session view of the chat shown.
    pub fn session(&self) -> Option<Entity<SessionView>> {
        self.sessions.get(self.shown.as_deref()?).cloned()
    }

    /// The editor of the prompts pane and the id of its chat, while that pane has focus.
    pub fn focused_prompts(&self, window: &Window, cx: &App) -> Option<(Entity<Editor>, String)> {
        let session = self.session()?;
        let session = session.read(cx);
        let editor = session.focused_prompts_editor(window, cx)?;
        Some((editor, session.chat_id().to_string()))
    }
}

impl Render for ChatPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.session() {
            Some(session) => session.into_any_element(),
            None => self.list.clone().into_any_element(),
        };
        let _ = cx;
        v_flex()
            .id("chat-panel")
            .key_context("ChatPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(body)
    }
}

impl Focusable for ChatPanel {
    /// The list takes the focus while it is shown, so its keys work at once.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.shown {
            None => self.list.read(cx).focus_handle(cx),
            Some(_) => self.focus_handle.clone(),
        }
    }
}

impl Panel for ChatPanel {
    fn persistent_name() -> &'static str {
        "Chats Panel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        ChatsSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.chats.get_or_insert_default().dock = Some(position.into());
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        px(ChatsSettings::get_global(cx).default_width)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Chat)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Chats Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(chat::TogglePanel)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// Adds the Chats panel to `workspace` while `chats.enabled` is true and removes it while false,
/// now and whenever the settings change (D24). Nothing is added when the runtime is missing.
pub fn init_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    sync_panel(workspace, window, cx);
    cx.observe_global_in::<SettingsStore>(window, |workspace, window, cx| {
        sync_panel(workspace, window, cx)
    })
    .detach();
}

fn sync_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let enabled = ChatsSettings::get_global(cx).enabled && ChatRuntime::global(cx).is_some();
    match (enabled, workspace.panel::<ChatPanel>(cx)) {
        (true, None) => {
            let weak = cx.entity().downgrade();
            let fs = workspace.app_state().fs.clone();
            let project = workspace.project().clone();
            let panel = cx.new(|cx| ChatPanel::new(weak, project, fs, window, cx));
            workspace.add_panel(panel, window, cx);
        }
        (false, Some(panel)) => workspace.remove_panel(&panel, window, cx),
        _ => {}
    }
}

/// Registers `chat: toggle panel`, `chat: toggle prompt editor`, `chat: focus terminal` and
/// `chat: focus prompts` on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &chat::TogglePanel, window, cx| {
            if enabled_panel(workspace, cx).is_some() {
                workspace.toggle_panel_focus::<ChatPanel>(window, cx);
            }
        })
        .register_action(|workspace, _: &chat::TogglePromptEditor, window, cx| {
            let Some(session) = shown_session(workspace, cx) else {
                return;
            };
            if let Err(message) =
                session.update(cx, |session, cx| session.toggle_prompt_editor(window, cx))
            {
                show_toast(workspace, message.to_string(), cx);
            }
        })
        .register_action(|workspace, _: &chat::FocusTerminal, window, cx| {
            let Some(session) = shown_session(workspace, cx) else {
                return;
            };
            workspace.open_panel::<ChatPanel>(window, cx);
            session.update(cx, |session, cx| session.focus_terminal(window, cx));
        })
        .register_action(|workspace, _: &chat::FocusPrompts, window, cx| {
            focus_prompts(workspace, window, cx)
        })
        .register_action(|workspace, _: &chat::ShowArchive, window, cx| {
            let Some(panel) = enabled_panel(workspace, cx) else {
                return;
            };
            panel.update(cx, |panel, cx| panel.show_archive(cx));
            workspace.open_panel::<ChatPanel>(window, cx);
        })
        .register_action(|workspace, _: &chat::ToggleAllTasks, window, cx| {
            let Some(panel) = list_panel(workspace, window, cx) else {
                return;
            };
            let list = panel.read(cx).list().clone();
            list.update(cx, |list, cx| list.toggle_all_tasks(cx));
        })
        .register_action(|workspace, _: &chat::Filter, window, cx| {
            let Some(panel) = list_panel(workspace, window, cx) else {
                return;
            };
            let list = panel.read(cx).list().clone();
            list.update(cx, |list, cx| list.ask_filter(window, cx));
        });
    #[cfg(debug_assertions)]
    workspace.register_action(|workspace, _: &chat::DumpView, _, cx| dump_view(workspace, cx));
}

/// The panel with its list shown and its dock open, or a toast saying chats are off.
fn list_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ChatPanel>> {
    let panel = enabled_panel(workspace, cx)?;
    panel.update(cx, |panel, cx| panel.show_list(cx));
    workspace.open_panel::<ChatPanel>(window, cx);
    Some(panel)
}

/// The file `chat: dump view` writes in the logs directory.
#[cfg(debug_assertions)]
pub(crate) const DUMP_FILE: &str = "chat-view.txt";

/// Writes `text` to [`DUMP_FILE`] in `logs_dir` and returns the path.
#[cfg(debug_assertions)]
pub(crate) fn write_view(
    text: &str,
    logs_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// `chat: dump view` (debug builds): writes the text of the view the panel shows.
#[cfg(debug_assertions)]
fn dump_view(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(panel) = enabled_panel(workspace, cx) else {
        return;
    };
    let text = panel.read(cx).view_text(cx);
    let message = match write_view(&text, paths::logs_dir()) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the chat view: {error}"),
    };
    show_toast(workspace, message, cx);
}

/// The panel, or a toast saying chats are off.
pub(crate) fn enabled_panel(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ChatPanel>> {
    let panel = if ChatsSettings::get_global(cx).enabled {
        workspace.panel::<ChatPanel>(cx)
    } else {
        None
    };
    if panel.is_none() {
        show_toast(workspace, SendError::Off.to_string(), cx);
    }
    panel
}

/// The session view of the chat the panel shows, or a toast saying why there is none.
fn shown_session(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<SessionView>> {
    let panel = enabled_panel(workspace, cx)?;
    let session = panel.read(cx).session();
    if session.is_none() {
        show_toast(workspace, NO_CHAT.to_string(), cx);
    }
    session
}

/// `chat: focus prompts`: the prompts pane of the chat shown, else the prompts file of the focused
/// task as an ordinary buffer in the center.
fn focus_prompts(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(panel) = enabled_panel(workspace, cx) else {
        return;
    };
    let Some(session) = panel.read(cx).session() else {
        open_prompts_in_center(workspace, window, cx);
        return;
    };
    workspace.open_panel::<ChatPanel>(window, cx);
    if let Err(message) = session.update(cx, |session, cx| session.focus_prompts(window, cx)) {
        show_toast(workspace, message.to_string(), cx);
    }
}

/// Opens the `prompts.txt` of the focused task in the center, creating it empty when it is not
/// there.
fn open_prompts_in_center(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let dir = WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .and_then(|tree| {
            let id = bongos_work::focused(&tree, cx)?;
            Some(tree.task_by_id(&id)?.dir.clone())
        });
    let Some(dir) = dir else {
        show_toast(
            workspace,
            "Focus a task to open its prompts file".to_string(),
            cx,
        );
        return;
    };
    let path = layout::prompts_file(&dir);
    let fs = workspace.app_state().fs.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        fs.create_file(
            &path,
            CreateOptions {
                overwrite: false,
                ignore_if_exists: true,
            },
        )
        .await?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, OpenOptions::default(), window, cx)
            })?
            .await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
