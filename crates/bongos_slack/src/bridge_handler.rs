//! The app side of a chat agent's `bongos --slack <op>` (see [`bongos_slack_model::bridge`]):
//! the handler of `slack` requests on the chat channel socket, the pending posts, their needs-you
//! reason, `slack: review pending posts`, and the `## Slack` section of a chat's context.
//!
//! - A post is decided with [`decide`] for [`Origin::ChatAgent`] against the allowed list of the
//!   chat's task's project. An allowed post is sent at once; any other becomes a pending post and
//!   nothing is sent until Kyle approves it in `slack: review pending posts`.
//! - Pending posts live in memory only ([`PendingPosts`]): a crash or a restart drops them
//!   unposted, and `post-status` then answers that it does not know the id. An approval takes the
//!   post out of the waiting state before it is sent, so it is sent once; a failed post shows its
//!   reason and is never resent.
//! - While `slack.enabled` is false no handler is registered, the context gets no section and the
//!   needs-you source reports nothing. While Slack is not connected every operation answers
//!   [`NOT_CONNECTED`] and the context gets no section.
//!
//! No request, reply or context section holds the Slack token: the handler only ever sees the
//! service, which keeps the token to itself.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_chat::ChannelServer;
use bongos_chat::ContextExtension;
use bongos_chat_model::channel::report::ChatId;
use bongos_chat_model::channel::request::{Body, REPLY_WAIT, RequestKind, Value};
use bongos_minibuffer::{Candidate, Outcome, Session, Source, open};
use bongos_slack_model::bridge::{
    self, KIND, NOT_CONNECTED, PostStatus, Reply, Request, needs_you_reason, posted_text,
    waiting_text,
};
use bongos_slack_model::render;
use bongos_slack_model::types::{ConversationId, Destination, Ts};
use bongos_slack_model::{AllowedList, Decision, Origin, decide, question_hash, quote};
use bongos_work::{NeedsYou, NeedsYouRegistry, NeedsYouSource};
use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Context, Entity, Global, Task,
    TaskExt as _, Window,
};
use language::Capability;
use scheduler::Clock;
use settings::{Settings as _, SettingsStore};
use workspace::{SaveIntent, Workspace};

use crate::SlackSettings;
use crate::allowed;
use crate::connection;
use crate::post::{escaped_text, thread_name};
use crate::read_buffer::{ConversationEntry, conversation_entries, replies_of, resolve_authors};
use crate::service::{OFF_MESSAGE, RECONNECT_MESSAGE, SlackService, SlackState};
use crate::token_prompt::{add_read_only_editor, show_toast};

/// The name of the needs-you source and of the context extension.
pub const SOURCE_NAME: &str = "slack";
/// The host name of the Slack prompts in the minibuffer.
const HOST: &str = "slack";
/// How long the socket's handler thread waits for the main thread's answer: less than the
/// client's [`REPLY_WAIT`], so the client gets a reply rather than a timeout.
const HANDLER_WAIT: Duration = Duration::from_secs(REPLY_WAIT.as_secs() - 10);
const NO_PENDING: &str = "No Slack post is waiting for you";

/// The task and project of a chat, for its posts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatScope {
    pub task_id: Option<String>,
    pub project: Option<String>,
}

/// How the scope of a chat is found.
pub type ScopeOf = Rc<dyn Fn(&ChatId, &App) -> ChatScope>;

/// One post a chat agent asked for that was not allowed. Never changed once made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPost {
    pub id: String,
    pub destination: Destination,
    /// `#name` or `a thread in #name`.
    pub name: String,
    /// The text as the agent gave it; the post sends it with only `&`, `<` and `>` escaped.
    pub text: String,
    /// [`question_hash`] of the destination and the text.
    pub hash: String,
    pub chat: ChatId,
    pub task_id: Option<String>,
    pub created: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Pending,
    /// Approved and being sent.
    Posting,
    Posted(String),
    Rejected,
    Failed(String),
}

/// The posts chat agents asked for this session, in memory only.
pub struct PendingPosts {
    clock: Arc<dyn Clock>,
    /// Part of every id, so an id of an earlier process is not mistaken for one of this.
    session: String,
    next: u64,
    posts: Vec<(PendingPost, State)>,
}

