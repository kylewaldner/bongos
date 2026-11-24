//! Parsing the answers of the Slack Web API: the HTTP status, the headers and the JSON body of
//! each method bongos uses become the types of this crate or an [`ApiError`].
//!
//! The functions are pure. An error never holds or echoes the body, so a token that Slack (or a
//! proxy) happened to repeat in it cannot reach a log or the screen.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::api::{ApiError, AuthInfo, PostedMessage, User};
use crate::types::{
    Conversation, ConversationId, ConversationKind, Cursor, Message, Page, Ts, UserId,
};

/// The response headers, as name and value pairs. Only `Retry-After` and `x-oauth-scopes` are
/// read, and names match without regard to case.
pub type Headers = [(String, String)];

/// How long to wait after a 429 when `Retry-After` is missing or is not a number of seconds.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The `Other` code used when the body has no usable error code.
const UNKNOWN_CODE: &str = "unknown_error";
/// The `Other` code used when the body is not what the method documents.
const INVALID_RESPONSE: &str = "invalid_response";

/// Parses the answer of `auth.test`. The scopes come from the `x-oauth-scopes` header.
pub fn parse_auth_test(status: u16, headers: &Headers, body: &[u8]) -> Result<AuthInfo, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        team: String,
        team_id: String,
        url: String,
        user: String,
        user_id: String,
    }
    let (raw, _) = decode::<Raw>(status, headers, body)?;
    let scopes = header(headers, "x-oauth-scopes")
        .map(|scopes| {
            scopes
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(AuthInfo {
        team: raw.team,
        team_id: raw.team_id,
        url: raw.url,
        user: raw.user,
        user_id: UserId(raw.user_id),
        scopes,
    })
}

/// Parses one page of `conversations.list`.
pub fn parse_conversations_list(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<Page<Conversation>, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        channels: Vec<RawConversation>,
    }
    let (raw, meta) = decode::<Raw>(status, headers, body)?;
    Ok(Page {
        items: raw.channels.into_iter().map(Conversation::from).collect(),
        next: meta.next_cursor(),
    })
}

/// Parses one page of `conversations.history`, newest message first as Slack sends it.
pub fn parse_conversations_history(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<Page<Message>, ApiError> {
    parse_messages(status, headers, body)
}

/// Parses one page of `conversations.replies`, the parent first as Slack sends it.
pub fn parse_conversations_replies(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<Page<Message>, ApiError> {
    parse_messages(status, headers, body)
}

/// Parses the answer of `users.info`.
pub fn parse_users_info(status: u16, headers: &Headers, body: &[u8]) -> Result<User, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        user: RawUser,
    }
    let (raw, _) = decode::<Raw>(status, headers, body)?;
    Ok(raw.user.into())
}

/// Parses one page of `users.list`.
pub fn parse_users_list(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<Page<User>, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        members: Vec<RawUser>,
    }
    let (raw, meta) = decode::<Raw>(status, headers, body)?;
    Ok(Page {
        items: raw.members.into_iter().map(User::from).collect(),
        next: meta.next_cursor(),
    })
}

/// Parses the answer of `chat.postMessage`.
pub fn parse_chat_post_message(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<PostedMessage, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        channel: String,
        ts: String,
    }
    let (raw, _) = decode::<Raw>(status, headers, body)?;
    Ok(PostedMessage {
        channel: ConversationId(raw.channel),
        ts: Ts::new(raw.ts),
    })
}

fn parse_messages(status: u16, headers: &Headers, body: &[u8]) -> Result<Page<Message>, ApiError> {
    #[derive(Deserialize)]
    struct Raw {
        messages: Vec<RawMessage>,
    }
    let (raw, meta) = decode::<Raw>(status, headers, body)?;
    Ok(Page {
        items: raw.messages.into_iter().map(Message::from).collect(),
        next: meta.next_cursor(),
    })
}

/// The fields every Slack answer has.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    ok: bool,
    error: Option<String>,
    needed: Option<String>,
    has_more: Option<bool>,
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Deserialize)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

impl Envelope {
    /// The cursor of the next page. An empty or absent cursor ends the paging, and so does
    /// `has_more: false`.
    fn next_cursor(&self) -> Option<Cursor> {
        if self.has_more == Some(false) {
            return None;
        }
        let cursor = self.response_metadata.as_ref()?.next_cursor.as_deref()?;
        (!cursor.is_empty()).then(|| Cursor(cursor.to_string()))
    }
}

