//! The model behind a Slack read buffer (`Slack: #name`) and the conversation picker, without a
//! window: [`ReadSession`] owns the [`ReadView`], the text buffer it renders into and the row
//! index, loads pages through the [`SlackService`] (so rate-limit waits, caches and the token
//! rules apply), shows the service's wait in the header while a load runs, and refuses to load
//! once Slack is disconnected. `read_commands` wires it to the editor.
//!
//! Nothing here opens, fetches or follows anything a message contains: the requests are the
//! conversation list, the user names of the authors on a page, one history page and the replies
//! of at most [`MAX_THREADS_PER_PAGE`](crate::read_view::MAX_THREADS_PER_PAGE) threads.

use std::collections::HashMap;
use std::fmt;

use bongos_slack_model::render::{Transcript, UserNames};
use bongos_slack_model::types::{Conversation, ConversationId, Message, Page, Ts, UserId};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EntityId, Global, Subscription, Task};
use language::{Buffer, Capability};

use crate::connection;
use crate::read_view::{
    LoadError, MessageLink, PAGE_SIZE, ReadView, conversation_label, row_of, thread_at,
    thread_parents, thread_transcript_of,
};
use crate::service::{Answer, Read, SlackError, SlackService};

/// The most unknown users looked up for one page or one picker.
const MAX_USER_LOOKUPS: usize = 50;
/// The most pages of one thread's replies that are read.
const MAX_REPLY_PAGES: usize = 20;

/// Why a read buffer did not load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    Load(LoadError),
    Slack(SlackError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(f, "{error}"),
            Self::Slack(error) => write!(f, "{error}"),
        }
    }
}

impl From<SlackError> for SessionError {
    fn from(error: SlackError) -> Self {
        Self::Slack(error)
    }
}

/// One line of the conversation picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationEntry {
    /// `#name`, `@user` or `@a, @b`.
    pub label: String,
    pub id: ConversationId,
}

/// The picker lines for `conversations`, ordered by label.
pub fn picker_entries(conversations: &[Conversation], users: &UserNames) -> Vec<ConversationEntry> {
    let mut entries: Vec<ConversationEntry> = conversations
        .iter()
        .map(|conversation| ConversationEntry {
            label: conversation_label(conversation, users),
            id: conversation.id.clone(),
        })
        .collect();
    entries.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.0.cmp(&b.id.0)));
    entries
}

/// The conversation list for the picker, in picker order. The service rereads the list when its
/// copy is older than five minutes (by its injected clock); the names of direct message partners
/// not seen yet are looked up.
pub fn conversation_entries(
    service: Entity<SlackService>,
    cx: &mut App,
) -> Task<Result<Vec<ConversationEntry>, SlackError>> {
    cx.spawn(async move |cx| {
        let conversations = ask(&service, cx, |service, cx| service.conversation_list(cx)).await?;
        let known = service.read_with(cx, |service, _| service.user_names());
        let unknown: Vec<UserId> = conversations
            .iter()
            .filter_map(|conversation| conversation.user.clone())
            .filter(|user| known.get(&user.0).is_none())
            .take(MAX_USER_LOOKUPS)
            .collect();
        for user in unknown {
            // A name that cannot be read leaves the id in the label.
            ask(&service, cx, |service, cx| service.user(user, cx))
                .await
                .ok();
        }
        let users = service.read_with(cx, |service, _| service.user_names());
        Ok(picker_entries(&conversations, &users))
    })
}

/// Runs `f` on the service and waits for the task it returns.
async fn ask<T: 'static>(
    service: &Entity<SlackService>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut SlackService, &mut Context<SlackService>) -> Task<Result<T, SlackError>>,
) -> Result<T, SlackError> {
    service.update(cx, |service, cx| f(service, cx)).await
}

/// Looks up the authors of `messages` that are not known yet, so the text shows their names.
async fn resolve_authors(service: &Entity<SlackService>, messages: &[Message], cx: &mut AsyncApp) {
    let known = service.read_with(cx, |service, _| service.user_names());
    let mut unknown: Vec<UserId> = Vec::new();
    for user in messages.iter().filter_map(|message| message.user.clone()) {
        if known.get(&user.0).is_none() && !unknown.contains(&user) {
            unknown.push(user);
        }
    }
    unknown.truncate(MAX_USER_LOOKUPS);
    for user in unknown {
        ask(service, cx, |service, cx| service.user(user, cx))
            .await
            .ok();
    }
}