impl PendingPosts {
    pub fn new(clock: Arc<dyn Clock>, session: String) -> Self {
        Self {
            clock,
            session,
            next: 1,
            posts: Vec::new(),
        }
    }

    /// Records a pending post and answers its id.
    pub fn add(
        &mut self,
        destination: Destination,
        name: String,
        text: String,
        chat: ChatId,
        task_id: Option<String>,
    ) -> String {
        let id = format!("{}-{}", self.session, self.next);
        self.next += 1;
        let hash = question_hash(&destination, &text);
        self.posts.push((
            PendingPost {
                id: id.clone(),
                destination,
                name,
                text,
                hash,
                chat,
                task_id,
                created: self.clock.utc_now(),
            },
            State::Pending,
        ));
        id
    }

    /// The posts still waiting, oldest first.
    pub fn waiting(&self) -> Vec<&PendingPost> {
        self.posts
            .iter()
            .filter(|(_, state)| *state == State::Pending)
            .map(|(post, _)| post)
            .collect()
    }

    pub fn status(&self, id: &str) -> PostStatus {
        match self.posts.iter().find(|(post, _)| post.id == id) {
            None => PostStatus::Unknown,
            Some((_, State::Pending | State::Posting)) => PostStatus::Pending,
            Some((_, State::Posted(link))) => PostStatus::Posted { link: link.clone() },
            Some((_, State::Rejected)) => PostStatus::Rejected,
            Some((_, State::Failed(reason))) => PostStatus::Failed {
                reason: reason.clone(),
            },
        }
    }

    /// Takes the waiting post `id` for sending: it stops waiting, so a second approval finds it
    /// gone. `None` when it is not waiting.
    pub fn take(&mut self, id: &str) -> Option<PendingPost> {
        let (post, state) = self
            .posts
            .iter_mut()
            .find(|(post, state)| post.id == id && *state == State::Pending)?;
        *state = State::Posting;
        Some(post.clone())
    }

    /// Records how the sending of post `id` ended: its link, or why it failed.
    pub fn finish(&mut self, id: &str, result: Result<String, String>) {
        if let Some((_, state)) = self
            .posts
            .iter_mut()
            .find(|(post, state)| post.id == id && *state == State::Posting)
        {
            *state = match result {
                Ok(link) => State::Posted(link),
                Err(reason) => State::Failed(reason),
            };
        }
    }

    /// Drops the waiting post `id` unsent; false when it was not waiting.
    pub fn reject(&mut self, id: &str) -> bool {
        match self
            .posts
            .iter_mut()
            .find(|(post, state)| post.id == id && *state == State::Pending)
        {
            Some((_, state)) => {
                *state = State::Rejected;
                true
            }
            None => false,
        }
    }
}

struct GlobalPendingPosts(PendingPosts);

impl Global for GlobalPendingPosts {}

/// What the bridge was installed with.
struct Bridge {
    /// The quoted command an agent runs: `'<bongos>' '--slack'`.
    command: String,
    scope_of: ScopeOf,
    /// Whether the `slack` handler is registered on the channel server.
    registered: bool,
}

impl Global for Bridge {}

/// The pending posts, once installed.
pub fn pending_posts(cx: &App) -> Option<&PendingPosts> {
    cx.try_global::<GlobalPendingPosts>()
        .map(|global| &global.0)
}

fn update_pending<R>(cx: &mut App, f: impl FnOnce(&mut PendingPosts) -> R) -> Option<R> {
    if !cx.has_global::<GlobalPendingPosts>() {
        return None;
    }
    let result = cx.update_global::<GlobalPendingPosts, _>(|global, _| f(&mut global.0));
    NeedsYouRegistry::changed(cx);
    Some(result)
}

/// Installs the bridge for an agent that runs bongos as `program` (the executable and its leading
/// arguments), finding a chat's scope through the chat store and the Work tree.
pub fn install(program: &[String], cx: &mut App) {
    if program.is_empty() {
        return;
    }
    let mut words = Vec::new();
    for word in program.iter().chain([&"--slack".to_string()]) {
        match bongos_chat_model::shell_quote(std::path::Path::new(word)) {
            Ok(quoted) => words.push(quoted),
            Err(error) => {
                log::error!("slack: the bongos path cannot be quoted for chats: {error}");
                return;
            }
        }
    }
    let clock = cx
        .background_executor()
        .scheduler_executor()
        .scheduler()
        .clock();
    let session = format!(
        "{:04x}",
        (u64::from(std::process::id()) ^ clock.utc_now().timestamp_millis() as u64) & 0xffff
    );
    install_with(
        words.join(" "),
        Rc::new(chat_scope),
        PendingPosts::new(clock, session),
        cx,
    );
}