/// Checks the status and the envelope, then reads the method's own fields.
fn decode<T: DeserializeOwned>(
    status: u16,
    headers: &Headers,
    body: &[u8],
) -> Result<(T, Envelope), ApiError> {
    if status == 429 {
        return Err(rate_limited(headers));
    }
    if !(200..300).contains(&status) {
        return Err(ApiError::Server { status });
    }
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| invalid_response())?;
    if !envelope.ok {
        return Err(error_of(&envelope, headers));
    }
    let fields = serde_json::from_slice(body).map_err(|_| invalid_response())?;
    Ok((fields, envelope))
}

fn invalid_response() -> ApiError {
    ApiError::Other {
        code: INVALID_RESPONSE.to_string(),
    }
}

fn rate_limited(headers: &Headers) -> ApiError {
    let retry_after = header(headers, "retry-after")
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs);
    ApiError::RateLimited { retry_after }
}

/// Maps the `error` code of an answer with `ok: false`.
fn error_of(envelope: &Envelope, headers: &Headers) -> ApiError {
    let code = envelope.error.as_deref().unwrap_or(UNKNOWN_CODE);
    match code {
        "invalid_auth" | "not_authed" | "token_revoked" | "token_expired" | "account_inactive" => {
            ApiError::Unauthorized {
                reason: code.to_string(),
            }
        }
        "missing_scope" => ApiError::MissingScope {
            needed: envelope
                .needed
                .as_deref()
                .filter(|needed| is_scope_list(needed))
                .unwrap_or("unknown")
                .to_string(),
        },
        "channel_not_found" => not_found("channel"),
        "thread_not_found" => not_found("thread"),
        "message_not_found" => not_found("message"),
        "user_not_found" => not_found("user"),
        "not_in_channel" => ApiError::NotInChannel,
        "request_admin"
        | "admin_required"
        | "admin_approval_required"
        | "app_approval_required"
        | "restricted_action" => ApiError::NeedsAdmin,
        "msg_too_long" => ApiError::TooLong { over: 0 },
        "ratelimited" | "rate_limited" => rate_limited(headers),
        _ => ApiError::Other {
            code: if is_code(code) { code } else { UNKNOWN_CODE }.to_string(),
        },
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError::NotFound {
        what: what.to_string(),
    }
}

/// Whether `code` is a Slack error code: `^[a-z_]{1,64}$`.
fn is_code(code: &str) -> bool {
    (1..=64).contains(&code.len()) && code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// Whether `text` looks like scope names (`channels:history`, or several joined by commas).
fn is_scope_list(text: &str) -> bool {
    (1..=256).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, '_' | ':' | '.' | ',' | '-'))
}

fn header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[derive(Deserialize)]
struct RawConversation {
    id: String,
    name: Option<String>,
    #[serde(default)]
    is_im: bool,
    #[serde(default)]
    is_mpim: bool,
    #[serde(default)]
    is_group: bool,
    #[serde(default)]
    is_private: bool,
    user: Option<String>,
}

impl From<RawConversation> for Conversation {
    fn from(raw: RawConversation) -> Self {
        let kind = if raw.is_im {
            ConversationKind::Dm
        } else if raw.is_mpim {
            ConversationKind::GroupDm
        } else if raw.is_group || raw.is_private {
            ConversationKind::Private
        } else {
            ConversationKind::Channel
        };
        Self {
            id: ConversationId(raw.id),
            kind,
            name: raw.name.unwrap_or_default(),
            user: raw.user.map(UserId),
        }
    }
}

#[derive(Deserialize)]
struct RawMessage {
    ts: String,
    user: Option<String>,
    #[serde(default)]
    text: String,
    thread_ts: Option<String>,
    #[serde(default)]
    reply_count: u32,
    subtype: Option<String>,
}

impl From<RawMessage> for Message {
    fn from(raw: RawMessage) -> Self {
        Self {
            ts: Ts::new(raw.ts),
            user: raw.user.map(UserId),
            text: raw.text,
            thread_ts: raw.thread_ts.map(Ts::new),
            reply_count: raw.reply_count,
            subtype: raw.subtype,
        }
    }
}

#[derive(Deserialize)]
struct RawUser {
    id: String,
    #[serde(default)]
    name: String,
    real_name: Option<String>,
    profile: Option<RawProfile>,
}

#[derive(Deserialize)]
struct RawProfile {
    display_name: Option<String>,
    real_name: Option<String>,
}

impl From<RawUser> for User {
    fn from(raw: RawUser) -> Self {
        let (display_name, profile_real_name) = match raw.profile {
            Some(profile) => (profile.display_name, profile.real_name),
            None => (None, None),
        };
        Self {
            id: UserId(raw.id),
            name: raw.name,
            display_name: display_name.unwrap_or_default(),
            real_name: raw.real_name.or(profile_real_name).unwrap_or_default(),
        }
    }
}
