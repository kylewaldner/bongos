//! The state of a Slack read buffer (`Slack: #name`): the messages shown keyed by timestamp, the
//! replies fetched for threads, whether older messages exist, the wait and disconnected lines, and
//! the row index that maps buffer rows to `(ts, thread_ts)` for `slack: reply` and the destination
//! picker.
//!
//! The text is built by `render::transcript`. Messages are shown as Slack sent them: loading a
//! page never opens, fetches or follows anything in a message; the only requests are the history
//! page and the replies of at most [`MAX_THREADS_PER_PAGE`] threads on it.

use std::collections::BTreeMap;
use std::time::Duration;

use bongos_slack_model::render::{self, Clock, RowRef, Transcript, UserNames};
use bongos_slack_model::types::{
    Conversation, ConversationId, ConversationKind, Cursor, Destination, Message, Page, Ts,
};
use bongos_slack_model::{ApiError, SlackApi};

/// The messages of one history page.
pub const PAGE_SIZE: usize = 50;
/// The most threads whose replies are fetched for one history page; the rest show a hint.
pub const MAX_THREADS_PER_PAGE: usize = 10;
/// The most pages of one thread's replies that are read.
const MAX_REPLY_PAGES: usize = 20;
/// What a disconnected buffer shows on its second line.
pub const DISCONNECTED_LINE: &str = "disconnected (slack: connect to read again)";

/// Why a load did not happen or failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// `slack: disconnect` ran: the buffer loads nothing more.
    Disconnected,
    /// This is the start of the conversation: there is nothing older.
    NothingOlder,
    Api(ApiError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => f.write_str("Slack is disconnected; this buffer loads no more"),
            Self::NothingOlder => f.write_str("this is the start of the conversation"),
            Self::Api(cause) => write!(f, "{cause}"),
        }
    }
}

