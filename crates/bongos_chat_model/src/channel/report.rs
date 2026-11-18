//! The report wire format: one newline-delimited JSON request line and one reply line per
//! connection on the chat channel socket.
//!
//! A report says only that an event happened in a chat (a session started, a prompt was submitted, a
//! turn ended, the agent needs input, a tool ran, or a named state), never what was said. Every field
//! is a bounded identifier or a fixed word, so a report cannot carry conversation content: an id
//! that fails its check is refused (chat, token, agent) or dropped (session, detail), never passed
//! through.
//!
//! A request, version 1, always renders every key, with `null` for an absent optional:
//!
//! ```json
//! {"v":1,"chat":"<id>","token":"<hex>","at":1762850000123,"agent":"claude","event":"prompt_submitted","session":"<id>","source":null,"state":null,"detail":null}
//! ```
//!
//! The reply is `{"ok":true}` or `{"ok":false,"error":"unknown chat"|"bad token"|"bad message"|"too large"}`.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::capture::is_uuid;

/// The only request version this build reads and writes. A request with another `v` is refused as a
/// bad message; a later version adds fields, which this one ignores.
pub const VERSION: u64 = 1;

/// The longest request or reply line, in bytes, counting its terminating newline.
pub const MAX_LINE_BYTES: usize = 4096;

/// The most connections the server keeps open at once.
pub const MAX_CONNECTIONS: usize = 32;

/// How long the server waits for a connection's request line.
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest chat id, token or session id.
pub const MAX_ID_CHARS: usize = 128;

/// The longest agent name.
pub const MAX_AGENT_CHARS: usize = 32;

/// The longest `detail` word.
pub const MAX_DETAIL_CHARS: usize = 64;

/// Whether `text` is 1 to `max` characters from `[A-Za-z0-9_-]`.
fn is_plain_id(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The id of the bongos chat a report is for: 1 to 128 characters from `[A-Za-z0-9_-]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChatId(String);

impl ChatId {
    pub fn new(text: &str) -> Option<Self> {
        is_plain_id(text, MAX_ID_CHARS).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The per-launch token that proves a report comes from that launch's process: 1 to 128 characters
/// from `[A-Za-z0-9_-]` (a launch uses 32 hex digits).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Token(String);

impl Token {
    pub fn new(text: &str) -> Option<Self> {
        is_plain_id(text, MAX_ID_CHARS).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The agent that sent a report, such as `claude`: 1 to 32 characters from `[a-z0-9_-]`. Any agent
/// name of that shape is accepted, so a new agent needs no change here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentName(String);

impl AgentName {
    pub const CLAUDE: &'static str = "claude";

    pub fn new(text: &str) -> Option<Self> {
        let valid = !text.is_empty()
            && text.len() <= MAX_AGENT_CHARS
            && text.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            });
        valid.then(|| Self(text.to_string()))
    }

    pub fn claude() -> Self {
        Self(Self::CLAUDE.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The agent's own session or thread id: a UUID or 1 to 128 characters from `[A-Za-z0-9_-]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(text: &str) -> Option<Self> {
        (is_uuid(text) || is_plain_id(text, MAX_ID_CHARS)).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A `needs_input` notification kind, such as `permission_prompt` or `idle_prompt`: 1 to 64
/// characters from `[a-z0-9_]`. The narrow shape is what keeps it a kind and never a message.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Detail(String);

impl Detail {
    pub fn new(text: &str) -> Option<Self> {
        let valid = !text.is_empty()
            && text.len() <= MAX_DETAIL_CHARS
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        valid.then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a session started, as the agent tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionSource {
    Startup,
    Resume,
    Fork,
    Clear,
    Compact,
}

impl SessionSource {
    pub const ALL: [SessionSource; 5] = [
        SessionSource::Startup,
        SessionSource::Resume,
        SessionSource::Fork,
        SessionSource::Clear,
        SessionSource::Compact,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SessionSource::Startup => "startup",
            SessionSource::Resume => "resume",
            SessionSource::Fork => "fork",
            SessionSource::Clear => "clear",
            SessionSource::Compact => "compact",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == text)
    }
}

/// The state a generic `state` report names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReportedState {
    Running,
    Waiting,
    Idle,
}

impl ReportedState {
    pub const ALL: [ReportedState; 3] = [
        ReportedState::Running,
        ReportedState::Waiting,
        ReportedState::Idle,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReportedState::Running => "running",
            ReportedState::Waiting => "waiting",
            ReportedState::Idle => "idle",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }
}

/// What happened. Each event carries the wire fields it needs, so a `session_start` always has its
/// source and a `state` event always has its state; the other optional wire fields are `null`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    SessionStart { source: SessionSource },
    PromptSubmitted,
    TurnDone,
    NeedsInput { detail: Option<Detail> },
    ToolDone,
    State { state: ReportedState },
}

impl Event {
    /// The `event` value on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            Event::SessionStart { .. } => "session_start",
            Event::PromptSubmitted => "prompt_submitted",
            Event::TurnDone => "turn_done",
            Event::NeedsInput { .. } => "needs_input",
            Event::ToolDone => "tool_done",
            Event::State { .. } => "state",
        }
    }
}

