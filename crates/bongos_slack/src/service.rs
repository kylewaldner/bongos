//! [`SlackService`]: the one place requests to Slack come from.
//!
//! The service holds the client (built from the token by the installed
//! [`ClientFactory`](crate::client::ClientFactory) on the first request of a session), the
//! rate-limit wait, the caches and the post queue. The connection itself is
//! [`connection`]'s: the service never reads `connection.toml` again.
//!
//! - While `slack.enabled` is false or nothing is connected, no request is made.
//! - On a rate limit (`RateLimited { retry_after }`) the service records one shared wait, measured
//!   with the injected [`Clock`] and ended by the background executor's timer, then repeats the
//!   request. Every request made during the wait queues behind the same `until`; a second limit
//!   never adds to it. A request whose wait would end more than [`MAX_WAIT`] after it began fails
//!   with the local time to try again. The wait lives in memory only, so it dies with the process.
//! - A read of the same request within [`READ_REUSE`] returns the last answer. User names and the
//!   conversation list are kept for the session; the list is read again for a picker when it is
//!   older than [`LIST_MAX_AGE`].
//! - Posts run one at a time, in the order they were asked for. A post is repeated only after a
//!   rate limit (Slack documents a limited request as not processed); a network failure, a timeout
//!   or a server error is final, because the post may have been sent.
//! - `Unauthorized` moves the state to reconnect, and every caller gets the same message.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bongos_slack_model::render::{self, UserNames};
use bongos_slack_model::types::{Conversation, ConversationId, Cursor, Message, Page, Ts, UserId};
use bongos_slack_model::{
    ApiError, Destination, Origin, PostRequest, PostedMessage, SlackApi, User,
};
use chrono::{DateTime, Local, Offset as _, TimeZone as _, Utc};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, Subscription, Task, WeakEntity,
};
use scheduler::{Clock, Instant};
use settings::Settings as _;

use crate::SlackSettings;
use crate::client;
use crate::connection::{self, Connection, SlackConnection};

/// The longest a request waits on a rate limit, from when it began.
pub const MAX_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long the answer of a read is reused for the same read.
pub const READ_REUSE: Duration = Duration::from_secs(5);
/// How old the conversation list may be when a picker opens.
pub const LIST_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// The most pages of the conversation list read for a picker.
const MAX_LIST_PAGES: usize = 50;

/// The time zone the service shows times in: the machine's local zone.
pub struct LocalZone;

impl render::Clock for LocalZone {
    fn utc_offset_seconds(&self, unix_seconds: i64) -> i32 {
        Utc.timestamp_opt(unix_seconds, 0)
            .single()
            .map_or(0, |time| {
                Local
                    .offset_from_utc_datetime(&time.naive_utc())
                    .fix()
                    .local_minus_utc()
            })
    }
}

/// Where Slack stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackState {
    NotConnected,
    Connected(Connection),
    /// Slack rejected the stored token, or it is missing: `slack: connect` again.
    Reconnect(Connection),
}

/// Why a request was not made or failed. No variant holds a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackError {
    Off,
    NotConnected,
    Reconnect,
    /// The rate-limit wait would end more than [`MAX_WAIT`] after the request began.
    RateLimit {
        /// The local time to try again, `HH:MM`.
        retry_at: String,
    },
    /// A read failed with something other than a rate limit.
    Read {
        channel: Option<ConversationId>,
        cause: ApiError,
    },
    /// The post's answer never came: it may or may not have been sent.
    PostMaybeNotSent {
        channel: ConversationId,
        cause: ApiError,
    },
    /// Slack answered that it did not post.
    PostRefused {
        channel: ConversationId,
        cause: ApiError,
    },
    /// The credential store failed when the token was read; the cause is redacted.
    SecretStore(String),
    /// No Slack client is installed.
    NoClient,
}

