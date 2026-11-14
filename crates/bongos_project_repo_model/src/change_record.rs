//! The record a landed change carries: the subject, body and notes its author wrote, who the
//! author is, and the git trailers the landing adds to the commit message. Trailers only describe
//! a landing that the state store already knows about; nothing ever recognizes a landing by them.

use serde::{Deserialize, Serialize};

use crate::{Author, AuthorKind};

/// The prefix of every trailer a landing writes. A caller's text never carries one: any line of
/// the subject, body or notes that starts with it is dropped before the message is built.
pub const TRAILER_PREFIX: &str = "Bongos-";

/// The author's judgment of whether a change is safe for running work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityMark {
    pub safe: bool,
    pub reason: String,
}

/// What the author of a change says about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeMeta {
    pub subject: String,
    #[serde(default)]
    pub body: String,
    /// One paragraph checks can read.
    #[serde(default)]
    pub notes: String,
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<CompatibilityMark>,
}

impl ChangeMeta {
    pub fn new(subject: impl Into<String>, author: Author) -> Self {
        Self {
            subject: subject.into(),
            body: String::new(),
            notes: String::new(),
            author,
            compatibility: None,
        }
    }
}

/// What the landing adds to the author's text: the change id, the outcomes of the checks on the
/// landed tree, and the tree itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingFacts<'a> {
    pub change_id: &'a str,
    /// `(check name, outcome word)`, in the order the checks ran.
    pub checks: &'a [(String, String)],
    pub tree: &'a str,
}

/// The word a trailer uses for an author kind. A claimed person is written as a person: the
/// trailer only labels, it never attests.
pub fn author_kind_word(kind: AuthorKind) -> &'static str {
    match kind {
        AuthorKind::Person | AuthorKind::PersonClaimed => "person",
        AuthorKind::Agent => "agent",
        AuthorKind::Workflow => "workflow",
        AuthorKind::Healer => "healer",
        AuthorKind::Unattributed => "unattributed",
    }
}

/// `text` without the lines that look like a landing's trailers.
pub fn strip_trailers(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with(TRAILER_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The commit message of a landing: the subject, the body, then one block of trailers.
pub fn commit_message(meta: &ChangeMeta, facts: &LandingFacts) -> String {
    let subject = strip_trailers(&meta.subject)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let subject = if subject.is_empty() {
        format!("change {}", facts.change_id)
    } else {
        subject
    };
    let body = strip_trailers(&meta.body);
    let notes = strip_trailers(&meta.notes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let author = &meta.author;
    let mut trailers = Vec::new();
    if !notes.is_empty() {
        trailers.push(format!("Notes: {notes}"));
    }
    trailers.push(format!("Bongos-Change: {}", facts.change_id));
    trailers.push(format!(
        "Bongos-Author-Kind: {}",
        author_kind_word(author.kind())
    ));
    trailers.push(format!("Bongos-Author: {}", one_line(&author.name)));
    if matches!(author.kind(), AuthorKind::Workflow | AuthorKind::Healer) {
        if let Some(run) = &author.run {
            trailers.push(format!("Bongos-Run: {}", one_line(run)));
        }
        if let Some(step) = &author.step {
            trailers.push(format!("Bongos-Step: {}", one_line(step)));
        }
    }
    if let Some((task, number)) = &author.proposal {
        trailers.push(format!("Bongos-Proposal: {task}#{number}"));
    }
    if let Some(mark) = &meta.compatibility {
        let word = if mark.safe { "safe" } else { "unsafe" };
        trailers.push(format!(
            "Bongos-Compatibility: {word}; {}",
            one_line(&mark.reason)
        ));
    }
    if !facts.checks.is_empty() {
        let checks = facts
            .checks
            .iter()
            .map(|(name, outcome)| format!("{name}={outcome}"))
            .collect::<Vec<_>>()
            .join(", ");
        trailers.push(format!("Bongos-Checks: {checks}"));
    }
    trailers.push(format!("Bongos-Tree: {}", facts.tree));

    let mut message = subject;
    if !body.is_empty() {
        message.push_str("\n\n");
        message.push_str(&body);
    }
    message.push_str("\n\n");
    message.push_str(&trailers.join("\n"));
    message.push('\n');
    message
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The trailers of the last paragraph of `message`, as `(key, value)` pairs in order. Only for
/// showing the details of a commit the state store lists as a landing.
pub fn parse_trailers(message: &str) -> Vec<(String, String)> {
    let last = message.trim_end().rsplit("\n\n").next().unwrap_or_default();
    last.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(": ")?;
            let valid =
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            valid.then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}