/// One report, version 1.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "WireOut", try_from = "WireIn")]
pub struct Report {
    pub chat: ChatId,
    pub token: Token,
    /// The reporter's wall clock in milliseconds since the Unix epoch when it started.
    pub at: u64,
    pub agent: AgentName,
    pub event: Event,
    pub session: Option<SessionId>,
}

/// The request as it is written: every key, in this order, `null` for an absent optional.
#[derive(Serialize)]
struct WireOut {
    v: u64,
    chat: String,
    token: String,
    at: u64,
    agent: String,
    event: &'static str,
    session: Option<String>,
    source: Option<&'static str>,
    state: Option<&'static str>,
    detail: Option<String>,
}

impl From<Report> for WireOut {
    fn from(report: Report) -> Self {
        let (source, state, detail) = match &report.event {
            Event::SessionStart { source } => (Some(source.as_str()), None, None),
            Event::State { state } => (None, Some(state.as_str()), None),
            Event::NeedsInput { detail } => {
                (None, None, detail.as_ref().map(|detail| detail.0.clone()))
            }
            Event::PromptSubmitted | Event::TurnDone | Event::ToolDone => (None, None, None),
        };
        WireOut {
            v: VERSION,
            chat: report.chat.0,
            token: report.token.0,
            at: report.at,
            agent: report.agent.0,
            event: report.event.name(),
            session: report.session.map(|session| session.0),
            source,
            state,
            detail,
        }
    }
}

/// The request as it is read. Unknown keys are ignored. `session` and `detail` are read as any JSON
/// value so that a bad one is dropped instead of refusing the report.
#[derive(Deserialize)]
struct WireIn {
    #[serde(default)]
    v: Option<serde_json::Value>,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    at: Option<u64>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    session: Option<serde_json::Value>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    detail: Option<serde_json::Value>,
}

impl TryFrom<WireIn> for Report {
    type Error = ParseError;

    fn try_from(wire: WireIn) -> Result<Self, ParseError> {
        match wire.v {
            None => return Err(ParseError::MissingField("v")),
            Some(serde_json::Value::Number(number)) if number.as_u64() == Some(VERSION) => {}
            Some(_) => return Err(ParseError::UnsupportedVersion),
        }
        let chat = wire.chat.ok_or(ParseError::MissingField("chat"))?;
        let chat = ChatId::new(&chat).ok_or(ParseError::InvalidField("chat"))?;
        let token = wire.token.ok_or(ParseError::MissingField("token"))?;
        let token = Token::new(&token).ok_or(ParseError::InvalidField("token"))?;
        let at = wire.at.ok_or(ParseError::MissingField("at"))?;
        let agent = wire.agent.ok_or(ParseError::MissingField("agent"))?;
        let agent = AgentName::new(&agent).ok_or(ParseError::InvalidField("agent"))?;
        let event = wire.event.ok_or(ParseError::MissingField("event"))?;
        let event = match event.as_str() {
            "session_start" => {
                let source = wire.source.ok_or(ParseError::MissingField("source"))?;
                let source =
                    SessionSource::from_wire(&source).ok_or(ParseError::InvalidField("source"))?;
                Event::SessionStart { source }
            }
            "prompt_submitted" => Event::PromptSubmitted,
            "turn_done" => Event::TurnDone,
            "needs_input" => Event::NeedsInput {
                detail: wire
                    .detail
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    .and_then(Detail::new),
            },
            "tool_done" => Event::ToolDone,
            "state" => {
                let state = wire.state.ok_or(ParseError::MissingField("state"))?;
                let state =
                    ReportedState::from_wire(&state).ok_or(ParseError::InvalidField("state"))?;
                Event::State { state }
            }
            _ => return Err(ParseError::InvalidField("event")),
        };
        let session = wire
            .session
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .and_then(SessionId::new);
        Ok(Report {
            chat,
            token,
            at,
            agent,
            event,
            session,
        })
    }
}

