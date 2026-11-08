//! The session view of the Chats panel (design D14, D20): a one-line header above a [`PaneGroup`]
//! with the chat's terminal and, in dual layout, the prompts pane split to the right.
//!
//! The terminal pane holds a `TerminalView` on the terminal the runtime owns; the view never
//! starts or stops anything. The prompts pane holds an editor on the chat's prompts file, built by
//! the global [`PromptPaneProvider`] (a plain editor until feature 25 plugs in). Hiding the prompts
//! pane only takes it out of the group: the pane and its editor stay alive in this view, so unsaved
//! text survives, and the terminal is not touched. The shown layout is written to the chat's
//! record and read back when the view is created. Focus never moves to the terminal on its own.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use bongos_chat_model::{ChatRecord, Layout};
use collections::HashMap;
use editor::Editor;
use fs::{CreateOptions, Fs};
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, Task, WeakEntity, Window,
};
use language::Buffer;
use project::Project;
use settings::{Settings as _, SettingsStore};
use terminal_view::TerminalView;
use ui::{Button, Clickable as _, Color, Label, LabelCommon as _, LabelSize, h_flex, v_flex};
use workspace::item::ItemHandle;
use workspace::{Pane, PaneGroup, PaneRenderContext, SplitDirection, Workspace};

use crate::layout::{self, PROMPT_PANE_OFF};
use crate::send::prompts_file;
use crate::{ChatRuntime, ChatRuntimeEvent, ChatsSettings};

/// Builds the item of the prompts pane for a prompts file. The default is a plain editor; feature
/// 25 installs its own with [`set_prompt_pane_provider`]. The item must act as an [`Editor`] for
/// `chat: send region` to read its text.
pub trait PromptPaneProvider: 'static {
    fn build_item(
        &self,
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Box<dyn ItemHandle>;
}

/// A plain text editor: no block rules, no `prompts:` commands.
struct PlainEditor;

impl PromptPaneProvider for PlainEditor {
    fn build_item(
        &self,
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Box<dyn ItemHandle> {
        Box::new(cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx)))
    }
}

struct GlobalPromptPaneProvider(Rc<dyn PromptPaneProvider>);

impl gpui::Global for GlobalPromptPaneProvider {}

/// Replaces the default plain-editor provider of the prompts pane.
pub fn set_prompt_pane_provider(provider: impl PromptPaneProvider, cx: &mut App) {
    cx.set_global(GlobalPromptPaneProvider(Rc::new(provider)));
}

