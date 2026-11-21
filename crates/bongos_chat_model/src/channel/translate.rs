//! Turning what an agent tells its hooks into a [`Report`].
//!
//! Claude Code runs the reporter as a hook and writes a JSON object to its stdin. That object holds
//! conversation content (the prompt, the last assistant message, tool input and output, the
//! transcript path, the working directory, compaction summaries). The reporter must never send any
//! of it, so [`ClaudeHookInput`] names only the four fields the mapping needs (`hook_event_name`,
//! `session_id`, `source`, `notification_type`): serde skips every other key without keeping its
//! value, and the four that are kept are checked again before they reach a report (a session id
//! must have an id's shape, a notification kind must be a short lowercase word).
//!
//! The `state <running|waiting|idle>` mode reads nothing; [`state_report`] builds its report.

use std::fmt;
use std::io::{self, Read};

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::report::{
    AgentName, ChatId, Detail, Event, Report, ReportedState, SessionId, SessionSource, Token,
};

/// The most hook input bytes the translator reads; the rest is read and thrown away.
pub const MAX_HOOK_INPUT_BYTES: u64 = 16 * 1024 * 1024;

/// Reads at most [`MAX_HOOK_INPUT_BYTES`] from `reader`, then reads and discards whatever follows so
/// the writer is never left blocked on a full pipe.
pub fn read_bounded(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take(MAX_HOOK_INPUT_BYTES)
        .read_to_end(&mut input)?;
    io::copy(reader, &mut io::sink())?;
    Ok(input)
}

/// What every report from one launch shares: the chat, the launch token and the reporter's start
/// time in milliseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportIdentity {
    pub chat: ChatId,
    pub token: Token,
    pub at: u64,
}

/// A whitelisted string field of hook input: its text when it is a JSON string, else only the fact
/// that it held something else (whose value is skipped, never kept).
#[derive(Clone, Debug, PartialEq, Eq)]
enum HookText {
    Text(String),
    NotText,
}

impl HookText {
    fn text(&self) -> Option<&str> {
        match self {
            HookText::Text(text) => Some(text),
            HookText::NotText => None,
        }
    }
}

impl<'de> Deserialize<'de> for HookText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(HookTextVisitor)
    }
}

struct HookTextVisitor;

impl<'de> Visitor<'de> for HookTextVisitor {
    type Value = HookText;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<HookText, E> {
        Ok(HookText::Text(value.to_string()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<HookText, E> {
        Ok(HookText::Text(value))
    }

    fn visit_bool<E: de::Error>(self, _value: bool) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_i64<E: de::Error>(self, _value: i64) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_u64<E: de::Error>(self, _value: u64) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_f64<E: de::Error>(self, _value: f64) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_unit<E: de::Error>(self) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_none<E: de::Error>(self) -> Result<HookText, E> {
        Ok(HookText::NotText)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<HookText, A::Error> {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(HookText::NotText)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<HookText, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(HookText::NotText)
    }
}

/// The only fields of Claude Code's hook input that are ever read. Every other key (and there are
/// many, most of them conversation content) is skipped by serde without being kept. Do not add a
/// field here without checking that it can never hold content.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ClaudeHookInput {
    #[serde(default)]
    hook_event_name: Option<HookText>,
    #[serde(default)]
    session_id: Option<HookText>,
    #[serde(default)]
    source: Option<HookText>,
    #[serde(default)]
    notification_type: Option<HookText>,
}

impl ClaudeHookInput {
    /// Reads hook input; `None` when it is not one JSON object.
    pub fn parse(input: &[u8]) -> Option<Self> {
        serde_json::from_slice(input).ok()
    }

    /// The report for this hook event, or `None` for an event that has no report.
    ///
    /// The mapping: `SessionStart` is `session_start` with its source (an absent source is taken
    /// as `startup`; a source this version does not know gives no report), `UserPromptSubmit` is
    /// `prompt_submitted`, `Stop` is `turn_done`, `Notification` is `needs_input` with
    /// `notification_type` as the detail, and `PostToolUse` is `tool_done`. Every other event
    /// (`PreToolUse`, `PermissionRequest`, `PreCompact`, `PostCompact`, `SessionEnd`, and any
    /// event a later version adds) gives no report.
    ///
    /// The `notification_type` values assumed downstream are `permission_prompt` (waiting on a
    /// permission answer) and `idle_prompt` (waiting for the next prompt), with any other kind
    /// treated as waiting. These are unverified: Claude Code fired no `Notification` hook in
    /// non-interactive runs, so the values still need checking in a real terminal. Any value of
    /// a notification kind's shape is passed on as it is.
    pub fn to_report(&self, identity: ReportIdentity) -> Option<Report> {
        let event_name = self.hook_event_name.as_ref()?.text()?;
        let event = match event_name {
            "SessionStart" => {
                let source = match &self.source {
                    None => SessionSource::Startup,
                    Some(source) => SessionSource::from_wire(source.text()?)?,
                };
                Event::SessionStart { source }
            }
            "UserPromptSubmit" => Event::PromptSubmitted,
            "Stop" => Event::TurnDone,
            "Notification" => Event::NeedsInput {
                detail: self
                    .notification_type
                    .as_ref()
                    .and_then(HookText::text)
                    .and_then(Detail::new),
            },
            "PostToolUse" => Event::ToolDone,
            _ => return None,
        };
        let session = self
            .session_id
            .as_ref()
            .and_then(HookText::text)
            .and_then(SessionId::new);
        Some(Report {
            chat: identity.chat,
            token: identity.token,
            at: identity.at,
            agent: AgentName::claude(),
            event,
            session,
        })
    }
}

/// The report for one Claude Code hook run, from its raw stdin; `None` when the input is not a hook
/// object or its event has no report.
pub fn translate_claude_hook(input: &[u8], identity: ReportIdentity) -> Option<Report> {
    ClaudeHookInput::parse(input)?.to_report(identity)
}

/// The report of the generic `state` mode, which any agent that can run a command on its own
/// events uses to name its state. It reads nothing and carries no session.
pub fn state_report(identity: ReportIdentity, agent: AgentName, state: ReportedState) -> Report {
    Report {
        chat: identity.chat,
        token: identity.token,
        at: identity.at,
        agent,
        event: Event::State { state },
        session: None,
    }
}