/// How a conversation is named in the picker and the buffer title: `#name`, `@user` for a direct
/// message and `@a, @b` for a group direct message.
pub fn conversation_label(conversation: &Conversation, users: &UserNames) -> String {
    match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => {
            format!("#{}", conversation.name)
        }
        ConversationKind::Dm => {
            let name = conversation
                .user
                .as_ref()
                .map(|user| users.get(&user.0).unwrap_or(&user.0).to_string())
                .unwrap_or_else(|| conversation.id.to_string());
            format!("@{name}")
        }
        ConversationKind::GroupDm => {
            // Slack names a group direct message `mpdm-a--b--c-1`.
            let inner = conversation
                .name
                .strip_prefix("mpdm-")
                .unwrap_or(&conversation.name);
            let inner = match inner.rsplit_once('-') {
                Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) => head,
                _ => inner,
            };
            inner
                .split("--")
                .filter(|name| !name.is_empty())
                .map(|name| format!("@{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// The buffer title for a conversation.
pub fn buffer_title(label: &str) -> String {
    format!("Slack: {label}")
}

/// The conversation, message and thread a message link points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageLink {
    pub conversation: ConversationId,
    pub ts: Ts,
    pub thread_ts: Option<Ts>,
}

impl MessageLink {
    /// Parses `https://<workspace>/archives/<channel>/p<ts without the dot>[?thread_ts=..]`.
    /// Nothing is fetched; the link only names a message.
    pub fn parse(link: &str) -> Option<Self> {
        let url = url::Url::parse(link.trim()).ok()?;
        let mut parts = url.path_segments()?;
        if parts.next()? != "archives" {
            return None;
        }
        let conversation = ConversationId::parse(parts.next()?).ok()?;
        let compact = parts.next()?.strip_prefix('p')?;
        if compact.len() <= 6 || !compact.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let (seconds, fraction) = compact.split_at(compact.len() - 6);
        let ts = Ts::parse(&format!("{seconds}.{fraction}")).ok()?;
        let thread_ts = url
            .query_pairs()
            .find(|(key, _)| key == "thread_ts")
            .and_then(|(_, value)| Ts::parse(&value).ok());
        Some(Self {
            conversation,
            ts,
            thread_ts,
        })
    }

    /// The thread the link points into: its thread, else the message itself.
    pub fn thread(&self) -> Ts {
        self.thread_ts.clone().unwrap_or_else(|| self.ts.clone())
    }
}

/// The messages of one conversation as a read buffer shows them.
#[derive(Clone, Debug)]
pub struct ReadView {
    channel: ConversationId,
    label: String,
    messages: BTreeMap<Ts, Message>,
    threads: render::Threads,
    /// The cursor of the page older than the oldest shown, when older messages exist.
    older: Option<Cursor>,
    waiting: Option<Duration>,
    disconnected: bool,
}

impl ReadView {
    pub fn new(channel: ConversationId, label: String) -> Self {
        Self {
            channel,
            label,
            messages: BTreeMap::new(),
            threads: render::Threads::default(),
            older: None,
            waiting: None,
            disconnected: false,
        }
    }

    pub fn channel(&self) -> &ConversationId {
        &self.channel
    }

    /// The buffer title, `Slack: #name`.
    pub fn title(&self) -> String {
        buffer_title(&self.label)
    }

    /// How many messages are shown (not counting replies).
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The timestamp of the oldest message shown.
    pub fn oldest(&self) -> Option<&Ts> {
        self.messages.keys().next()
    }

    pub fn older_exist(&self) -> bool {
        self.older.is_some()
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// `slack: disconnect` ran: the buffer shows "disconnected" and refuses to load more.
    pub fn disconnect(&mut self) {
        self.disconnected = true;
    }

    /// Slack asked to wait `wait` (rounded up to whole seconds), or the wait is over (`None`).
    pub fn set_waiting(&mut self, wait: Option<Duration>) {
        self.waiting = wait;
    }

    /// Reads the newest page.
    pub async fn load_first(&mut self, api: &dyn SlackApi) -> Result<(), LoadError> {
        self.load(api, None).await
    }

    /// Reads one page older than the oldest message shown and merges it by `ts`.
    pub async fn load_older(&mut self, api: &dyn SlackApi) -> Result<(), LoadError> {
        if self.disconnected {
            return Err(LoadError::Disconnected);
        }
        let Some(oldest) = self.oldest().cloned() else {
            return self.load_first(api).await;
        };
        if self.older.is_none() {
            return Err(LoadError::NothingOlder);
        }
        self.load(api, Some(oldest)).await
    }

    async fn load(&mut self, api: &dyn SlackApi, latest: Option<Ts>) -> Result<(), LoadError> {
        if self.disconnected {
            return Err(LoadError::Disconnected);
        }
        let page = api
            .history(self.channel.clone(), latest, PAGE_SIZE)
            .await
            .map_err(LoadError::Api)?;
        // Replies for the first threads of the page, newest first; the rest keep their hint.
        let parents: Vec<Ts> = page
            .items
            .iter()
            .filter(|message| message.reply_count > 0 && is_parent(message))
            .take(MAX_THREADS_PER_PAGE)
            .map(|message| message.ts.clone())
            .collect();
        for parent in parents {
            match read_thread(api, &self.channel, &parent).await {
                Ok(replies) => {
                    self.threads.insert(parent, replies);
                }
                // A thread that cannot be read keeps its hint; a refused token stops the load.
                Err(error @ (ApiError::Unauthorized { .. } | ApiError::NotConnected)) => {
                    return Err(LoadError::Api(error));
                }
                Err(_) => {}
            }
        }
        self.older = page.next;
        for message in page.items {
            self.messages.insert(message.ts.clone(), message);
        }
        self.waiting = None;
        Ok(())
    }

    /// The buffer text and its row index: the transcript with the wait and disconnected lines
    /// after the header. Inserted lines belong to no message.
    pub fn render(&self, users: &UserNames, clock: &dyn Clock) -> Transcript {
        let page = Page {
            items: self.messages.values().cloned().collect(),
            next: self.older.clone(),
        };
        let mut transcript = render::transcript(&[page], users, &self.threads, clock);
        let mut extra = Vec::new();
        if let Some(wait) = self.waiting {
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            extra.push(format!("Slack asked to wait {seconds} seconds"));
        }
        if self.disconnected {
            extra.push(DISCONNECTED_LINE.to_string());
        }
        if !extra.is_empty() {
            let mut lines: Vec<&str> = transcript.text.split('\n').collect();
            let at = 1.min(lines.len());
            for (offset, line) in extra.iter().enumerate() {
                lines.insert(at + offset, line);
                transcript.index.insert(at + offset, None);
            }
            transcript.text = lines.join("\n");
        }
        transcript
    }
}

/// A message that starts a thread (not a reply in one).
fn is_parent(message: &Message) -> bool {
    message
        .thread_ts
        .as_ref()
        .is_none_or(|thread| *thread == message.ts)
}

/// Every message of a thread, its parent first, as `conversations.replies` answers.
pub async fn read_thread(
    api: &dyn SlackApi,
    channel: &ConversationId,
    thread_ts: &Ts,
) -> Result<Vec<Message>, ApiError> {
    let mut all = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_REPLY_PAGES {
        let page = api
            .replies(channel.clone(), thread_ts.clone(), cursor.take())
            .await?;
        all.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(all)
}

/// `slack: read thread`: the text of the thread `thread_ts` as a buffer, its parent first. A
/// thread's parent is the message the buffer shows; the replies follow it.
pub async fn thread_transcript(
    api: &dyn SlackApi,
    channel: &ConversationId,
    thread_ts: &Ts,
    users: &UserNames,
    clock: &dyn Clock,
) -> Result<Transcript, ApiError> {
    let messages = read_thread(api, channel, thread_ts).await?;
    let parent: Vec<Message> = messages
        .iter()
        .filter(|message| message.ts == *thread_ts)
        .cloned()
        .collect();
    let mut threads = render::Threads::default();
    threads.insert(thread_ts.clone(), messages);
    let page = Page {
        items: parent,
        next: None,
    };
    Ok(render::transcript(&[page], users, &threads, clock))
}

/// The thread at buffer row `row`, from the row index: the row's thread, else the message
/// itself. For `slack: read thread` at point.
pub fn thread_at(transcript: &Transcript, row: usize) -> Option<Ts> {
    let (ts, thread) = transcript.at(row)?;
    Some(thread.clone().unwrap_or_else(|| ts.clone()))
}

/// Where `slack: reply` at buffer row `row` posts: the conversation and the thread of the message
/// there (a message with no thread starts one).
pub fn reply_destination(
    channel: &ConversationId,
    transcript: &Transcript,
    row: usize,
) -> Option<Destination> {
    Some(Destination {
        conversation: channel.clone(),
        thread: Some(thread_at(transcript, row)?),
    })
}

/// The first buffer row of the message `ts`, to keep point on it after a load.
pub fn row_of(transcript: &Transcript, ts: &Ts) -> Option<usize> {
    transcript
        .index
        .iter()
        .position(|row: &Option<RowRef>| row.as_ref().is_some_and(|(row_ts, _)| row_ts == ts))
}
