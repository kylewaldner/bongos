//! Slack message text as plain text and back: mentions and links shown as people read them, the
//! escaping a post needs, the length limit, links to messages and the transcript of a
//! conversation. Pure functions: the time zone comes in through [`Clock`].

use std::collections::HashMap;

use crate::api::{ApiError, User};
use crate::types::{ConversationId, Message, Page, Ts, UserId};

/// The most characters Slack keeps of a posted message; it cuts the rest off.
pub const MAX_POST_CHARS: usize = 40_000;

/// The time zone to show times in. It is a trait so that a test, or code that must not read the
/// real zone, can give a fixed one.
pub trait Clock {
    /// The offset of local time from UTC, in seconds east, at the instant `unix_seconds`.
    fn utc_offset_seconds(&self, unix_seconds: i64) -> i32;
}

/// A zone with a fixed offset from UTC, in seconds east.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedOffset(pub i32);

impl Clock for FixedOffset {
    fn utc_offset_seconds(&self, _unix_seconds: i64) -> i32 {
        self.0
    }
}

/// Display names by user id, for mentions and authors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserNames(HashMap<String, String>);

impl UserNames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the name to show for `id`.
    pub fn insert(&mut self, id: &UserId, name: impl Into<String>) {
        self.0.insert(id.0.clone(), name.into());
    }

    /// The names of `users`: the display name, else the real name, else the handle.
    pub fn from_users<'a>(users: impl IntoIterator<Item = &'a User>) -> Self {
        let mut names = Self::new();
        for user in users {
            let name = [&user.display_name, &user.real_name, &user.name]
                .into_iter()
                .find(|name| !name.is_empty());
            if let Some(name) = name {
                names.insert(&user.id, name.clone());
            }
        }
        names
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.0.get(id).map(String::as_str)
    }
}

/// Replies already fetched, by the timestamp of the thread's parent. Each list is as
/// `conversations.replies` answered it, so it may start with the parent itself.
pub type Threads = HashMap<Ts, Vec<Message>>;

/// Slack message text as people read it: `<@U1>` is `@name` (or `@U1` when the name is unknown),
/// `<#C1|general>` is `#general`, `<!here>` is `@here`, `<https://x|label>` is `label
/// (https://x)` and `<https://x>` is `https://x`. Then `&amp;`, `&lt;` and `&gt;` are unescaped,
/// once and last, so `&amp;lt;` is `&lt;`.
pub fn text(text: &str, users: &UserNames) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let (before, from_bracket) = rest.split_at(start);
        out.push_str(before);
        let after = from_bracket.get(1..).unwrap_or_default();
        match after.find('>') {
            Some(end) if !after.get(..end).unwrap_or_default().contains('<') => {
                let (inner, tail) = after.split_at(end);
                out.push_str(&token(inner, users));
                rest = tail.get(1..).unwrap_or_default();
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    unescape(&out)
}

/// What the inside of one `<…>` stands for.
fn token(inner: &str, users: &UserNames) -> String {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(label).filter(|label| !label.is_empty())),
        None => (inner, None),
    };
    if let Some(id) = target.strip_prefix('@') {
        let name = users.get(id).or(label).unwrap_or(id);
        format!("@{name}")
    } else if let Some(id) = target.strip_prefix('#') {
        format!("#{}", label.unwrap_or(id))
    } else if let Some(special) = target.strip_prefix('!') {
        match (special, label) {
            ("here" | "channel" | "everyone", _) => format!("@{special}"),
            (_, Some(label)) => label.to_string(),
            (_, None) => format!("@{}", special.split('^').next().unwrap_or(special)),
        }
    } else {
        match label {
            Some(label) if label != target => format!("{label} ({target})"),
            _ => target.to_string(),
        }
    }
}