fn prompt_pane_provider(cx: &App) -> Rc<dyn PromptPaneProvider> {
    cx.try_global::<GlobalPromptPaneProvider>()
        .map(|global| global.0.clone())
        .unwrap_or_else(|| Rc::new(PlainEditor))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionViewEvent {
    /// The "Chats" link of the header was clicked.
    ShowList,
}

/// One chat's terminal and prompts panes.
pub struct SessionView {
    chat_id: String,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    terminal_pane: Entity<Pane>,
    prompts_pane: Entity<Pane>,
    group: PaneGroup,
    active_pane: Entity<Pane>,
    /// The layout the record asks for; what is shown also depends on `chats.prompt_pane`.
    layout: Layout,
    terminal_view: Option<Entity<TerminalView>>,
    _loading_prompts: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionViewEvent> for SessionView {}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn new_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut Context<SessionView>,
) -> Entity<Pane> {
    cx.new(|cx| {
        let mut pane = Pane::new(
            workspace,
            project,
            Default::default(),
            None,
            gpui::NoAction.boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_can_navigate(false, cx);
        pane.set_close_pane_if_empty(false, cx);
        pane.set_should_display_tab_bar(|_, _| false);
        pane.display_nav_history_buttons(None);
        pane
    })
}

impl SessionView {
    pub fn new(
        chat_id: String,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let layout = ChatRuntime::global(cx)
            .and_then(|runtime| {
                let store = runtime.read(cx).store().read(cx);
                Some(store.find(&chat_id)?.entry.record()?.layout)
            })
            .unwrap_or_default();
        let terminal_pane = new_pane(workspace.clone(), project.clone(), window, cx);
        let prompts_pane = new_pane(workspace.clone(), project.clone(), window, cx);
        let mut subscriptions = Vec::new();
        for pane in [&terminal_pane, &prompts_pane] {
            subscriptions.push(cx.subscribe_in(
                pane,
                window,
                |this, pane, event: &workspace::pane::Event, _, cx| {
                    if matches!(event, workspace::pane::Event::Focus) {
                        this.active_pane = pane.clone();
                        cx.notify();
                    }
                },
            ));
        }
        if let Some(runtime) = ChatRuntime::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &runtime,
                window,
                |this, _, event: &ChatRuntimeEvent, window, cx| {
                    let ChatRuntimeEvent::Changed(id) = event;
                    if *id == this.chat_id {
                        this.sync_terminal(window, cx);
                    }
                    cx.notify();
                },
            ));
            let store = runtime.read(cx).store().clone();
            subscriptions.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        subscriptions.push(
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                this.apply_layout(window, cx);
            }),
        );
        let mut this = Self {
            chat_id,
            workspace,
            project,
            fs,
            focus_handle: cx.focus_handle(),
            group: PaneGroup::new(terminal_pane.clone()),
            active_pane: terminal_pane.clone(),
            terminal_pane,
            prompts_pane,
            layout,
            terminal_view: None,
            _loading_prompts: None,
            _subscriptions: subscriptions,
        };
        this.rebuild_group(cx);
        this.sync_terminal(window, cx);
        this.load_prompts(window, cx);
        this
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn terminal_pane(&self) -> &Entity<Pane> {
        &self.terminal_pane
    }

    pub fn prompts_pane(&self) -> &Entity<Pane> {
        &self.prompts_pane
    }

    /// The terminal view, once the chat has a terminal.
    pub fn terminal_view(&self) -> Option<&Entity<TerminalView>> {
        self.terminal_view.as_ref()
    }

    /// The layout shown: the record's, unless `chats.prompt_pane` is false.
    pub fn shown_layout(&self, cx: &App) -> Layout {
        layout::effective(self.layout, ChatsSettings::get_global(cx).prompt_pane)
    }

    /// The panes in the group now: the terminal pane, and the prompts pane in dual layout.
    pub fn visible_panes(&self) -> Vec<Entity<Pane>> {
        self.group.panes().into_iter().cloned().collect()
    }

    /// The editor of the prompts pane, once its buffer has loaded.
    pub fn prompts_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.prompts_pane
            .read(cx)
            .active_item()?
            .act_as::<Editor>(cx)
    }

    /// The prompts editor while keyboard focus is inside the prompts pane.
    pub fn focused_prompts_editor(&self, window: &Window, cx: &App) -> Option<Entity<Editor>> {
        if !self
            .prompts_pane
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            return None;
        }
        self.prompts_editor(cx)
    }

    /// The path of the record file of this chat.
    fn record_path(&self, cx: &App) -> Option<PathBuf> {
        let store = ChatRuntime::global(cx)?.read(cx).store().clone();
        store
            .read(cx)
            .index()
            .iter()
            .find(|(_, chat)| chat.entry.id() == self.chat_id)
            .map(|(path, _)| path.clone())
    }

    /// Puts the terminal pane and the prompts pane in the group the layout asks for.
    fn rebuild_group(&mut self, cx: &mut Context<Self>) {
        let mut group = PaneGroup::new(self.terminal_pane.clone());
        if layout::shows_prompts(self.shown_layout(cx)) {
            group.split(
                &self.terminal_pane,
                &self.prompts_pane,
                SplitDirection::Right,
                cx,
            );
        }
        self.group = group;
        if !self.group.panes().contains(&&self.active_pane) {
            self.active_pane = self.terminal_pane.clone();
        }
        cx.notify();
    }

    /// Follows a change of `chats.prompt_pane`. Focus inside a pane that goes away moves to the
    /// view itself, never to the terminal.
    fn apply_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_prompts = self.visible_panes().contains(&self.prompts_pane);
        let had_focus = self
            .prompts_pane
            .focus_handle(cx)
            .contains_focused(window, cx);
        self.rebuild_group(cx);
        let has_prompts = self.visible_panes().contains(&self.prompts_pane);
        if had_prompts && !has_prompts && had_focus {
            window.focus(&self.focus_handle, cx);
        }
    }

    /// `chat: toggle prompt editor`: removes the prompts pane, or adds it back, and writes the
    /// choice to the record. Refused with the text to show while `chats.prompt_pane` is false.
    pub fn toggle_prompt_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if !ChatsSettings::get_global(cx).prompt_pane {
            return Err(PROMPT_PANE_OFF);
        }
        let showing = !layout::shows_prompts(self.layout);
        self.set_layout(layout::toggled(self.layout), window, cx);
        if showing {
            self.prompts_pane
                .update(cx, |pane, cx| pane.focus_active_item(window, cx));
        }
        Ok(())
    }

    fn set_layout(&mut self, new: Layout, window: &mut Window, cx: &mut Context<Self>) {
        self.layout = new;
        self.apply_layout(window, cx);
        let (Some(path), Some(runtime)) = (self.record_path(cx), ChatRuntime::global(cx)) else {
            return;
        };
        let store = runtime.read(cx).store().clone();
        let write = store.update(cx, |store, cx| {
            store.update(
                path,
                move |record: &mut ChatRecord| {
                    record.layout = new;
                    Ok(())
                },
                cx,
            )
        });
        cx.spawn(async move |_, _| {
            if let Err(error) = write.await {
                log::warn!("chats: could not record the layout: {error}");
            }
        })
        .detach();
    }

    /// `chat: focus terminal`.
    pub fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_pane
            .update(cx, |pane, cx| pane.focus_active_item(window, cx));
    }

    /// `chat: focus prompts`. A hidden prompts pane is shown first (and the choice written).
    /// Refused with the text to show while `chats.prompt_pane` is false.
    pub fn focus_prompts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if !ChatsSettings::get_global(cx).prompt_pane {
            return Err(PROMPT_PANE_OFF);
        }
        if !layout::shows_prompts(self.layout) {
            self.set_layout(Layout::Dual, window, cx);
        }
        self.prompts_pane
            .update(cx, |pane, cx| pane.focus_active_item(window, cx));
        Ok(())
    }

    /// Shows the chat's terminal, and a new one after a resume gave the chat another.
    fn sync_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = ChatRuntime::global(cx) else {
            return;
        };
        let Some(terminal) = runtime
            .read(cx)
            .live(&self.chat_id)
            .map(|chat| chat.terminal().clone())
        else {
            // A closed chat keeps showing what its terminal last showed.
            return;
        };
        let current = self
            .terminal_view
            .as_ref()
            .map(|view| view.read(cx).terminal().clone());
        if current.as_ref() == Some(&terminal) {
            return;
        }
        let workspace = self.workspace.clone();
        let project = self.project.downgrade();
        let view = cx.new(|cx| TerminalView::new(terminal, workspace, None, project, window, cx));
        let old = self.terminal_view.replace(view.clone());
        self.terminal_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), false, false, None, window, cx);
            if let Some(old) = old {
                pane.remove_item(old.entity_id(), false, false, window, cx);
            }
        });
    }

    /// The prompts file of this chat: its `prompts_file`, else the `prompts.txt` of its scope.
    fn prompts_path(&self, cx: &App) -> Option<PathBuf> {
        let store = ChatRuntime::global(cx)?.read(cx).store().clone();
        let store = store.read(cx);
        let chat = store.find(&self.chat_id)?;
        Some(prompts_file(&chat.scope_dir, chat.entry.record()?))
    }

    /// Opens the prompts file into the prompts pane. The file is created empty when it is not
    /// there, so the pane never shows an error for a fresh chat.
    fn load_prompts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.prompts_path(cx) else {
            return;
        };
        let project = self.project.clone();
        let fs = self.fs.clone();
        self._loading_prompts = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = async {
                fs.create_file(
                    &path,
                    CreateOptions {
                        overwrite: false,
                        ignore_if_exists: true,
                    },
                )
                .await?;
                project
                    .update(cx, |project, cx| project.open_local_buffer(&path, cx))
                    .await
            }
            .await;
            match opened {
                Ok(buffer) => {
                    this.update_in(cx, |this, window, cx| {
                        this.set_prompts_buffer(buffer, window, cx)
                    })
                    .ok();
                }
                Err(error) => log::warn!("chats: could not open the prompts file: {error}"),
            }
        }));
    }

    fn set_prompts_buffer(
        &mut self,
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = prompt_pane_provider(cx);
        let item = provider.build_item(buffer, self.project.clone(), window, cx);
        self.prompts_pane.update(cx, |pane, cx| {
            pane.add_item(item, false, false, None, window, cx)
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut line = String::new();
        if let Some(runtime) = ChatRuntime::global(cx) {
            let runtime = runtime.read(cx);
            if let Some(record) = runtime
                .store()
                .read(cx)
                .find(&self.chat_id)
                .and_then(|chat| chat.entry.record())
            {
                line = format!(
                    "{} - {} - {} - {}",
                    record.name,
                    record.agent,
                    record.chat_type,
                    runtime.state(&self.chat_id)
                );
            }
        }
        h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .child(Label::new(line).size(LabelSize::Small).color(Color::Muted))
            .child(
                Button::new("chat-session-list", "Chats")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::ShowList))),
            )
            .into_any_element()
    }
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let panes = self
            .workspace
            .update(cx, |workspace, cx| {
                self.group
                    .render(
                        workspace.zoomed_item(),
                        &PaneRenderContext {
                            follower_states: &HashMap::default(),
                            active_pane: &self.active_pane,
                            app_state: workspace.app_state(),
                            project: workspace.project(),
                            workspace: &workspace.weak_handle(),
                        },
                        window,
                        cx,
                    )
                    .into_any_element()
            })
            .ok();
        v_flex()
            .id("chat-session")
            .key_context("ChatSession")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(header)
            .child(v_flex().flex_1().size_full().children(panes))
    }
}
