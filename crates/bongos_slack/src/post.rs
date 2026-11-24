//! `slack: post` and `slack: reply`: Kyle's own posts. The text is the active editor's selected
//! region (else asked in the minibuffer), sent as typed with only `&`, `<` and `>` escaped. The
//! destination is picked among the conversations and the threads shown in open read buffers
//! (`slack: reply` takes the thread at point). Every post shows its first lines and asks yes or
//! no before anything is sent: Kyle's posts are always confirmed (see
//! [`bongos_slack_model::approval`]). A text past Slack's limit, or a "no", sends nothing. A post
//! whose answer never came is reported once and never sent again.
//!
//! [`PostDraft`], [`answer`] and [`send`] hold the logic and are tested over the fake Slack; the
//! command functions only wire them to the minibuffer and the toasts.

use bongos_minibuffer::{Candidate, Outcome, Session, Source, open};
use bongos_slack_model::approval::PostSummary;
use bongos_slack_model::render;
use bongos_slack_model::types::{ConversationId, Destination, Ts};
use bongos_slack_model::{ApiError, Origin};
use editor::{Editor, MultiBufferOffset};
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity, Window};
use workspace::Workspace;

use crate::connection;
use crate::read_buffer::{ConversationEntry, ReadSession, conversation_entries};
use crate::read_commands::active_read_session;
use crate::read_view::{reply_destination, threads_shown};
use crate::service::{NOT_CONNECTED_MESSAGE, SlackError, SlackService};
use crate::token_prompt::show_toast;

/// The host name of the Slack prompts in the minibuffer.
const HOST: &str = "slack";
/// How many lines of the text the confirmation shows.
pub const PREVIEW_LINES: usize = 5;
/// The longest thread line shown in the destination picker, in characters.
const THREAD_LINE_CHARS: usize = 60;
/// What a post whose answer never came says, once.
pub const POST_MAY_NOT_HAVE_BEEN_SENT: &str =
    "The post may not have been sent; check the channel before posting again";
const NOTHING_TO_POST: &str = "Nothing to post";
const NOT_IN_READ_BUFFER: &str = "Not in a Slack read buffer (slack: read channel)";

/// A destination the post picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationChoice {
    /// `#name`, `@user`, or `thread in #name: <its first line>`.
    pub label: String,
    /// How the confirmation and the toast name it: `#name` or `a thread in #name`.
    pub name: String,
    pub destination: Destination,
}

/// A thread shown in an open read buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownThread {
    /// The conversation's label, `#name`.
    pub conversation_label: String,
    pub conversation: ConversationId,
    pub thread: Ts,
    /// The first line of the thread's parent as the buffer shows it.
    pub line: String,
}

/// The picker's destinations: every conversation, then every thread shown in a read buffer.
pub fn destination_choices(
    entries: &[ConversationEntry],
    threads: &[ShownThread],
) -> Vec<DestinationChoice> {
    let conversations = entries.iter().map(|entry| DestinationChoice {
        label: entry.label.clone(),
        name: entry.label.clone(),
        destination: Destination {
            conversation: entry.id.clone(),
            thread: None,
        },
    });
    let threads = threads.iter().map(|shown| {
        let line: String = shown.line.chars().take(THREAD_LINE_CHARS).collect();
        DestinationChoice {
            label: format!("thread in {}: {line}", shown.conversation_label),
            name: thread_name(&shown.conversation_label),
            destination: Destination {
                conversation: shown.conversation.clone(),
                thread: Some(shown.thread.clone()),
            },
        }
    });
    conversations.chain(threads).collect()
}

/// How a thread of `conversation_label` is named: `a thread in #name`.
pub fn thread_name(conversation_label: &str) -> String {
    format!("a thread in {conversation_label}")
}

/// The threads shown in every open read buffer.
pub fn shown_threads(cx: &App) -> Vec<ShownThread> {
    let mut threads = Vec::new();
    for session in ReadSession::open_sessions(cx) {
        let session = session.read(cx);
        let title = session.title();
        let label = title.strip_prefix("Slack: ").unwrap_or(&title).to_string();
        for (thread, line) in threads_shown(session.transcript()) {
            threads.push(ShownThread {
                conversation_label: label.clone(),
                conversation: session.channel().clone(),
                thread,
                line,
            });
        }
    }
    threads
}