/// The message of [`SlackError::Off`].
pub const OFF_MESSAGE: &str = "Slack is off (slack.enabled)";
/// The message of [`SlackError::NotConnected`].
pub const NOT_CONNECTED_MESSAGE: &str = "Slack is not connected";
/// The message of [`SlackError::Reconnect`].
pub const RECONNECT_MESSAGE: &str = "Slack rejected the token; reconnect Slack (slack: connect)";
/// What a post whose answer never came says.
pub const MAY_NOT_HAVE_BEEN_SENT: &str =
    "the post may not have been sent; check the channel before posting again";

impl fmt::Display for SlackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str(OFF_MESSAGE),
            Self::NotConnected => f.write_str(NOT_CONNECTED_MESSAGE),
            Self::Reconnect => f.write_str(RECONNECT_MESSAGE),
            Self::RateLimit { retry_at } => {
                write!(f, "Slack rate limit, try again at {retry_at}")
            }
            Self::Read {
                channel: Some(channel),
                cause,
            } => write!(f, "Could not read {channel} from Slack: {cause}"),
            Self::Read {
                channel: None,
                cause,
            } => write!(f, "Could not read from Slack: {cause}"),
            Self::PostMaybeNotSent { channel, cause } => {
                write!(
                    f,
                    "Posting to {channel} failed ({cause}): {MAY_NOT_HAVE_BEEN_SENT}"
                )
            }
            Self::PostRefused { channel, cause } => {
                write!(f, "Slack did not post to {channel}: {cause}")
            }
            Self::SecretStore(cause) => write!(
                f,
                "Could not read the Slack token from the secret store ({cause})"
            ),
            Self::NoClient => f.write_str("The Slack client is not available"),
        }
    }
}

impl std::error::Error for SlackError {}

/// One read of the Slack Web API. Two equal reads are the same request: the 5 s reuse and the
/// in-flight sharing are keyed by it (method, channel, cursor or latest).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Read {
    Conversations {
        cursor: Option<Cursor>,
    },
    History {
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    },
    Replies {
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    },
    User {
        id: UserId,
    },
    Users {
        cursor: Option<Cursor>,
    },
}

impl Read {
    /// The conversation the read is about, for its error message.
    pub fn channel(&self) -> Option<&ConversationId> {
        match self {
            Self::History { channel, .. } | Self::Replies { channel, .. } => Some(channel),
            Self::Conversations { .. } | Self::User { .. } | Self::Users { .. } => None,
        }
    }

    fn send(&self, api: &dyn SlackApi) -> BoxFuture<'static, Result<Answer, ApiError>> {
        match self.clone() {
            Self::Conversations { cursor } => api
                .conversations(cursor)
                .map(|result| result.map(Answer::Conversations))
                .boxed(),
            Self::History {
                channel,
                latest,
                limit,
            } => api
                .history(channel, latest, limit)
                .map(|result| result.map(Answer::Messages))
                .boxed(),
            Self::Replies {
                channel,
                thread_ts,
                cursor,
            } => api
                .replies(channel, thread_ts, cursor)
                .map(|result| result.map(Answer::Messages))
                .boxed(),
            Self::User { id } => api.user(id).map(|result| result.map(Answer::User)).boxed(),
            Self::Users { cursor } => api
                .users(cursor)
                .map(|result| result.map(Answer::Users))
                .boxed(),
        }
    }
}

/// The answer of a [`Read`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Conversations(Page<Conversation>),
    Messages(Page<Message>),
    User(User),
    Users(Page<User>),
}

/// What a request does: a read or a post.
enum Operation {
    Read(Read),
    Post(PostRequest),
}

enum Done {
    Answer(Answer),
    Posted(PostedMessage),
}