/// [`install`] with the quoted `command`, how chat scopes are found and the pending posts.
pub fn install_with(command: String, scope_of: ScopeOf, pending: PendingPosts, cx: &mut App) {
    cx.set_global(GlobalPendingPosts(pending));
    cx.set_global(Bridge {
        command,
        scope_of,
        registered: false,
    });
    bongos_chat::add_context_extension(SOURCE_NAME, SlackContext, cx);
    NeedsYouRegistry::add_source(SOURCE_NAME, Rc::new(SlackNeedsYou), cx);
    sync_handler(cx);
    cx.observe_global::<SettingsStore>(sync_handler).detach();
}

/// Registers the `slack` handler while `slack.enabled` is true and removes it while it is false.
fn sync_handler(cx: &mut App) {
    let enabled = SlackService::is_enabled(cx);
    let Some(registered) = cx.try_global::<Bridge>().map(|bridge| bridge.registered) else {
        return;
    };
    if enabled == registered {
        return;
    }
    let Some(server) = ChannelServer::global(cx) else {
        return;
    };
    let Some(kind) = RequestKind::new(KIND) else {
        return;
    };
    if enabled {
        let sender = spawn_dispatcher(cx);
        server.update(cx, |server, _| {
            server.set_request_handler(kind, move |chat, body| {
                answer_on_thread(&sender, chat, body)
            })
        });
    } else {
        server.update(cx, |server, _| server.remove_request_handler(&kind));
    }
    cx.update_global::<Bridge, _>(|bridge, _| bridge.registered = enabled);
}

struct Job {
    chat: ChatId,
    body: Body,
    reply: std::sync::mpsc::Sender<Value>,
}

/// Runs every job sent on the returned sender on the main thread, until the sender is dropped
/// (with the handler).
fn spawn_dispatcher(cx: &mut App) -> mpsc::UnboundedSender<Job> {
    let (sender, mut receiver) = mpsc::unbounded::<Job>();
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(job) = receiver.next().await {
            cx.update(|cx| {
                let task = handle(job.chat, job.body, cx);
                cx.spawn(async move |_| {
                    job.reply.send(task.await).ok();
                })
                .detach();
            });
        }
    })
    .detach();
    sender
}

/// The socket handler's thread: hands the request to the main thread and waits for its answer.
fn answer_on_thread(sender: &mpsc::UnboundedSender<Job>, chat: ChatId, body: Body) -> Value {
    let (reply, answer) = std::sync::mpsc::channel();
    if sender.unbounded_send(Job { chat, body, reply }).is_err() {
        return Reply::error("bongos is not answering Slack requests").to_value();
    }
    answer
        .recv_timeout(HANDLER_WAIT)
        .unwrap_or_else(|_| Reply::error("bongos did not answer in time").to_value())
}

/// The scope of `chat` from the chat store and the Work tree.
fn chat_scope(chat: &ChatId, cx: &App) -> ChatScope {
    let task_id = bongos_chat::ChatRuntime::global(cx).and_then(|runtime| {
        runtime
            .read(cx)
            .store()
            .read(cx)
            .find(chat.as_str())
            .and_then(|indexed| indexed.task_id.clone())
    });
    let project = task_id.as_deref().and_then(|id| {
        let tree = bongos_work::WorkStore::global(cx)?.read(cx).snapshot()?;
        let task = tree.task_by_id(id)?;
        Some(
            bongos_work::project_repo_of(task, cx)?
                .location()
                .id
                .clone(),
        )
    });
    ChatScope { task_id, project }
}

/// Why no operation can run now, if so.
fn unavailable(cx: &App) -> Option<&'static str> {
    let Some(service) = SlackService::global(cx) else {
        return Some(NOT_CONNECTED);
    };
    if !SlackService::is_enabled(cx) {
        return Some(OFF_MESSAGE);
    }
    match service.read(cx).state(cx) {
        SlackState::Connected(_) => None,
        SlackState::NotConnected => Some(NOT_CONNECTED),
        SlackState::Reconnect(_) => Some(RECONNECT_MESSAGE),
    }
}

