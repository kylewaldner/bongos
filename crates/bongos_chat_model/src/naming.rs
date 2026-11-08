//! Chat names. All of it is pure: the caller decides when a name applies (a name Kyle chose is
//! never replaced, see [`crate::ChatRecord::apply_auto_name`]).

use chrono::NaiveDate;

use crate::record::Agent;

/// The longest name [`from_prompt`] and [`from_title`] return, in characters.
pub const MAX_NAME_CHARS: usize = 48;

/// "<type> <agent> <date>", the name a chat keeps until it gets a better one.
pub fn default_name(chat_type: &str, agent: Agent, date: NaiveDate) -> String {
    format!("{chat_type} {} {}", agent.as_str(), date.format("%Y-%m-%d"))
}

/// The name a first prompt gives: its first non-empty line, whitespace collapsed, cut at a word
/// boundary to [`MAX_NAME_CHARS`] characters. `None` when the prompt has no text.
pub fn from_prompt(prompt: &str) -> Option<String> {
    let line = prompt.lines().find(|line| !line.trim().is_empty())?;
    Some(shorten(&collapse(line)))
}

/// The name a terminal title gives, for a chat that was only typed into. `None` when the title is
/// empty, is the program name (`program`, compared without case) or is only a spinner. A spinner
/// glyph in front of a real title is dropped.
pub fn from_title(title: &str, program: &str) -> Option<String> {
    let collapsed = collapse(title);
    let text = collapsed.trim_start_matches(is_spinner).trim_start();
    if text.is_empty() || text.eq_ignore_ascii_case(program.trim()) {
        return None;
    }
    Some(shorten(text))
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_spinner(character: char) -> bool {
    character.is_whitespace()
        || matches!(character, '\u{2800}'..='\u{28FF}')
        || "|/-\\.·•*✢✳✶✻✽●○◐◑◒◓◴◵◶◷".contains(character)
}

/// `text` (already collapsed) cut to [`MAX_NAME_CHARS`] characters at a word boundary. A first
/// word longer than that is cut in the middle.
fn shorten(text: &str) -> String {
    let mut characters = text.char_indices();
    let Some((cut, _)) = characters.nth(MAX_NAME_CHARS) else {
        return text.to_string();
    };
    let head = &text[..cut];
    if text[cut..].starts_with(' ') {
        return head.to_string();
    }
    match head.rfind(' ') {
        Some(space) => head[..space].to_string(),
        None => head.to_string(),
    }
}
