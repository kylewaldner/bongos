//! The editor side of `slack: read channel`, `slack: load older messages` and
//! `slack: read thread`: the conversation picker, the read-only buffers and point. The state and
//! the loading are [`ReadSession`]'s, which is tested without a window.

use bongos_minibuffer::{Candidate, Outcome, Session, Source, open};
use editor::scroll::Autoscroll;
use editor::{Editor, SelectionEffects};
use gpui::{Context, Entity, Window};
use language::{Buffer, Point};
use workspace::Workspace;

use crate::read_buffer::{
    ConversationEntry, ReadSession, SessionError, conversation_entries, link_label, picker_entries,
    read_thread,
};
use crate::read_view::{MessageLink, buffer_title};
use crate::service::{NOT_CONNECTED_MESSAGE, SlackService};
use crate::token_prompt::{add_read_only_editor, open_text_buffer, show_toast};

/// The host name of the Slack prompts in the minibuffer.
const HOST: &str = "slack";
const NOT_IN_READ_BUFFER: &str = "Not in a Slack read buffer (slack: read channel)";

/// `slack: read channel`: picks a conversation and opens `Slack: <label>`.
pub(crate) fn read_channel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(service) = SlackService::global(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    let entries = conversation_entries(service.clone(), cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = entries.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Err(error) => show_toast(workspace, error.to_string(), cx),
                Ok(entries) => {
                    let candidates = entries
                        .into_iter()
                        .map(|entry| Candidate::new(entry.label.clone(), entry))
                        .collect();
                    let weak = cx.entity().downgrade();
                    let session = Session::completing_read(HOST, "Read channel: ")
                        .source(Source::Fixed(candidates))
                        .on_confirm(move |outcome, window, cx| {
                            let Outcome::Candidate(value) = outcome else {
                                return;
                            };
                            let Some(entry) = value.downcast_ref::<ConversationEntry>() else {
                                return;
                            };
                            let entry = entry.clone();
                            let service = service.clone();
                            weak.update(cx, |workspace, cx| {
                                open_channel(workspace, service, entry, window, cx)
                            })
                            .ok();
                        });
                    open(workspace, session, window, cx);
                }
            })
            .ok();
    })
    .detach();
}

/// Opens the read-only buffer `Slack: <label>` and loads its newest page into it.
fn open_channel(
    workspace: &mut Workspace,
    service: Entity<SlackService>,
    entry: ConversationEntry,
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
        workspace
            .update_in(cx, |workspace, window, cx| {
                let session = ReadSession::open(service, buffer.clone(), entry.id, entry.label, cx);
                let title = session.read(cx).title();
                let editor = add_read_only_editor(workspace, buffer, title, window, cx);
                let load = session.update(cx, |session, cx| session.load_first(cx));
                cx.spawn_in(window, async move |workspace, cx| {
                    match load.await {
                        Ok(()) => {
                            editor
                                .update_in(cx, |editor, window, cx| {
                                    editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
                                })
                                .ok();
                        }
                        Err(error) => {
                            workspace
                                .update(cx, |workspace, cx| {
                                    show_toast(workspace, error.to_string(), cx)
                                })
                                .ok();
                        }
                    };
                })
                .detach();
            })
            .ok();
    })
    .detach();
}

/// The active editor, the read session it shows and the row of its cursor.
pub(crate) fn active_read_session(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
) -> Option<(Entity<Editor>, Entity<ReadSession>, usize)> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let buffer: Entity<Buffer> = editor.read(cx).buffer().read(cx).as_singleton()?;
    let session = ReadSession::for_buffer(&buffer, cx)?;
    let row = editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head().row as usize
    });
    Some((editor, session, row))
}

/// `slack: load older messages`: reads the page before the oldest message shown and keeps point
/// on the message it was on.
pub(crate) fn load_older(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((editor, session, row)) = active_read_session(workspace, cx) else {
        show_toast(workspace, NOT_IN_READ_BUFFER.to_string(), cx);
        return;
    };
    let load = session.update(cx, |session, cx| session.load_older(row, cx));
    cx.spawn_in(window, async move |workspace, cx| match load.await {
        Ok(Some(row)) => {
            editor
                .update_in(cx, |editor, window, cx| {
                    let point = Point::new(row as u32, 0);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                })
                .ok();
        }
        Ok(None) => {}
        Err(error) => {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, error.to_string(), cx)
                })
                .ok();
        }
    })
    .detach();
}

/// `slack: read thread`: the thread of the message at point in a read buffer, or, anywhere else,
/// the thread of a message link asked for.
pub(crate) fn read_thread_command(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(service) = SlackService::global(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    if let Some((_, session, row)) = active_read_session(workspace, cx) {
        let (channel, thread, title) = {
            let session = session.read(cx);
            (
                session.channel().clone(),
                session.thread_at(row),
                session.title(),
            )
        };
        let Some(thread) = thread else {
            show_toast(workspace, "No message at point".to_string(), cx);
            return;
        };
        let label = title.strip_prefix("Slack: ").unwrap_or(&title).to_string();
        open_thread(service, channel, thread, label, window, cx);
        return;
    }
    let weak = cx.entity().downgrade();
    let session = Session::completing_read(HOST, "Slack message link: ")
        .free_input(true)
        .accept(
            |input| MessageLink::parse(input).is_some(),
            "not a Slack message link",
        )
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Input(text) = outcome else {
                return;
            };
            let Some(link) = MessageLink::parse(&text) else {
                return;
            };
            let service = service.clone();
            weak.update(cx, |_, cx| {
                let label = {
                    let service = service.read(cx);
                    let entries = service
                        .cached_conversations()
                        .map(|list| picker_entries(list, &service.user_names()))
                        .unwrap_or_default();
                    link_label(&link, &entries)
                };
                open_thread(
                    service,
                    link.conversation.clone(),
                    link.thread(),
                    label,
                    window,
                    cx,
                );
            })
            .ok();
        });
    open(workspace, session, window, cx);
}

fn open_thread(
    service: Entity<SlackService>,
    channel: bongos_slack_model::types::ConversationId,
    thread: bongos_slack_model::types::Ts,
    label: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let task = read_thread(service, channel, thread, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = task.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(transcript) => open_text_buffer(
                    buffer_title(&format!("thread in {label}")),
                    transcript.text,
                    workspace,
                    window,
                    cx,
                ),
                Err(error) => show_toast(workspace, SessionError::from(error).to_string(), cx),
            })
            .ok();
    })
    .detach();
}