/// Answers one `slack` request of `chat`: the reply value the client prints.
pub fn handle(chat: ChatId, body: Body, cx: &mut App) -> Task<Value> {
    let reply = handle_request(chat, body, cx);
    cx.background_spawn(async move { reply.await.to_value() })
}

fn handle_request(chat: ChatId, body: Body, cx: &mut App) -> Task<Reply> {
    let request = match Request::from_body(&body) {
        Ok(request) => request,
        Err(error) => return Task::ready(Reply::error(error)),
    };
    if let Some(message) = unavailable(cx) {
        return Task::ready(Reply::error(message));
    }
    let Some(service) = SlackService::global(cx) else {
        return Task::ready(Reply::error(NOT_CONNECTED));
    };
    match request {
        Request::Channels => {
            let entries = conversation_entries(service, cx);
            cx.background_spawn(async move {
                match entries.await {
                    Ok(entries) => Reply::ok(
                        entries
                            .into_iter()
                            .map(|entry| entry.label)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(error) => Reply::error(error.to_string()),
                }
            })
        }
        Request::Read {
            channel,
            count,
            thread,
        } => read(
            service,
            channel,
            count.clamp(1, bridge::MAX_COUNT),
            thread,
            cx,
        ),
        Request::Post {
            channel,
            thread,
            text,
        } => {
            let scope = cx
                .try_global::<Bridge>()
                .map(|bridge| (bridge.scope_of)(&chat, cx))
                .unwrap_or_default();
            post(service, chat, scope, channel, thread, text, cx)
        }
        Request::PostStatus { id } => Task::ready(Reply::ok(
            pending_posts(cx)
                .map_or(PostStatus::Unknown, |pending| pending.status(&id))
                .to_string(),
        )),
    }
}

/// The picker entry `channel` names: `#name`, `name`, `@person` or a conversation id.
fn find_entry<'a>(
    entries: &'a [ConversationEntry],
    channel: &str,
) -> Option<&'a ConversationEntry> {
    let channel = channel.trim();
    let named = if channel.starts_with('#') || channel.starts_with('@') {
        channel.to_string()
    } else {
        format!("#{channel}")
    };
    entries
        .iter()
        .find(|entry| entry.label == named || entry.id.as_str() == channel)
}

fn no_such_channel() -> Reply {
    Reply::error("No Slack conversation of that name is visible (bongos --slack channels)")
}

fn read(
    service: Entity<SlackService>,
    channel: String,
    count: usize,
    thread: Option<String>,
    cx: &mut App,
) -> Task<Reply> {
    let entries = conversation_entries(service.clone(), cx);
    cx.spawn(async move |cx| {
        let entries = match entries.await {
            Ok(entries) => entries,
            Err(error) => return Reply::error(error.to_string()),
        };
        let Some(entry) = find_entry(&entries, &channel) else {
            return no_such_channel();
        };
        let id: ConversationId = entry.id.clone();
        let messages = match thread {
            Some(thread) => {
                let Ok(thread) = Ts::parse(&thread) else {
                    return Reply::error("not a Slack message timestamp");
                };
                match replies_of(&service, &id, &thread, cx).await {
                    Ok(mut messages) => {
                        let skip = messages.len().saturating_sub(count);
                        messages.drain(..skip);
                        messages
                    }
                    Err(error) => return Reply::error(error.to_string()),
                }
            }
            None => {
                let page = service
                    .update(cx, |service, cx| {
                        service.history(id.clone(), None, count, cx)
                    })
                    .await;
                match page {
                    Ok(page) => page.items.into_iter().rev().collect(),
                    Err(error) => return Reply::error(error.to_string()),
                }
            }
        };
        resolve_authors(&service, &messages, cx).await;
        cx.update(|cx| {
            let Some(conversation) = service
                .read(cx)
                .cached_conversations()
                .and_then(|list| list.iter().find(|conversation| conversation.id == id))
                .cloned()
            else {
                return no_such_channel();
            };
            let workspace = connection::current(cx)
                .map(|connection| connection.workspace)
                .unwrap_or_default();
            let service = service.read(cx);
            Reply::ok(quote::render(
                &conversation,
                &messages,
                &workspace,
                &service.user_names(),
                &*service.zone(),
            ))
        })
    })
}

