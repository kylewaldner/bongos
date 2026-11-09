//! Issue identifiers (`SEQ-121`), UUIDs and `linear.app` issue URLs.

use std::fmt;

use uuid::Uuid;

use crate::api::IssueRef;

/// A team key and an issue number, such as `SEQ-121`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identifier {
    team_key: String,
    number: u32,
}

impl Identifier {
    pub fn new(team_key: &str, number: u32) -> Option<Self> {
        if number == 0 || !valid_team_key(team_key) {
            return None;
        }
        Some(Self {
            team_key: team_key.to_ascii_uppercase(),
            number,
        })
    }

    pub fn team_key(&self) -> &str {
        &self.team_key
    }

    pub fn number(&self) -> u32 {
        self.number
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.team_key, self.number)
    }
}

fn valid_team_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    key.len() <= 10 && first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Parses `SEQ-121` (case-insensitive, surrounding whitespace ignored). The key is normalized to
/// upper case. Zero and leading-zero numbers are rejected.
pub fn parse_identifier(text: &str) -> Option<Identifier> {
    let (key, number) = text.trim().split_once('-')?;
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Identifier::new(key, number.parse().ok()?)
}

/// Parses a UUID in the hyphenated form Linear uses.
pub fn parse_uuid(text: &str) -> Option<Uuid> {
    let text = text.trim();
    if text.len() != 36 {
        return None;
    }
    Uuid::parse_str(text).ok()
}

/// A parsed `https://linear.app/<workspace>/issue/<IDENT>[/<slug>]` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueUrl {
    pub workspace: String,
    pub identifier: Identifier,
}

pub fn parse_issue_url(text: &str) -> Option<IssueUrl> {
    let rest = text.trim().strip_prefix("https://")?;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.split('/');
    if parts.next()? != "linear.app" {
        return None;
    }
    let workspace = parts.next().filter(|w| valid_workspace(w))?;
    if parts.next()? != "issue" {
        return None;
    }
    let identifier = parse_identifier(parts.next()?)?;
    // An optional slug, then at most a trailing slash.
    let slug = parts.next();
    if slug.is_some_and(|s| s.contains(' ')) || parts.any(|p| !p.is_empty()) {
        return None;
    }
    Some(IssueUrl {
        workspace: workspace.to_string(),
        identifier,
    })
}

fn valid_workspace(workspace: &str) -> bool {
    !workspace.is_empty()
        && workspace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The canonical URL of an issue (without a slug, which Linear redirects).
pub fn issue_url(workspace: &str, identifier: &Identifier) -> String {
    format!("https://linear.app/{workspace}/issue/{identifier}")
}

/// Parses whatever a user may paste for a ticket: an identifier, a UUID or an issue URL.
pub fn parse_issue_ref(text: &str) -> Option<IssueRef> {
    if let Some(id) = parse_uuid(text) {
        return Some(IssueRef::Id(id));
    }
    if let Some(identifier) = parse_identifier(text) {
        return Some(IssueRef::Identifier(identifier));
    }
    parse_issue_url(text).map(|url| IssueRef::Identifier(url.identifier))
}