impl Operation {
    fn send(&self, api: &dyn SlackApi) -> BoxFuture<'static, Result<Done, ApiError>> {
        match self {
            Self::Read(read) => read
                .send(api)
                .map(|result| result.map(Done::Answer))
                .boxed(),
            Self::Post(request) => api
                .post(request.clone())
                .map(|result| result.map(Done::Posted))
                .boxed(),
        }
    }

    /// The error of a failure that is neither a rate limit nor a refused token.
    fn failure(&self, cause: ApiError) -> SlackError {
        match self {
            Self::Read(read) => SlackError::Read {
                channel: read.channel().cloned(),
                cause,
            },
            Self::Post(request) => match cause {
                ApiError::Network | ApiError::Server { .. } => SlackError::PostMaybeNotSent {
                    channel: request.channel.clone(),
                    cause,
                },
                cause => SlackError::PostRefused {
                    channel: request.channel.clone(),
                    cause,
                },
            },
        }
    }
}

/// The one shared rate-limit wait.
#[derive(Clone)]
struct Wait {
    until: Instant,
    until_utc: DateTime<Utc>,
    /// Ready once the wait is over and cleared.
    done: Shared<Task<()>>,
}

type ClientTask = Shared<Task<Result<Arc<dyn SlackApi>, SlackError>>>;
type ReadTask = Shared<Task<Result<Answer, SlackError>>>;

/// The application's Slack service. One per application; see [`SlackService::global`].
pub struct SlackService {
    clock: Arc<dyn Clock>,
    zone: Arc<dyn render::Clock>,
    /// The client of the connection with the given generation, or its loading.
    client: Option<(u64, ClientTask)>,
    /// The generation of the connection whose token Slack rejected.
    rejected: Option<u64>,
    wait: Option<Wait>,
    /// The generation the caches below belong to.
    cache_generation: u64,
    reads: HashMap<Read, (Instant, Answer)>,
    in_flight: HashMap<Read, ReadTask>,
    users: HashMap<UserId, User>,
    conversations: Option<(Instant, Vec<Conversation>)>,
    /// Ready once the last post asked for has finished.
    post_tail: Option<Shared<oneshot::Receiver<()>>>,
    _connection: Subscription,
    _settings: Subscription,
}

struct GlobalSlackService(Entity<SlackService>);

impl Global for GlobalSlackService {}

/// The check that a post's text fits Slack's limit belongs here; it comes with the post commands.
/// Until then every text passes.
fn check_post_text(_text: &str) -> Result<(), SlackError> {
    Ok(())
}

impl SlackService {
    /// Installs the service with the executor's clock and the local time zone. Makes no request.
    pub fn init(cx: &mut App) -> Entity<Self> {
        let clock = cx
            .background_executor()
            .scheduler_executor()
            .scheduler()
            .clock();
        Self::install(clock, Arc::new(LocalZone), cx)
    }

    /// Installs the service with `clock` (for waits and the five-minute rule) and `zone` (for the
    /// times shown). Makes no request.
    pub fn install(
        clock: Arc<dyn Clock>,
        zone: Arc<dyn render::Clock>,
        cx: &mut App,
    ) -> Entity<Self> {
        let service = cx.new(|cx| Self {
            clock,
            zone,
            client: None,
            rejected: None,
            wait: None,
            cache_generation: connection::generation(cx),
            reads: HashMap::default(),
            in_flight: HashMap::default(),
            users: HashMap::default(),
            conversations: None,
            post_tail: None,
            _connection: cx.observe_global::<SlackConnection>(|this: &mut Self, cx| {
                this.sync_generation(cx);
                cx.notify();
            }),
            _settings: cx.observe_global::<settings::SettingsStore>(|_: &mut Self, cx| cx.notify()),
        });
        cx.set_global(GlobalSlackService(service.clone()));
        service
    }