/// Why a request line was refused. No variant holds any text from the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the line is longer than {MAX_LINE_BYTES} bytes")]
    TooLarge,
    #[error("the line is not UTF-8")]
    NotUtf8,
    #[error("the line holds more than one line")]
    NotOneLine,
    #[error("the line is not one JSON object of the expected shape")]
    Json,
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("missing `{0}`")]
    MissingField(&'static str),
    #[error("invalid `{0}`")]
    InvalidField(&'static str),
}

impl ParseError {
    /// The error the server replies with for this refusal.
    pub fn reply_error(self) -> ReplyError {
        match self {
            ParseError::TooLarge => ReplyError::TooLarge,
            ParseError::NotUtf8
            | ParseError::NotOneLine
            | ParseError::Json
            | ParseError::UnsupportedVersion
            | ParseError::MissingField(_)
            | ParseError::InvalidField(_) => ReplyError::BadMessage,
        }
    }
}

/// Why a report could not be rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("the report would be longer than {MAX_LINE_BYTES} bytes")]
    TooLarge,
    #[error("the report could not be serialized")]
    Serialize,
}

/// Strips one trailing `\n` (and a `\r` before it).
fn strip_line_end(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Reads one request line, with or without its terminating newline.
pub fn parse(line: &[u8]) -> Result<Report, ParseError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ParseError::TooLarge);
    }
    let line = strip_line_end(line);
    let text = std::str::from_utf8(line).map_err(|_| ParseError::NotUtf8)?;
    if text.contains(['\n', '\r']) {
        return Err(ParseError::NotOneLine);
    }
    // `serde_json` refuses anything after the object other than whitespace.
    let wire: WireIn = serde_json::from_str(text).map_err(|_| ParseError::Json)?;
    Report::try_from(wire)
}

/// Writes one request line, newline included, refusing one longer than [`MAX_LINE_BYTES`].
pub fn render(report: &Report) -> Result<String, RenderError> {
    let mut line = serde_json::to_string(&WireOut::from(report.clone()))
        .map_err(|_| RenderError::Serialize)?;
    line.push('\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(RenderError::TooLarge);
    }
    Ok(line)
}

/// The errors a server reply names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplyError {
    UnknownChat,
    BadToken,
    BadMessage,
    TooLarge,
}

impl ReplyError {
    pub const ALL: [ReplyError; 4] = [
        ReplyError::UnknownChat,
        ReplyError::BadToken,
        ReplyError::BadMessage,
        ReplyError::TooLarge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReplyError::UnknownChat => "unknown chat",
            ReplyError::BadToken => "bad token",
            ReplyError::BadMessage => "bad message",
            ReplyError::TooLarge => "too large",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.as_str() == text)
    }
}

#[derive(Deserialize)]
struct WireReply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Writes one reply line, newline included: `{"ok":true}` or `{"ok":false,"error":"..."}`.
pub fn render_reply(reply: Result<(), ReplyError>) -> String {
    match reply {
        Ok(()) => "{\"ok\":true}\n".to_string(),
        Err(error) => format!("{{\"ok\":false,\"error\":\"{}\"}}\n", error.as_str()),
    }
}

/// Reads one reply line; `None` when it is not a reply this version knows.
pub fn parse_reply(line: &[u8]) -> Option<Result<(), ReplyError>> {
    if line.len() > MAX_LINE_BYTES {
        return None;
    }
    let text = std::str::from_utf8(strip_line_end(line)).ok()?;
    let reply: WireReply = serde_json::from_str(text).ok()?;
    match (reply.ok, reply.error) {
        (true, None) => Some(Ok(())),
        (false, Some(error)) => ReplyError::from_wire(&error).map(Err),
        _ => None,
    }
}
