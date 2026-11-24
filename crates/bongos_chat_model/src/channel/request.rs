//! Request lines on the chat channel socket: a running chat asks bongos something and waits for the
//! answer, on the same socket, with the same chat and token checks as a report.
//!
//! A request, version 1, is one newline-terminated JSON line of at most [`MAX_REQUEST_BYTES`]:
//!
//! ```json
//! {"v":1,"chat":"<id>","token":"<hex>","request":"<kind>","body":{}}
//! ```
//!
//! The reply is one line of at most [`MAX_REPLY_BYTES`]: `{"ok":true,"reply":<any JSON>}`, or
//! `{"ok":false,"error":"..."}` with an error of [`ReplyError`] (`unknown request` when no handler
//! serves the kind).
//!
//! A line is a request when the `request` key of its top-level object appears within its first
//! [`MAX_LINE_BYTES`]; [`render`] writes that key before the body, so it always does. Any other line
//! is a report and keeps the report limit, so a report can never grow past [`MAX_LINE_BYTES`].
//!
//! [`send`] is the blocking client: it connects like the reporter, writes one request and waits at
//! most the given time (in production [`REPLY_WAIT`]) for the reply.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::client::ChannelTarget;
use super::report::{ChatId, MAX_LINE_BYTES, ParseError, ReplyError, Token, VERSION};

pub use serde_json::Value;

/// A request's body: always a JSON object.
pub type Body = serde_json::Map<String, Value>;

/// The longest request line, in bytes, counting its terminating newline.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// The longest reply line to a request, in bytes, counting its terminating newline.
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// How long [`send`]'s production callers wait for a reply.
pub const REPLY_WAIT: Duration = Duration::from_secs(330);

/// How long [`send`] waits for the connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long [`send`] waits for the request to be written.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest request kind.
pub const MAX_KIND_CHARS: usize = 64;

/// The key that makes a line a request.
pub const REQUEST_KEY: &str = "request";

/// What a request asks for: 1 to 64 characters from `[a-z0-9_]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestKind(String);

impl RequestKind {
    pub fn new(text: &str) -> Option<Self> {
        let valid = !text.is_empty()
            && text.len() <= MAX_KIND_CHARS
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        valid.then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One request, version 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub chat: ChatId,
    pub token: Token,
    pub kind: RequestKind,
    pub body: Body,
}

/// The request as it is written, in this key order (`request` before the body).
#[derive(Serialize)]
struct WireOut<'a> {
    v: u64,
    chat: &'a str,
    token: &'a str,
    request: &'a str,
    body: &'a Body,
}

/// The request as it is read. Unknown keys are ignored.
#[derive(Deserialize)]
struct WireIn {
    #[serde(default)]
    v: Option<Value>,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    body: Option<Value>,
}

/// Whether `line` is a request: its top-level object has a `request` key within the first
/// [`MAX_LINE_BYTES`]. Only those bytes are looked at, so the caller can decide before reading more.
pub fn is_request(line: &[u8]) -> bool {
    let prefix = &line[..line.len().min(MAX_LINE_BYTES)];
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    // Where the string being read started, when it sits directly in the top-level object.
    let mut key_start = None;
    // The top-level string just closed, until something other than whitespace follows it.
    let mut closed: Option<(usize, usize)> = None;
    for (index, &byte) in prefix.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                closed = key_start.take().map(|start| (start, index));
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                closed = None;
                if depth == 1 {
                    key_start = Some(index + 1);
                }
            }
            b'{' | b'[' => {
                depth += 1;
                closed = None;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                closed = None;
            }
            b':' => {
                if let Some((start, end)) = closed.take()
                    && &prefix[start..end] == REQUEST_KEY.as_bytes()
                {
                    return true;
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' => {}
            _ => closed = None,
        }
    }
    false
}

/// Strips one trailing `\n` (and a `\r` before it).
fn strip_line_end(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Reads one request line, with or without its terminating newline.
pub fn parse(line: &[u8]) -> Result<Request, ParseError> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ParseError::TooLarge);
    }
    let text = std::str::from_utf8(strip_line_end(line)).map_err(|_| ParseError::NotUtf8)?;
    if text.contains(['\n', '\r']) {
        return Err(ParseError::NotOneLine);
    }
    let wire: WireIn = serde_json::from_str(text).map_err(|_| ParseError::Json)?;
    match wire.v {
        None => return Err(ParseError::MissingField("v")),
        Some(Value::Number(number)) if number.as_u64() == Some(VERSION) => {}
        Some(_) => return Err(ParseError::UnsupportedVersion),
    }
    let chat = wire.chat.ok_or(ParseError::MissingField("chat"))?;
    let chat = ChatId::new(&chat).ok_or(ParseError::InvalidField("chat"))?;
    let token = wire.token.ok_or(ParseError::MissingField("token"))?;
    let token = Token::new(&token).ok_or(ParseError::InvalidField("token"))?;
    let kind = wire.request.ok_or(ParseError::MissingField("request"))?;
    let kind = RequestKind::new(&kind).ok_or(ParseError::InvalidField("request"))?;
    let body = match wire.body.ok_or(ParseError::MissingField("body"))? {
        Value::Object(body) => body,
        _ => return Err(ParseError::InvalidField("body")),
    };
    Ok(Request {
        chat,
        token,
        kind,
        body,
    })
}