/// Every message of a thread, its parent first, through the service.
async fn replies_of(
    service: &Entity<SlackService>,
    channel: &ConversationId,
    thread_ts: &Ts,
    cx: &mut AsyncApp,
) -> Result<Vec<Message>, SlackError> {
    let mut all = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_REPLY_PAGES {
        let read = Read::Replies {
            channel: channel.clone(),
            thread_ts: thread_ts.clone(),
            cursor: cursor.take(),
        };
        let answer = service
            .update(cx, |service, cx| service.call(read, cx))
            .await?;
        let Answer::Messages(page) = answer else {
            unreachable!("conversations.replies answers messages");
        };
        all.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(all)
}

/// `slack: read thread`: the text of the thread `thread_ts` of `channel`, its parent first.
pub fn read_thread(
    service: Entity<SlackService>,
    channel: ConversationId,
    thread_ts: Ts,
    cx: &mut App,
) -> Task<Result<Transcript, SlackError>> {
    cx.spawn(async move |cx| {
        let messages = replies_of(&service, &channel, &thread_ts, cx).await?;
        resolve_authors(&service, &messages, cx).await;
        let (users, zone) =
            service.read_with(cx, |service, _| (service.user_names(), service.zone()));
        Ok(thread_transcript_of(messages, &thread_ts, &users, &*zone))
    })
}

/// How the thread buffer of a message link is titled: its conversation as the picker names it.
pub fn link_label(link: &MessageLink, entries: &[ConversationEntry]) -> String {
    entries
        .iter()
        .find(|entry| entry.id == link.conversation)
        .map_or_else(
            || link.conversation.to_string(),
            |entry| entry.label.clone(),
        )
}

/// The read buffers that are open, by their buffer.
#[derive(Default)]
struct Sessions(HashMap<EntityId, Entity<ReadSession>>);

impl Global for Sessions {}

/// The state of one `Slack: #name` buffer.
pub struct ReadSession {
    service: Entity<SlackService>,
    buffer: Entity<Buffer>,
    view: ReadView,
    transcript: Transcript,
    loading: bool,
    _subscriptions: Vec<Subscription>,
}

impl ReadSession {
    /// A read buffer for `channel`, shown in `buffer` (made read-only here). Nothing is loaded
    /// yet: call [`Self::load_first`]. The session is found again with [`Self::for_buffer`] for
    /// as long as the buffer lives.
    pub fn open(
        service: Entity<SlackService>,
        buffer: Entity<Buffer>,
        channel: ConversationId,
        label: String,
        cx: &mut App,
    ) -> Entity<Self> {
        buffer.update(cx, |buffer, cx| {
            buffer.set_capability(Capability::ReadOnly, cx)
        });
        let session = cx.new(|cx| {
            let subscriptions = vec![
                // A rate-limit wait shows in the header while a load runs.
                cx.observe(&service, |this: &mut Self, service, cx| {
                    if this.loading {
                        let wait = service.read(cx).wait_left();
                        this.view.set_waiting(wait);
                        this.refresh(cx);
                    }
                }),
                cx.observe_global::<connection::SlackConnection>(|this: &mut Self, cx| {
                    if connection::current(cx).is_none() && !this.view.is_disconnected() {
                        this.view.disconnect();
                        this.refresh(cx);
                    }
                }),
            ];
            let mut session = Self {
                service,
                buffer: buffer.clone(),
                view: ReadView::new(channel, label),
                transcript: Transcript {
                    text: String::new(),
                    index: Vec::new(),
                },
                loading: false,
                _subscriptions: subscriptions,
            };
            session.refresh(cx);
            session
        });
        let id = buffer.entity_id();
        cx.default_global::<Sessions>()
            .0
            .insert(id, session.clone());
        cx.observe_release(&buffer, move |_, cx| {
            cx.default_global::<Sessions>().0.remove(&id);
        })
        .detach();
        session
    }

    /// The session whose text is `buffer`.
    pub fn for_buffer(buffer: &Entity<Buffer>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Sessions>()?
            .0
            .get(&buffer.entity_id())
            .cloned()
    }

    /// Every read buffer open now.
    pub fn open_sessions(cx: &App) -> Vec<Entity<Self>> {
        cx.try_global::<Sessions>()
            .map(|sessions| sessions.0.values().cloned().collect())
            .unwrap_or_default()
    }

    /// The buffer title, `Slack: #name`.
    pub fn title(&self) -> String {
        self.view.title()
    }

    pub fn channel(&self) -> &ConversationId {
        self.view.channel()
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn view(&self) -> &ReadView {
        &self.view
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// The thread of the message at buffer row `row` (`slack: read thread` at point).
    pub fn thread_at(&self, row: usize) -> Option<Ts> {
        thread_at(&self.transcript, row)
    }

    /// Renders the view into the buffer.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let (users, zone) = {
            let service = self.service.read(cx);
            (service.user_names(), service.zone())
        };
        self.transcript = self.view.render(&users, &*zone);
        let text = self.transcript.text.clone();
        self.buffer.update(cx, |buffer, cx| {
            if buffer.text() != text {
                buffer.set_text(text, cx);
            }
        });
        cx.notify();
    }

    /// Reads the newest page.
    pub fn load_first(&mut self, cx: &mut Context<Self>) -> Task<Result<(), SessionError>> {
        let task = self.load(false, None, cx);
        cx.spawn(async move |_, _| task.await.map(|_| ()))
    }

    /// `slack: load older messages` with point on buffer row `row`: reads the page before the
    /// oldest message shown and answers the row the message that was at `row` is on now, so point
    /// stays on it. A disconnected buffer, or one at the start of the conversation, loads nothing.
    pub fn load_older(
        &mut self,
        row: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<usize>, SessionError>> {
        self.load(true, Some(row), cx)
    }

    fn load(
        &mut self,
        older: bool,
        row: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<usize>, SessionError>> {
        let latest = if older {
            self.view.older_request()
        } else if self.view.is_disconnected() {
            Err(LoadError::Disconnected)
        } else {
            Ok(None)
        };
        let latest = match latest {
            Ok(latest) => latest,
            Err(error) => return Task::ready(Err(SessionError::Load(error))),
        };
        let keep = row
            .and_then(|row| self.transcript.at(row))
            .map(|(ts, _)| ts.clone());
        self.loading = true;
        let service = self.service.clone();
        let channel = self.view.channel().clone();
        cx.spawn(async move |this, cx| {
            let fetched = fetch(&service, channel, latest, cx).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match fetched {
                    Ok((page, replies)) => {
                        this.view.apply_page(page, replies);
                        this.refresh(cx);
                        Ok(keep.and_then(|ts| row_of(&this.transcript, &ts)))
                    }
                    Err(error) => {
                        this.view.set_waiting(None);
                        this.refresh(cx);
                        Err(SessionError::Slack(error))
                    }
                }
            })
            .map_err(|_| SessionError::Slack(SlackError::NotConnected))?
        })
    }
}

type Fetched = (Page<Message>, Vec<(Ts, Vec<Message>)>);

/// One history page, the replies of its first threads and the names of its authors.
async fn fetch(
    service: &Entity<SlackService>,
    channel: ConversationId,
    latest: Option<Ts>,
    cx: &mut AsyncApp,
) -> Result<Fetched, SlackError> {
    let page = {
        let channel = channel.clone();
        ask(service, cx, move |service, cx| {
            service.history(channel, latest, PAGE_SIZE, cx)
        })
        .await?
    };
    let mut replies = Vec::new();
    for parent in thread_parents(&page) {
        match replies_of(service, &channel, &parent, cx).await {
            Ok(messages) => replies.push((parent, messages)),
            // A thread that cannot be read keeps its hint; a refused token stops the load.
            Err(error @ (SlackError::Reconnect | SlackError::NotConnected | SlackError::Off)) => {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    resolve_authors(service, &page.items, cx).await;
    Ok((page, replies))
}