    /// The service, once installed.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSlackService>()
            .map(|global| global.0.clone())
    }

    /// Where the connection stands, whatever `slack.enabled` says (see [`Self::is_enabled`]).
    pub fn state(&self, cx: &App) -> SlackState {
        match connection::current(cx) {
            None => SlackState::NotConnected,
            Some(connection) if self.rejected == Some(connection::generation(cx)) => {
                SlackState::Reconnect(connection)
            }
            Some(connection) => SlackState::Connected(connection),
        }
    }

    /// Whether `slack.enabled` is true.
    pub fn is_enabled(cx: &App) -> bool {
        SlackSettings::try_get(cx).is_some_and(|settings| settings.enabled)
    }

    /// How long the shared rate-limit wait has left, or `None` when there is none.
    pub fn wait_left(&self) -> Option<Duration> {
        let wait = self.wait.as_ref()?;
        Some(wait.until.saturating_duration_since(self.clock.now()))
    }

    /// When the shared rate-limit wait ends, or `None` when there is none.
    pub fn waiting_until(&self) -> Option<Instant> {
        self.wait.as_ref().map(|wait| wait.until)
    }

    /// The local time the shared wait ends, `HH:MM:SS`, or `None` when there is none.
    pub fn wait_ends_at(&self) -> Option<String> {
        let wait = self.wait.as_ref()?;
        Some(self.local_time(wait.until_utc, "%H:%M:%S"))
    }

    /// The names of the users seen this session.
    pub fn user_names(&self) -> UserNames {
        UserNames::from_users(self.users.values())
    }

    /// The conversation list as last read, if any.
    pub fn cached_conversations(&self) -> Option<&[Conversation]> {
        self.conversations
            .as_ref()
            .map(|(_, conversations)| conversations.as_slice())
    }

    fn local_time(&self, utc: DateTime<Utc>, format: &str) -> String {
        let offset = self.zone.utc_offset_seconds(utc.timestamp());
        (utc + chrono::Duration::seconds(i64::from(offset)))
            .format(format)
            .to_string()
    }

    fn rate_limit_error(&self, wait: &Wait) -> SlackError {
        SlackError::RateLimit {
            retry_at: self.local_time(wait.until_utc, "%H:%M"),
        }
    }

    /// Why no request may be made now, if so.
    fn check(&self, cx: &App) -> Result<(), SlackError> {
        if !Self::is_enabled(cx) {
            return Err(SlackError::Off);
        }
        match self.state(cx) {
            SlackState::NotConnected => Err(SlackError::NotConnected),
            SlackState::Reconnect(_) => Err(SlackError::Reconnect),
            SlackState::Connected(_) => Ok(()),
        }
    }

    /// Drops the client and the caches of an earlier connection.
    fn sync_generation(&mut self, cx: &App) {
        let generation = connection::generation(cx);
        if generation == self.cache_generation {
            return;
        }
        self.cache_generation = generation;
        self.client = None;
        self.reads.clear();
        self.in_flight.clear();
        self.users.clear();
        self.conversations = None;
    }

    /// Slack rejected the token of the connection `generation` (or it is missing).
    fn reject(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation == connection::generation(cx) {
            self.rejected = Some(generation);
            self.client = None;
            cx.notify();
        }
    }

    /// The client of the current connection, reading the token on the first request.
    fn client(&mut self, cx: &mut Context<Self>) -> (u64, ClientTask) {
        self.sync_generation(cx);
        let generation = connection::generation(cx);
        if let Some((loaded, task)) = &self.client
            && *loaded == generation
        {
            return (generation, task.clone());
        }
        let token = connection::read_token(cx);
        let task = cx
            .spawn(async move |this, cx| {
                let result = match token.await {
                    Err(cause) => Err(SlackError::SecretStore(cause)),
                    Ok(None) => Err(SlackError::Reconnect),
                    Ok(Some(token)) => cx
                        .update(|cx| client::client(Some(&token), cx))
                        .map_err(|_| SlackError::NoClient),
                };
                if let Err(error) = &result {
                    this.update(cx, |this, cx| {
                        if *error == SlackError::Reconnect {
                            // A connection whose secret is missing at first use.
                            this.reject(generation, cx);
                        } else if this
                            .client
                            .as_ref()
                            .is_some_and(|(loaded, _)| *loaded == generation)
                        {
                            // The next request tries again.
                            this.client = None;
                        }
                    })
                    .ok();
                }
                result
            })
            .shared();
        self.client = Some((generation, task.clone()));
        (generation, task)
    }

    /// Records Slack's request to wait `retry_after`, or returns the wait already running when it
    /// ends as late or later: one shared `until`, never stacked.
    fn start_wait(&mut self, retry_after: Duration, cx: &mut Context<Self>) -> Wait {
        let until = self.clock.now() + retry_after;
        if let Some(wait) = &self.wait
            && wait.until >= until
        {
            return wait.clone();
        }
        let until_utc = self.clock.utc_now()
            + chrono::Duration::from_std(retry_after).unwrap_or(chrono::Duration::MAX);
        let timer = cx.background_executor().timer(retry_after);
        let done = cx
            .spawn(async move |this, cx| {
                timer.await;
                this.update(cx, |this, cx| {
                    if this.wait.as_ref().is_some_and(|wait| wait.until == until) {
                        this.wait = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .shared();
        let wait = Wait {
            until,
            until_utc,
            done,
        };
        self.wait = Some(wait.clone());
        cx.notify();
        wait
    }

    /// Makes `read`, or returns its answer from the last [`READ_REUSE`], or shares the same read
    /// already running.
    pub fn call(&mut self, read: Read, cx: &mut Context<Self>) -> Task<Result<Answer, SlackError>> {
        if let Err(error) = self.check(cx) {
            return Task::ready(Err(error));
        }
        self.sync_generation(cx);
        let now = self.clock.now();
        if let Some((at, answer)) = self.reads.get(&read)
            && now.saturating_duration_since(*at) < READ_REUSE
        {
            return Task::ready(Ok(answer.clone()));
        }
        let running = match self.in_flight.get(&read) {
            Some(running) => running.clone(),
            None => {
                let generation = self.cache_generation;
                let operation = Operation::Read(read.clone());
                let key = read.clone();
                let running = cx
                    .spawn(async move |this, cx| {
                        let result = run(this.clone(), now, &operation, cx).await;
                        let result = result.map(|done| match done {
                            Done::Answer(answer) => answer,
                            Done::Posted(_) => unreachable!("a read answers a read"),
                        });
                        this.update(cx, |this, _| {
                            if this.cache_generation != generation {
                                return;
                            }
                            this.in_flight.remove(&key);
                            if let Ok(answer) = &result {
                                this.remember(answer);
                                this.reads.insert(key, (this.clock.now(), answer.clone()));
                            }
                        })
                        .ok();
                        result
                    })
                    .shared();
                self.in_flight.insert(read, running.clone());
                running
            }
        };
        cx.spawn(async move |_, _| running.await)
    }

    fn remember(&mut self, answer: &Answer) {
        match answer {
            Answer::User(user) => {
                self.users.insert(user.id.clone(), user.clone());
            }
            Answer::Users(page) => {
                for user in &page.items {
                    self.users.insert(user.id.clone(), user.clone());
                }
            }
            Answer::Conversations(_) | Answer::Messages(_) => {}
        }
    }

    /// One page of a conversation's history, newest first.
    pub fn history(
        &mut self,
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Page<Message>, SlackError>> {
        let task = self.call(
            Read::History {
                channel,
                latest,
                limit,
            },
            cx,
        );
        cx.background_spawn(async move {
            match task.await? {
                Answer::Messages(page) => Ok(page),
                _ => unreachable!("history answers messages"),
            }
        })
    }

    /// A user, from the session cache when it was seen before.
    pub fn user(&mut self, id: UserId, cx: &mut Context<Self>) -> Task<Result<User, SlackError>> {
        if let Some(user) = self.users.get(&id)
            && self.check(cx).is_ok()
        {
            return Task::ready(Ok(user.clone()));
        }
        let task = self.call(Read::User { id }, cx);
        cx.background_spawn(async move {
            match task.await? {
                Answer::User(user) => Ok(user),
                _ => unreachable!("users.info answers a user"),
            }
        })
    }

    /// The conversation list for a picker: the session's copy, read again when it is older than
    /// [`LIST_MAX_AGE`].
    pub fn conversation_list(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Conversation>, SlackError>> {
        if let Err(error) = self.check(cx) {
            return Task::ready(Err(error));
        }
        self.sync_generation(cx);
        if let Some((at, conversations)) = &self.conversations
            && self.clock.now().saturating_duration_since(*at) < LIST_MAX_AGE
        {
            return Task::ready(Ok(conversations.clone()));
        }
        let generation = self.cache_generation;
        cx.spawn(async move |this, cx| {
            let mut conversations = Vec::new();
            let mut cursor = None;
            for _ in 0..MAX_LIST_PAGES {
                let page = this
                    .update(cx, |this, cx| {
                        this.call(
                            Read::Conversations {
                                cursor: cursor.take(),
                            },
                            cx,
                        )
                    })
                    .map_err(|_| SlackError::NotConnected)?
                    .await?;
                let Answer::Conversations(page) = page else {
                    unreachable!("conversations.list answers conversations");
                };
                conversations.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            this.update(cx, |this, _| {
                if this.cache_generation == generation {
                    this.conversations = Some((this.clock.now(), conversations.clone()));
                }
            })
            .ok();
            Ok(conversations)
        })
    }

    /// Posts `text` to `destination`, after every post asked for before it. `origin` is who asks;
    /// whether it may post without asking is decided before this call. The text is sent as given.
    pub fn post(
        &mut self,
        destination: Destination,
        text: String,
        origin: Origin,
        cx: &mut Context<Self>,
    ) -> Task<Result<PostedMessage, SlackError>> {
        if let Err(error) = self.check(cx).and_then(|()| check_post_text(&text)) {
            return Task::ready(Err(error));
        }
        log::debug!("Slack post to {destination} from {origin:?}");
        let began = self.clock.now();
        let operation = Operation::Post(PostRequest {
            channel: destination.conversation,
            text,
            thread_ts: destination.thread,
        });
        let (finished, finished_rx) = oneshot::channel::<()>();
        let previous = self.post_tail.replace(finished_rx.shared());
        cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await.ok();
            }
            let result = run(this, began, &operation, cx).await;
            drop(finished);
            result.map(|done| match done {
                Done::Posted(posted) => posted,
                Done::Answer(_) => unreachable!("a post answers a post"),
            })
        })
    }
}

/// Sends `operation`, waiting behind the shared rate-limit wait and repeating after a rate
/// limit, until it is answered, fails, or its wait would end more than [`MAX_WAIT`] after
/// `began`.
async fn run(
    this: WeakEntity<SlackService>,
    began: Instant,
    operation: &Operation,
    cx: &mut AsyncApp,
) -> Result<Done, SlackError> {
    let gone = |_| SlackError::NotConnected;
    loop {
        let wait = this
            .update(cx, |this, cx| this.check(cx).map(|()| this.wait.clone()))
            .map_err(gone)??;
        if let Some(wait) = wait {
            if wait.until > began + MAX_WAIT {
                return Err(this
                    .read_with(cx, |this, _| this.rate_limit_error(&wait))
                    .map_err(gone)?);
            }
            wait.done.await;
            continue;
        }
        let (generation, client) = this.update(cx, |this, cx| this.client(cx)).map_err(gone)?;
        let client = client.await?;
        match operation.send(&*client).await {
            Ok(done) => return Ok(done),
            Err(ApiError::RateLimited { retry_after }) => {
                let wait = this
                    .update(cx, |this, cx| this.start_wait(retry_after, cx))
                    .map_err(gone)?;
                if wait.until > began + MAX_WAIT {
                    return Err(this
                        .read_with(cx, |this, _| this.rate_limit_error(&wait))
                        .map_err(gone)?);
                }
            }
            Err(ApiError::Unauthorized { .. }) => {
                this.update(cx, |this, cx| this.reject(generation, cx)).ok();
                return Err(SlackError::Reconnect);
            }
            Err(ApiError::NotConnected) => return Err(SlackError::NotConnected),
            Err(cause) => return Err(operation.failure(cause)),
        }
    }
}