/// Why a request could not be rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("the request would be longer than {MAX_REQUEST_BYTES} bytes")]
    TooLarge,
    #[error("the request could not be serialized")]
    Serialize,
}

/// Writes one request line, newline included, refusing one longer than [`MAX_REQUEST_BYTES`].
pub fn render(request: &Request) -> Result<String, RenderError> {
    let mut line = serde_json::to_string(&WireOut {
        v: VERSION,
        chat: request.chat.as_str(),
        token: request.token.as_str(),
        request: request.kind.as_str(),
        body: &request.body,
    })
    .map_err(|_| RenderError::Serialize)?;
    line.push('\n');
    if line.len() > MAX_REQUEST_BYTES {
        return Err(RenderError::TooLarge);
    }
    Ok(line)
}

#[derive(Serialize)]
struct WireReplyOut<'a> {
    ok: bool,
    reply: &'a Value,
}

#[derive(Deserialize)]
struct WireReplyIn {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    reply: Option<Value>,
}

/// Writes one reply line, newline included: `{"ok":true,"reply":...}` or
/// `{"ok":false,"error":"..."}`. A reply longer than [`MAX_REPLY_BYTES`] becomes `too large`.
pub fn render_reply(reply: Result<&Value, ReplyError>) -> String {
    let error = match reply {
        Ok(reply) => match serde_json::to_string(&WireReplyOut { ok: true, reply }) {
            Ok(mut line) => {
                line.push('\n');
                if line.len() <= MAX_REPLY_BYTES {
                    return line;
                }
                ReplyError::TooLarge
            }
            Err(_) => ReplyError::BadMessage,
        },
        Err(error) => error,
    };
    super::report::render_reply(Err(error))
}

/// Reads one reply line; `None` when it is not a reply this version knows.
pub fn parse_reply(line: &[u8]) -> Option<Result<Value, ReplyError>> {
    if line.len() > MAX_REPLY_BYTES {
        return None;
    }
    let text = std::str::from_utf8(strip_line_end(line)).ok()?;
    let reply: WireReplyIn = serde_json::from_str(text).ok()?;
    match (reply.ok, reply.error) {
        (true, None) => Some(Ok(reply.reply.unwrap_or(Value::Null))),
        (false, Some(error)) => ReplyError::from_wire(&error).map(Err),
        _ => None,
    }
}

/// Why [`send`] got no reply. No variant holds any text from the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("no chat channel in the environment")]
    NoChannel,
    #[error("invalid request kind")]
    InvalidKind,
    #[error("the request is longer than {MAX_REQUEST_BYTES} bytes")]
    TooLarge,
    #[error("cannot connect to the chat channel")]
    Connect,
    #[error("cannot write the request")]
    Write,
    #[error("no reply in time")]
    Timeout,
    #[error("the connection closed without a reply")]
    Closed,
    #[error("the reply is not one this version knows")]
    BadReply,
    #[error("refused: {}", .0.as_str())]
    Refused(ReplyError),
}

/// Sends one request of `kind` for the chat the environment (read through `env`, as the reporter
/// does) names, and waits at most `reply_wait` for its reply ([`REPLY_WAIT`] in production).
pub fn send(
    env: &dyn Fn(&str) -> Option<String>,
    kind: &str,
    body: Body,
    reply_wait: Duration,
) -> Result<Value, SendError> {
    let target = ChannelTarget::from_env(env).ok_or(SendError::NoChannel)?;
    let kind = RequestKind::new(kind).ok_or(SendError::InvalidKind)?;
    send_to(&target, kind, body, reply_wait)
}

/// [`send`] to a known target.
pub fn send_to(
    target: &ChannelTarget,
    kind: RequestKind,
    body: Body,
    reply_wait: Duration,
) -> Result<Value, SendError> {
    let line = render(&Request {
        chat: target.chat.clone(),
        token: target.token.clone(),
        kind,
        body,
    })
    .map_err(|_| SendError::TooLarge)?;
    exchange(&target.socket, line.as_bytes(), reply_wait)
}

#[cfg(unix)]
fn exchange(socket: &str, line: &[u8], reply_wait: Duration) -> Result<Value, SendError> {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::time::Instant;

    let mut stream = super::client::connect(socket, CONNECT_TIMEOUT).ok_or(SendError::Connect)?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|_| SendError::Write)?;
    stream
        .write_all(line)
        .and_then(|()| stream.flush())
        .map_err(|_| SendError::Write)?;
    // One deadline covers the whole reply, so a server that sends it slowly cannot stretch it.
    let deadline = Instant::now() + reply_wait;
    let mut reply = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SendError::Timeout);
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| SendError::Closed)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(SendError::Closed),
            Ok(read) => {
                let start = reply.len();
                reply.extend_from_slice(&chunk[..read]);
                if let Some(end) = reply[start..].iter().position(|byte| *byte == b'\n') {
                    reply.truncate(start + end + 1);
                    break;
                }
                if reply.len() > MAX_REPLY_BYTES {
                    return Err(SendError::BadReply);
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(SendError::Timeout);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Err(SendError::Closed),
        }
    }
    match parse_reply(&reply) {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(SendError::Refused(error)),
        None => Err(SendError::BadReply),
    }
}

#[cfg(not(unix))]
fn exchange(_socket: &str, _line: &[u8], _reply_wait: Duration) -> Result<Value, SendError> {
    Err(SendError::Connect)
}