/// The allowed list of `project`; empty without a project.
fn allowed_for(project: Option<&str>, cx: &App) -> AllowedList {
    project.map_or_else(AllowedList::default, |project| {
        allowed::load_in(&connection::installed_dir(cx), project)
    })
}

fn post(
    service: Entity<SlackService>,
    chat: ChatId,
    scope: ChatScope,
    channel: String,
    thread: Option<String>,
    text: String,
    cx: &mut App,
) -> Task<Reply> {
    let thread = match thread.as_deref().map(Ts::parse).transpose() {
        Ok(thread) => thread,
        Err(_) => return Task::ready(Reply::error("not a Slack message timestamp")),
    };
    let escaped = match escaped_text(&text) {
        Ok(escaped) => escaped,
        Err(message) => return Task::ready(Reply::error(message)),
    };
    let entries = conversation_entries(service.clone(), cx);
    cx.spawn(async move |cx| {
        let entries = match entries.await {
            Ok(entries) => entries,
            Err(error) => return Reply::error(error.to_string()),
        };
        let Some(entry) = find_entry(&entries, &channel) else {
            return no_such_channel();
        };
        let name = match &thread {
            Some(_) => thread_name(&entry.label),
            None => entry.label.clone(),
        };
        let destination = Destination {
            conversation: entry.id.clone(),
            thread,
        };
        let allowed = cx.update(|cx| allowed_for(scope.project.as_deref(), cx));
        match decide(Origin::ChatAgent, &destination, &allowed) {
            Decision::Ask => {
                let id = cx.update(|cx| {
                    update_pending(cx, |pending| {
                        pending.add(destination, name.clone(), text.clone(), chat, scope.task_id)
                    })
                });
                match id {
                    Some(id) => Reply::ok(waiting_text(&id, &name, &text)),
                    None => Reply::error(NOT_CONNECTED),
                }
            }
            Decision::Send => {
                let sent = service.update(cx, |service, cx| {
                    service.post(destination.clone(), escaped, Origin::ChatAgent, cx)
                });
                match sent.await {
                    Ok(posted) => {
                        let link = cx.update(|cx| link_of(&destination, &posted.ts, cx));
                        Reply::ok(posted_text(&name, &link))
                    }
                    Err(error) => Reply::error(error.to_string()),
                }
            }
        }
    })
}

/// The link of the message `ts` posted to `destination`.
fn link_of(destination: &Destination, ts: &Ts, cx: &App) -> String {
    let url = connection::current(cx)
        .map(|connection| connection.workspace_url)
        .unwrap_or_default();
    render::message_link(
        &url,
        &destination.conversation,
        ts,
        destination.thread.as_ref(),
    )
}

/// Kyle approves the waiting post `id`: its exact stored text is sent once. Answers what to show.
pub fn approve(id: &str, cx: &mut App) -> Task<String> {
    let Some(post) = update_pending(cx, |pending| pending.take(id)).flatten() else {
        return Task::ready(format!(
            "Slack post {id} is no longer waiting; nothing was sent"
        ));
    };
    if question_hash(&post.destination, &post.text) != post.hash {
        update_pending(cx, |pending| {
            pending.finish(
                &post.id,
                Err("the post changed after it was asked".to_string()),
            )
        });
        return Task::ready("The Slack post changed after it was asked; nothing was sent".into());
    }
    let Some(service) = SlackService::global(cx) else {
        update_pending(cx, |pending| {
            pending.finish(&post.id, Err(NOT_CONNECTED.to_string()))
        });
        return Task::ready(NOT_CONNECTED.to_string());
    };
    let text = render::escape_for_post(&post.text);
    let sent = service.update(cx, |service, cx| {
        service.post(post.destination.clone(), text, Origin::ChatAgent, cx)
    });
    cx.spawn(async move |cx| {
        let result = sent.await;
        cx.update(|cx| match result {
            Ok(posted) => {
                let link = link_of(&post.destination, &posted.ts, cx);
                update_pending(cx, |pending| pending.finish(&post.id, Ok(link.clone())));
                posted_text(&post.name, &link)
            }
            Err(error) => {
                let reason = error.to_string();
                update_pending(cx, |pending| pending.finish(&post.id, Err(reason.clone())));
                format!("Slack did not post to {}: {reason}", post.name)
            }
        })
    })
}