/// Replaces `&amp;`, `&lt;` and `&gt;` in one pass, so the output is never unescaped twice.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        let (before, from_amp) = rest.split_at(start);
        out.push_str(before);
        let (replacement, length) = if from_amp.starts_with("&amp;") {
            ('&', 5)
        } else if from_amp.starts_with("&lt;") {
            ('<', 4)
        } else if from_amp.starts_with("&gt;") {
            ('>', 4)
        } else {
            ('&', 1)
        };
        out.push(replacement);
        rest = from_amp.get(length..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// Escapes the three characters Slack treats as markup: `&`, `<` and `>`, and nothing else.
pub fn escape_for_post(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Checks the escaped text of a post against Slack's limit of [`MAX_POST_CHARS`] characters (not
/// bytes).
pub fn check_length(escaped: &str) -> Result<(), ApiError> {
    let length = escaped.chars().count();
    match length.checked_sub(MAX_POST_CHARS) {
        Some(over) if over > 0 => Err(ApiError::TooLong { over }),
        _ => Ok(()),
    }
}

/// The message to show when a post is refused for being `over` characters too long.
pub fn too_long_message(over: usize) -> String {
    format!("{over} characters over Slack's limit; nothing was sent")
}

/// The link to a message: `<workspace>/archives/<channel>/p<ts without the dot>`, and for a
/// reply in a thread `?thread_ts=<thread>&cid=<channel>` after it.
pub fn message_link(
    workspace_url: &str,
    channel: &ConversationId,
    ts: &Ts,
    thread_ts: Option<&Ts>,
) -> String {
    let base = workspace_url.trim_end_matches('/');
    let compact: String = ts.as_str().chars().filter(|c| *c != '.').collect();
    let mut link = format!("{base}/archives/{channel}/p{compact}");
    if let Some(thread) = thread_ts.filter(|thread| *thread != ts) {
        link.push_str(&format!("?thread_ts={thread}&cid={channel}"));
    }
    link
}

/// `YYYY-MM-DD HH:MM` of the message timestamp in the clock's zone, or question marks when the
/// timestamp is not a number.
pub fn format_time(ts: &Ts, clock: &dyn Clock) -> String {
    let seconds = ts
        .as_str()
        .split('.')
        .next()
        .and_then(|seconds| seconds.parse::<i64>().ok());
    let Some(seconds) = seconds else {
        return "????-??-?? ??:??".to_string();
    };
    let local = seconds.saturating_add(i64::from(clock.utc_offset_seconds(seconds)));
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let minutes = local.rem_euclid(86_400) / 60;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// The year, month and day of a day count since 1970-01-01 (proleptic Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The name of a message's author: the user's name, else the user id, else `bot`.
pub(crate) fn author(message: &Message, users: &UserNames) -> String {
    match &message.user {
        Some(user) => users.get(&user.0).unwrap_or(&user.0).to_string(),
        None => "bot".to_string(),
    }
}

/// A row of a transcript's buffer: the message's timestamp and the timestamp of its thread.
pub type RowRef = (Ts, Option<Ts>);

/// A conversation as the text of a buffer, with what each row stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// One entry per line of `text` (0-based row): the message the row belongs to, or `None`
    /// for the header and the blank line after it.
    pub index: Vec<Option<RowRef>>,
}

impl Transcript {
    /// The message and thread timestamps of buffer row `row`.
    pub fn at(&self, row: usize) -> Option<&RowRef> {
        self.index.get(row)?.as_ref()
    }
}

/// Renders `pages` of history (each as `conversations.history` answered it) as a transcript,
/// oldest message first. A message's fetched replies in `threads` follow it, indented with
/// `  > `; a thread whose replies are not fetched says how many there are.
pub fn transcript(
    pages: &[Page<Message>],
    users: &UserNames,
    threads: &Threads,
    clock: &dyn Clock,
) -> Transcript {
    let mut messages: Vec<&Message> = pages.iter().flat_map(|page| &page.items).collect();
    messages.sort_by(|a, b| a.ts.cmp(&b.ts));
    messages.dedup_by(|a, b| a.ts == b.ts);

    let older_exist = pages.last().is_some_and(|page| page.next.is_some());
    let noun = if messages.len() == 1 {
        "message"
    } else {
        "messages"
    };
    let header = if older_exist {
        format!(
            "Showing {} {noun} (older messages exist: slack: load older messages)",
            messages.len()
        )
    } else {
        format!(
            "Showing {} {noun} (this is the start of the conversation)",
            messages.len()
        )
    };

    let mut lines: Vec<(String, Option<RowRef>)> = vec![(header, None), (String::new(), None)];
    for message in messages {
        push_message(
            &mut lines,
            "",
            message,
            message.thread_ts.clone(),
            users,
            clock,
        );
        let fetched = threads.get(&message.ts);
        if let Some(replies) = fetched {
            let mut replies: Vec<&Message> = replies
                .iter()
                .filter(|reply| reply.ts != message.ts)
                .collect();
            replies.sort_by(|a, b| a.ts.cmp(&b.ts));
            for reply in replies {
                push_message(
                    &mut lines,
                    "  > ",
                    reply,
                    Some(message.ts.clone()),
                    users,
                    clock,
                );
            }
        } else if message.reply_count > 0 {
            let noun = if message.reply_count == 1 {
                "reply"
            } else {
                "replies"
            };
            lines.push((
                format!("  > {} {noun} (slack: read thread)", message.reply_count),
                Some((message.ts.clone(), Some(message.ts.clone()))),
            ));
        }
    }

    let text = lines
        .iter()
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let index = lines.into_iter().map(|(_, row)| row).collect();
    Transcript { text, index }
}

fn push_message(
    lines: &mut Vec<(String, Option<RowRef>)>,
    prefix: &str,
    message: &Message,
    thread_ts: Option<Ts>,
    users: &UserNames,
    clock: &dyn Clock,
) {
    let row = Some((message.ts.clone(), thread_ts));
    let body = text(&message.text, users);
    let when = format_time(&message.ts, clock);
    let who = author(message, users);
    for (number, line) in body.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        let shown = if number == 0 {
            format!("{prefix}{when}  {who}  {line}")
        } else {
            format!("{prefix}    {line}")
        };
        lines.push((shown.trim_end().to_string(), row.clone()));
    }
}
