//! The Slack token: which kind it is, refusing the kinds that cannot read or post, and keeping it
//! out of every message. A `SlackToken` has no `Display`, and its `Debug` never shows the token.

use std::fmt;
use std::sync::Arc;

/// What every token is replaced with in text that may be shown or logged.
pub const HIDDEN: &str = "slack token (hidden)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// A user OAuth token (`xoxp-`): reads and posts as the user.
    User,
    /// A bot token (`xoxb-`): reads and posts as the app's bot.
    Bot,
}

/// Why a pasted text is not a usable token. The message never repeats the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error(
        "this is a refresh token; turn off token rotation for the app and paste its user OAuth token"
    )]
    Refresh,
    #[error("an app-level token cannot read or post")]
    AppLevel,
    #[error("not a Slack user or bot token")]
    NotAToken,
}

/// A Slack user or bot token.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackToken(Arc<str>);

impl SlackToken {
    /// Checks the kind of `text` (trimmed) before any request is made.
    pub fn parse(text: &str) -> Result<Self, TokenError> {
        let text = text.trim();
        token_kind(text)?;
        Ok(Self(Arc::from(text)))
    }

    pub fn kind(&self) -> TokenKind {
        token_kind(&self.0).expect("checked on parse")
    }

    /// The token itself, for the `Authorization` header and the credential store only.
    pub fn secret(&self) -> &str {
        &self.0
    }

    /// Replaces this token, and anything else that looks like a Slack token, in `text`.
    pub fn redact(&self, text: &str) -> String {
        redact(&text.replace(&*self.0, HIDDEN))
    }
}

impl fmt::Debug for SlackToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(HIDDEN)
    }
}

/// The kind of the token in `text`, or why it is refused.
pub fn token_kind(text: &str) -> Result<TokenKind, TokenError> {
    let body = |prefix: &str| {
        text.strip_prefix(prefix).is_some_and(|rest| {
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    };
    if body("xoxp-") {
        Ok(TokenKind::User)
    } else if body("xoxb-") {
        Ok(TokenKind::Bot)
    } else if text.starts_with("xoxe") {
        Err(TokenError::Refresh)
    } else if text.starts_with("xapp-") {
        Err(TokenError::AppLevel)
    } else {
        Err(TokenError::NotAToken)
    }
}

/// Replaces everything in `text` that looks like a Slack token (`xox?-…`, `xoxe.…`, `xapp-…`)
/// with [`HIDDEN`].
pub fn redact(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let prefix = token_prefix_len(&bytes[i..]);
        if prefix > 0 {
            let mut end = i + prefix;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-') {
                end += 1;
            }
            out.push_str(&text[copied..i]);
            out.push_str(HIDDEN);
            copied = end;
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The length of the token prefix `bytes` starts with, or 0.
fn token_prefix_len(bytes: &[u8]) -> usize {
    if bytes.starts_with(b"xapp-") {
        return 5;
    }
    match bytes {
        [b'x', b'o', b'x', kind, b'-' | b'.', ..] if kind.is_ascii_alphanumeric() => 5,
        _ => 0,
    }
}
