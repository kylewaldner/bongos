//! The Slack Web API methods bongos uses, as a trait, with their answers and errors. The real
//! client lives in the app; tests use `fake::FakeSlack`.

use std::time::Duration;

use futures::future::BoxFuture;

use crate::types::{Conversation, ConversationId, Cursor, Message, Page, Ts, UserId};

/// The answer of `auth.test`: who the token belongs to and what it may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthInfo {
    /// The workspace name.
    pub team: String,
    pub team_id: String,
    /// The workspace URL, like `https://acme.slack.com/`.
    pub url: String,
    /// The user name.
    pub user: String,
    pub user_id: UserId,
    /// The scopes the token was granted (from the `x-oauth-scopes` header).
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    /// The handle, like `kyle`.
    pub name: String,
    /// The display name, which may be empty.
    pub display_name: String,
    pub real_name: String,
}

/// A message to post with `chat.postMessage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostRequest {
    pub channel: ConversationId,
    pub text: String,
    /// The thread to reply in, if any.
    pub thread_ts: Option<Ts>,
}

/// Where a posted message landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostedMessage {
    pub channel: ConversationId,
    pub ts: Ts,
}

/// Why a Slack request failed. No variant ever holds a token.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    #[error("Slack is not connected")]
    NotConnected,
    #[error("Slack refused the token ({reason})")]
    Unauthorized { reason: String },
    #[error("the Slack token lacks the {needed} scope")]
    MissingScope { needed: String },
    #[error("Slack could not find {what}")]
    NotFound { what: String },
    #[error("you are not in that Slack channel")]
    NotInChannel,
    #[error("a Slack workspace admin must approve the app first")]
    NeedsAdmin,
    #[error("Slack is rate limiting requests; retry in {} s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
    #[error("the message is {over} characters over Slack's limit")]
    TooLong { over: usize },
    #[error("could not reach Slack")]
    Network,
    #[error("Slack answered with server error {status}")]
    Server { status: u16 },
    #[error("Slack answered with error {code}")]
    Other { code: String },
}

pub trait SlackApi: Send + Sync {
    /// `auth.test`: checks the token and says whose it is.
    fn auth_test(&self) -> BoxFuture<'static, Result<AuthInfo, ApiError>>;
    /// `conversations.list` of every kind the token can see, one page at a time.
    fn conversations(
        &self,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Conversation>, ApiError>>;
    /// `conversations.history`: up to `limit` messages older than `latest` (the newest when
    /// `None`), newest first.
    fn history(
        &self,
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>>;
    /// `conversations.replies`: a thread, its parent first.
    fn replies(
        &self,
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>>;
    /// `users.info`.
    fn user(&self, id: UserId) -> BoxFuture<'static, Result<User, ApiError>>;
    /// `users.list`, one page at a time.
    fn users(&self, cursor: Option<Cursor>) -> BoxFuture<'static, Result<Page<User>, ApiError>>;
    /// `chat.postMessage`.
    fn post(&self, request: PostRequest) -> BoxFuture<'static, Result<PostedMessage, ApiError>>;
}
