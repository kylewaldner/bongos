//! Identifiers and records of Slack: conversations, users, message timestamps, messages, pages
//! of results and the destination of a post.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// The id of a conversation: a public channel (`C…`), a private channel or group DM (`G…`) or a
/// direct message (`D…`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConversationId(pub String);

impl ConversationId {
    /// Checks that `text` looks like a conversation id: `C`, `D` or `G` followed by one or more
    /// uppercase letters or digits.
    pub fn parse(text: &str) -> Result<Self, DestinationError> {
        let mut chars = text.chars();
        match chars.next() {
            None => return Err(DestinationError::Empty),
            Some('C' | 'D' | 'G') => {}
            Some(_) => return Err(DestinationError::BadConversation),
        }
        let rest = chars.as_str();
        if rest.is_empty()
            || !rest
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Err(DestinationError::BadConversation);
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The id of a Slack user (`U…` or `W…`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(pub String);

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A message timestamp as Slack sends it (`"1730000000.000100"`), kept as text so it round-trips
/// exactly. Timestamps order numerically: seconds first, then the fraction as a decimal (so
/// `1.1` is `1.100000`, after `1.000100`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ts(String);

impl Ts {
    /// Wraps a timestamp Slack sent, without checking it.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Checks that `text` is digits, `.`, then exactly six digits.
    pub fn parse(text: &str) -> Result<Self, DestinationError> {
        match text.split_once('.') {
            Some((_, fraction)) if fraction.len() == 6 && Self::key_of(text).is_some() => {
                Ok(Self(text.to_string()))
            }
            _ => Err(DestinationError::BadTs),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The seconds, and the fraction digits without trailing zeros, so that comparing the
    /// digits as text compares the fractions as decimals.
    fn key_of(text: &str) -> Option<(u64, &str)> {
        let (seconds, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
        if seconds.is_empty() || !digits(seconds) || !digits(fraction) {
            return None;
        }
        if text.contains('.') && fraction.is_empty() {
            return None;
        }
        Some((seconds.parse().ok()?, fraction.trim_end_matches('0')))
    }
}

impl Ord for Ts {
    fn cmp(&self, other: &Self) -> Ordering {
        Self::key_of(&self.0)
            .cmp(&Self::key_of(&other.0))
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Ts {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The cursor of the next page of a paged Slack method.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cursor(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    Channel,
    Private,
    Dm,
    GroupDm,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// The channel name without `#`; empty for a direct message.
    pub name: String,
    /// The other person of a direct message.
    pub user: Option<UserId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ts: Ts,
    pub user: Option<UserId>,
    pub text: String,
    /// The thread the message belongs to, or starts when `thread_ts == ts`.
    pub thread_ts: Option<Ts>,
    pub reply_count: u32,
    /// Slack's subtype (`bot_message`, `channel_join`, …); `None` for a plain message.
    pub subtype: Option<String>,
}

/// One page of a paged method; `next` is `None` on the last page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}

/// Where a post goes: a conversation, or a thread in it.
///
/// Its text form is the conversation id, then optionally `:` and the thread's timestamp:
/// `C0123` or `C0123:1730000000.000100`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Destination {
    pub conversation: ConversationId,
    pub thread: Option<Ts>,
}

impl Destination {
    pub fn parse(text: &str) -> Result<Self, DestinationError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(DestinationError::Empty);
        }
        let (conversation, thread) = match text.split_once(':') {
            Some((conversation, thread)) => (conversation, Some(Ts::parse(thread)?)),
            None => (text, None),
        };
        Ok(Self {
            conversation: ConversationId::parse(conversation)?,
            thread,
        })
    }
}

impl FromStr for Destination {
    type Err = DestinationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.thread {
            Some(thread) => write!(f, "{}:{}", self.conversation, thread),
            None => write!(f, "{}", self.conversation),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DestinationError {
    #[error("no Slack destination given")]
    Empty,
    // The messages do not echo the input: it may be a pasted token.
    #[error("not a Slack conversation id (C…, D… or G…)")]
    BadConversation,
    #[error("not a Slack message timestamp (like 1730000000.000100)")]
    BadTs,
}
