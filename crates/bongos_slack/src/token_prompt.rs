//! `slack: connect` and `slack: disconnect` in a window: the three-way choice, the masked
//! [`TokenPrompt`], the replace confirmation and the toasts.
//!
//! The token is typed only into [`TokenPrompt`], its own masked view (not a prompt of the
//! minibuffer, so it has no history), which blocks copy, cut and kill while the token is in it,
//! so neither the clipboard nor the kill ring can receive it.

use bongos_slack_model::scopes::admin_request_text;
use chrono::Utc;
use editor::actions::{Copy, CopyAndTrim, Cut, CutToEndOfLine, KillRingCut};
use editor::{Editor, MultiBuffer};
use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, Styled, WeakEntity, Window,
};
use language::Capability;
use ui::{
    ActiveTheme as _, DynamicSpacing, Headline, HeadlineSize, Icon, IconName, IconSize,
    InteractiveElement as _, Label, LabelCommon as _, LabelSize, ParentElement as _,
    StyledExt as _, StyledTypography as _, div, h_flex, v_flex,
};
use workspace::notifications::NotificationId;
use workspace::{ModalView, Toast, Workspace};

use crate::connection::{self, ConnectError};

/// The host name of the Slack prompts in the minibuffer.
const HOST: &str = "slack";

/// A toast of the Slack commands.
pub struct SlackToast;

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<SlackToast>(), message),
        cx,
    );
}

fn toast(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
        .ok();
}

/// `slack: connect`: asks to replace a connection first, then offers the three-way choice.
pub(crate) fn connect(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if connection::is_busy(cx) {
        show_toast(workspace, ConnectError::AlreadyRunning.to_string(), cx);
        return;
    }
    let Some(current) = connection::current(cx) else {
        choose(workspace, window, cx);
        return;
    };
    let weak = cx.entity().downgrade();
    bongos_minibuffer::yes_or_no(
        workspace,
        HOST,
        format!("Replace the connection to {}?", current.workspace),
        false,
        move |yes, window, cx| {
            if yes {
                weak.update(cx, |workspace, cx| choose(workspace, window, cx))
                    .ok();
            }
        },
        window,
        cx,
    );
}

/// The three-way choice: paste a token, how to make the app, the admin request.
fn choose(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let weak = cx.entity().downgrade();
    let prompt = bongos_minibuffer::ReadKey::new(HOST, "Connect Slack:")
        .key('p', "paste a token")
        .key('h', "how to make the Slack app")
        .key('a', "my workspace needs an admin to approve apps")
        .on_answer(move |key, window, cx| {
            weak.update(cx, |workspace, cx| match key {
                'p' => open_token_prompt(workspace, window, cx),
                'h' => open_text_buffer(
                    "How to make the Slack app".to_string(),
                    connection::how_to_make_the_app(),
                    workspace,
                    window,
                    cx,
                ),
                'a' => copy_admin_text(workspace, cx),
                _ => {}
            })
            .ok();
        });
    bongos_minibuffer::read_key(workspace, prompt, window, cx);
}

fn copy_admin_text(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    cx.write_to_clipboard(ClipboardItem::new_string(admin_request_text()));
    show_toast(
        workspace,
        "Copied the request for a Slack admin to the clipboard".to_string(),
        cx,
    );
}

fn open_token_prompt(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let weak = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| TokenPrompt::new(weak, window, cx));
}

/// Opens `text` in a read-only buffer titled `title`; nothing is written to disk.
pub(crate) fn open_text_buffer(
    title: String,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the buffer".to_string(), cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                add_read_only_editor(workspace, buffer, title, window, cx);
            })
            .ok();
    })
    .detach();
}

/// Shows `buffer` in a read-only editor titled `title` in the active pane.
pub(crate) fn add_read_only_editor(
    workspace: &mut Workspace,
    buffer: Entity<language::Buffer>,
    title: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Editor> {
    let project = workspace.project().clone();
    let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
        editor.set_read_only(true);
        editor
    });
    workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    editor
}

/// Checks and stores the token typed in the prompt, then says how it went. When the workspace
/// has not approved the app, offers to copy the admin request text.
fn check_and_store(workspace: WeakEntity<Workspace>, text: String, cx: &mut App) {
    let task = connection::connect(&text, Utc::now(), cx);
    drop(text);
    cx.spawn(async move |cx| {
        let result = task.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(connection) => show_toast(
                    workspace,
                    format!(
                        "Connected to Slack: {} as {}",
                        connection.workspace, connection.user
                    ),
                    cx,
                ),
                Err(ConnectError::NeedsAdmin) => {
                    let weak = cx.entity().downgrade();
                    bongos_minibuffer::yes_or_no(
                        workspace,
                        HOST,
                        format!(
                            "{}. Copy the request for an admin?",
                            ConnectError::NeedsAdmin
                        ),
                        true,
                        move |yes, _, cx| {
                            if yes {
                                weak.update(cx, |workspace, cx| copy_admin_text(workspace, cx))
                                    .ok();
                            }
                        },
                        window,
                        cx,
                    );
                }
                Err(error) => show_toast(workspace, error.to_string(), cx),
            })
            .ok();
    })
    .detach();
}

/// `slack: disconnect`.
pub(crate) fn disconnect(_: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    let task = connection::disconnect(cx);
    cx.spawn(async move |workspace, cx| {
        let message = match task.await {
            Ok(()) => "Disconnected from Slack".to_string(),
            Err(error) => error.to_string(),
        };
        cx.update(|cx| toast(&workspace, message, cx));
    })
    .detach();
}

/// The masked one-line modal the token is typed into.
pub struct TokenPrompt {
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
}

impl TokenPrompt {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor
        });
        Self { editor, workspace }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            editor.clear(window, cx);
            text
        });
        cx.emit(DismissEvent);
        if !text.trim().is_empty() {
            check_and_store(self.workspace.clone(), text, cx);
        }
    }
}

impl EventEmitter<DismissEvent> for TokenPrompt {}
impl ModalView for TokenPrompt {}

impl Focusable for TokenPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TokenPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SlackTokenPrompt")
            // The editor masks what it shows, not what it copies: while the token is typed, the
            // copy, cut and Emacs kill commands do nothing. Capture runs before the editor's own
            // handlers.
            .capture_action(|_: &Copy, _, cx| cx.stop_propagation())
            .capture_action(|_: &CopyAndTrim, _, cx| cx.stop_propagation())
            .capture_action(|_: &Cut, _, cx| cx.stop_propagation())
            .capture_action(|_: &CutToEndOfLine, _, cx| cx.stop_propagation())
            .capture_action(|_: &KillRingCut, _, cx| cx.stop_propagation())
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .size_full()
            .child(
                h_flex()
                    .font_buffer(cx)
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pt(DynamicSpacing::Base08.rems(cx))
                    .pb(DynamicSpacing::Base04.rems(cx))
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::LockOutlined).size(IconSize::XSmall))
                    .child(Headline::new("Slack token").size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .font_buffer(cx)
                    .text_buffer(cx)
                    .py_2()
                    .px_3()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .size_full()
                    .overflow_hidden()
                    .child(self.editor.clone())
                    .child(
                        Label::new("Checked with Slack, then stored in the secret store")
                            .size(LabelSize::Small),
                    ),
            )
    }
}