/// Kyle rejects the waiting post `id`: it is dropped unsent. Answers what to show.
pub fn reject(id: &str, cx: &mut App) -> String {
    match update_pending(cx, |pending| pending.reject(id)) {
        Some(true) => format!("Rejected Slack post {id}; nothing was sent"),
        _ => format!("Slack post {id} is no longer waiting"),
    }
}

/// The `## Slack` section of a chat's context, while Slack is on and connected.
struct SlackContext;

impl ContextExtension for SlackContext {
    fn section(&self, _chat_type: &str, _task_id: Option<&str>, cx: &App) -> Option<String> {
        context_section(cx)
    }
}

/// The section [`SlackContext`] adds, or `None` while Slack is off or not connected.
pub fn context_section(cx: &App) -> Option<String> {
    if unavailable(cx).is_some() {
        return None;
    }
    let command = &cx.try_global::<Bridge>()?.command;
    Some(bridge::context_section(command))
}

/// Every waiting post of a task needs Kyle.
struct SlackNeedsYou;

impl NeedsYouSource for SlackNeedsYou {
    fn needs_you(&self, cx: &App) -> Vec<NeedsYou> {
        if !SlackSettings::try_get(cx).is_some_and(|settings| settings.enabled) {
            return Vec::new();
        }
        pending_posts(cx)
            .map(|pending| {
                pending
                    .waiting()
                    .into_iter()
                    .filter_map(|post| {
                        Some(NeedsYou {
                            task_id: post.task_id.clone()?,
                            reason: needs_you_reason(&post.name),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The picker line of a waiting post: where, and its first line.
fn picker_label(post: &PendingPost) -> String {
    let first = post.text.lines().next().unwrap_or_default();
    let first: String = first.chars().take(60).collect();
    format!("{}: {first} ({})", post.name, post.id)
}

/// `slack: review pending posts`: a picker over the waiting posts, then the exact text in a
/// read-only buffer and the question; yes posts it, no rejects it.
pub(crate) fn review_pending_posts(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let waiting: Vec<PendingPost> = pending_posts(cx)
        .map(|pending| pending.waiting().into_iter().cloned().collect())
        .unwrap_or_default();
    if waiting.is_empty() {
        show_toast(workspace, NO_PENDING.to_string(), cx);
        return;
    }
    let candidates = waiting
        .into_iter()
        .map(|post| Candidate::new(picker_label(&post), post))
        .collect();
    let weak = cx.entity().downgrade();
    let session = Session::completing_read(HOST, "Review Slack post: ")
        .source(Source::Fixed(candidates))
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(post) = value.downcast_ref::<PendingPost>() else {
                return;
            };
            let post = post.clone();
            weak.update(cx, |workspace, cx| preview(workspace, post, window, cx))
                .ok();
        });
    open(workspace, session, window, cx);
}

/// Shows the exact text of `post` read-only, asks whether to post it, then closes the preview.
fn preview(
    workspace: &mut Workspace,
    post: PendingPost,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the preview".to_string(), cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(post.text.clone(), cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                let title = format!("Slack post to {}", post.name);
                let editor = add_read_only_editor(workspace, buffer, title, window, cx);
                let item = editor.entity_id();
                let weak = cx.entity().downgrade();
                bongos_minibuffer::yes_or_no(
                    workspace,
                    HOST,
                    bridge::approval_question(&post.name),
                    false,
                    move |yes, window, cx| {
                        weak.update(cx, |workspace, cx| {
                            for pane in workspace.panes().to_vec() {
                                pane.update(cx, |pane, cx| {
                                    pane.close_item_by_id(item, SaveIntent::Skip, window, cx)
                                        .detach_and_log_err(cx)
                                });
                            }
                            if yes {
                                let task = approve(&post.id, cx);
                                cx.spawn(async move |workspace, cx| {
                                    let message = task.await;
                                    workspace
                                        .update(cx, |workspace, cx| {
                                            show_toast(workspace, message, cx)
                                        })
                                        .ok();
                                })
                                .detach();
                            } else {
                                let message = reject(&post.id, cx);
                                show_toast(workspace, message, cx);
                            }
                        })
                        .ok();
                    },
                    window,
                    cx,
                );
            })
            .ok();
    })
    .detach();
}