/// The text sent for `typed`: `&`, `<` and `>` escaped, nothing else changed. Refused, with the
/// message to show, when it is empty or past Slack's limit of [`render::MAX_POST_CHARS`] characters.
pub fn escaped_text(typed: &str) -> Result<String, String> {
    if typed.trim().is_empty() {
        return Err(NOTHING_TO_POST.to_string());
    }
    let text = render::escape_for_post(typed);
    if let Err(ApiError::TooLong { over }) = render::check_length(&text) {
        return Err(render::too_long_message(over));
    }
    Ok(text)
}

/// A post Kyle is about to confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostDraft {
    pub destination: Destination,
    /// `#name` or `a thread in #name`.
    pub name: String,
    /// The text as typed.
    pub typed: String,
    /// The text as sent: `typed` with `&`, `<` and `>` escaped, nothing else changed.
    pub text: String,
}

impl PostDraft {
    /// The post of `typed` to `destination`. Refused as [`escaped_text`] refuses it.
    pub fn new(destination: Destination, name: String, typed: String) -> Result<Self, String> {
        let text = escaped_text(&typed)?;
        Ok(Self {
            destination,
            name,
            typed,
            text,
        })
    }

    /// The confirmation: where, as whom, the length, the first [`PREVIEW_LINES`] lines of the
    /// text and the question.
    pub fn question(&self, workspace: &str, user: &str) -> String {
        let summary = PostSummary {
            destination: &self.name,
            workspace,
            user,
            text: &self.typed,
        }
        .text();
        let mut lines = summary.lines();
        let mut shown = lines
            .by_ref()
            .take(1 + PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let more = lines.count();
        if more > 0 {
            shown.push_str(&format!("\n… and {more} more lines"));
        }
        format!("{shown}\nSend it?")
    }

    /// The toast after Slack posted it.
    pub fn posted_message(&self) -> String {
        format!("Posted to {}", self.name)
    }
}

/// What a failed post says. A post whose answer never came says so once; it is never resent.
pub fn failure_message(draft: &PostDraft, error: &SlackError) -> String {
    match error {
        SlackError::PostMaybeNotSent { cause, .. } => {
            format!(
                "Posting to {} failed ({cause}). {POST_MAY_NOT_HAVE_BEEN_SENT}",
                draft.name
            )
        }
        SlackError::PostRefused { cause, .. } => {
            format!("Slack did not post to {}: {cause}", draft.name)
        }
        error => error.to_string(),
    }
}

/// Sends `draft` once, as Kyle, and answers the message to show. Never repeats a failed post.
pub fn send(service: &Entity<SlackService>, draft: PostDraft, cx: &mut App) -> Task<String> {
    let task = service.update(cx, |service, cx| {
        service.post(
            draft.destination.clone(),
            draft.text.clone(),
            Origin::Kyle,
            cx,
        )
    });
    cx.background_spawn(async move {
        match task.await {
            Ok(_) => draft.posted_message(),
            Err(error) => failure_message(&draft, &error),
        }
    })
}

/// Kyle's answer to the confirmation of `draft`: yes sends it (see [`send`]); no sends nothing
/// and answers `None`.
pub fn answer(
    service: &Entity<SlackService>,
    draft: PostDraft,
    yes: bool,
    cx: &mut App,
) -> Option<Task<String>> {
    yes.then(|| send(service, draft, cx))
}

/// The text of the newest selection of `editor`, when it is not empty.
fn selection_text(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<String> {
    let snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
    (!selection.is_empty()).then(|| {
        snapshot
            .buffer_snapshot()
            .text_for_range(selection.start..selection.end)
            .collect()
    })
}

/// `slack: post`: the selected region (else a text asked for), a destination, then the
/// confirmation.
pub(crate) fn post(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(service) = SlackService::global(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    let selected = workspace
        .active_item_as::<Editor>(cx)
        .and_then(|editor| editor.update(cx, selection_text));
    match selected {
        Some(text) => pick_destination(workspace, service, text, window, cx),
        None => {
            let weak = cx.entity().downgrade();
            ask_text(
                workspace,
                "Post text: ",
                window,
                cx,
                move |text, window, cx| {
                    weak.update(cx, |workspace, cx| {
                        pick_destination(workspace, service, text, window, cx)
                    })
                    .ok();
                },
            );
        }
    }
}

/// `slack: reply`: a text asked for, posted in the thread at point of a read buffer (a message
/// with no replies yet starts its thread).
pub(crate) fn reply(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(service) = SlackService::global(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    let Some((_, session, row)) = active_read_session(workspace, cx) else {
        show_toast(workspace, NOT_IN_READ_BUFFER.to_string(), cx);
        return;
    };
    let (destination, title) = {
        let session = session.read(cx);
        (
            reply_destination(session.channel(), session.transcript(), row),
            session.title(),
        )
    };
    let Some(destination) = destination else {
        show_toast(workspace, "No message at point".to_string(), cx);
        return;
    };
    let label = title.strip_prefix("Slack: ").unwrap_or(&title);
    let name = thread_name(label);
    let weak = cx.entity().downgrade();
    ask_text(workspace, "Reply: ", window, cx, move |text, window, cx| {
        weak.update(cx, |workspace, cx| {
            confirm(workspace, service, destination, name, text, window, cx)
        })
        .ok();
    });
}

/// Asks for the text of a post in the minibuffer.
fn ask_text(
    workspace: &mut Workspace,
    prompt: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(String, &mut Window, &mut App) + 'static,
) {
    let session = Session::completing_read(HOST, prompt)
        .free_input(true)
        .no_history()
        .on_confirm(move |outcome, window, cx| {
            if let Outcome::Input(text) = outcome {
                then(text, window, cx);
            }
        });
    open(workspace, session, window, cx);
}

/// The destination picker, then the confirmation.
fn pick_destination(
    workspace: &mut Workspace,
    service: Entity<SlackService>,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Refuse a text past the limit before anything is asked.
    if let Err(message) = escaped_text(&text) {
        show_toast(workspace, message, cx);
        return;
    }
    let entries = conversation_entries(service.clone(), cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = entries.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Err(error) => show_toast(workspace, error.to_string(), cx),
                Ok(entries) => {
                    let choices = destination_choices(&entries, &shown_threads(cx));
                    let candidates = choices
                        .into_iter()
                        .map(|choice| Candidate::new(choice.label.clone(), choice))
                        .collect();
                    let weak = cx.entity().downgrade();
                    let session = Session::completing_read(HOST, "Post to: ")
                        .source(Source::Fixed(candidates))
                        .on_confirm(move |outcome, window, cx| {
                            let Outcome::Candidate(value) = outcome else {
                                return;
                            };
                            let Some(choice) = value.downcast_ref::<DestinationChoice>() else {
                                return;
                            };
                            let choice = choice.clone();
                            weak.update(cx, |workspace, cx| {
                                confirm(
                                    workspace,
                                    service,
                                    choice.destination,
                                    choice.name,
                                    text,
                                    window,
                                    cx,
                                )
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

/// Shows the first lines of the post and asks yes or no; sends it only on yes.
fn confirm(
    workspace: &mut Workspace,
    service: Entity<SlackService>,
    destination: Destination,
    name: String,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let draft = match PostDraft::new(destination, name, text) {
        Ok(draft) => draft,
        Err(message) => {
            show_toast(workspace, message, cx);
            return;
        }
    };
    let Some(connection) = connection::current(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    let question = draft.question(&connection.workspace, &connection.user);
    let weak = cx.entity().downgrade();
    bongos_minibuffer::yes_or_no(
        workspace,
        HOST,
        question,
        false,
        move |yes, _, cx| {
            if let Some(task) = answer(&service, draft, yes, cx) {
                report(weak, task, cx);
            }
        },
        window,
        cx,
    );
}

/// Shows the message of a finished post once.
fn report(workspace: WeakEntity<Workspace>, task: Task<String>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let message = task.await;
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}
