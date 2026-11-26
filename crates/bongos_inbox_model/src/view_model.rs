//! Pure view model for the inbox list: rows, grouping, ordering, filters,
//! unread count, age text and action resolution. It reads no files and no
//! clock; callers pass a snapshot and the current time.

use crate::maildir::{Entry, ParseResult};
use chrono::{DateTime, Utc};
use std::cmp::Reverse;
use std::collections::HashSet;

pub const OPEN: &str = "open";
pub const DISMISS: &str = "dismiss";
pub const UNREADABLE_KIND: &str = "unreadable";

/// One scanned entry with its parse result.
pub type Snapshot = [(Entry, ParseResult)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRow {
    pub unique: String,
    pub inode: u64,
    pub kind: String,
    pub source: String,
    /// `group` if set, else `source`; `None` for an unreadable entry.
    pub group_key: Option<String>,
    pub summary: String,
    pub read: bool,
    pub created: DateTime<Utc>,
    pub actions: Vec<String>,
    /// The item's target no longer exists. The row shows `stale` after the kind and stays
    /// openable and dismissable.
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRow {
    pub key: String,
    pub source: String,
    /// The newest member's summary.
    pub summary: String,
    pub count: usize,
    pub unread: usize,
    pub created: DateTime<Utc>,
    /// Newest first.
    pub members: Vec<ItemRow>,
}

impl GroupRow {
    /// `<source>: <n> items (<u> unread)`.
    pub fn label(&self) -> String {
        format!(
            "{}: {} items ({} unread)",
            self.source, self.count, self.unread
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Item(ItemRow),
    Group(GroupRow),
}

impl Row {
    pub fn created(&self) -> DateTime<Utc> {
        match self {
            Row::Item(item) => item.created,
            Row::Group(group) => group.created,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Kind(String),
    Source(String),
}

impl Filter {
    fn matches(&self, row: &ItemRow) -> bool {
        match self {
            Filter::All => true,
            Filter::Kind(kind) => &row.kind == kind,
            Filter::Source(source) => &row.source == source,
        }
    }
}

/// Items in `new/` plus items in `cur/` without the `S` flag.
pub fn unread_count(snapshot: &Snapshot) -> usize {
    snapshot
        .iter()
        .filter(|(entry, _)| !entry.is_read())
        .count()
}

fn item_row(
    entry: &Entry,
    parsed: &ParseResult,
    known: &dyn Fn(&str) -> bool,
    stale: &dyn Fn(&Entry) -> bool,
) -> ItemRow {
    let read = entry.is_read();
    let stale = stale(entry);
    match parsed.as_ref() {
        Ok(item) => ItemRow {
            unique: entry.unique.clone(),
            inode: entry.inode,
            kind: item.kind.clone(),
            source: item.source.clone(),
            group_key: Some(item.group.clone().unwrap_or_else(|| item.source.clone())),
            summary: item.summary.clone(),
            read,
            created: item.created,
            actions: resolve_actions(&item.actions, known),
            stale,
        },
        Err(_) => ItemRow {
            unique: entry.unique.clone(),
            inode: entry.inode,
            kind: UNREADABLE_KIND.to_string(),
            source: String::new(),
            group_key: None,
            summary: entry.file_name(),
            read,
            created: DateTime::<Utc>::from(entry.mtime),
            actions: vec![OPEN.to_string(), DISMISS.to_string()],
            stale,
        },
    }
}

/// Builds the visible rows: filter, order newest first, then collapse items
/// sharing a group key into one row at the position of the newest member.
pub fn rows(snapshot: &Snapshot, filter: &Filter, known: &dyn Fn(&str) -> bool) -> Vec<Row> {
    rows_marked(snapshot, filter, known, &|_| false)
}

/// [`rows`], with each item row's `stale` set by `stale`.
pub fn rows_marked(
    snapshot: &Snapshot,
    filter: &Filter,
    known: &dyn Fn(&str) -> bool,
    stale: &dyn Fn(&Entry) -> bool,
) -> Vec<Row> {
    let mut items: Vec<ItemRow> = snapshot
        .iter()
        .map(|(entry, parsed)| item_row(entry, parsed, known, stale))
        .filter(|row| filter.matches(row))
        .collect();
    items.sort_by(|a, b| (Reverse(a.created), &a.unique).cmp(&(Reverse(b.created), &b.unique)));

    let mut out: Vec<Row> = Vec::new();
    for item in items {
        let Some(key) = item.group_key.clone() else {
            out.push(Row::Item(item));
            continue;
        };
        let existing = out.iter_mut().find_map(|row| match row {
            Row::Item(first) if first.group_key.as_deref() == Some(&key) => Some(row),
            Row::Group(group) if group.key == key => Some(row),
            _ => None,
        });
        match existing {
            None => out.push(Row::Item(item)),
            Some(row) => {
                let members = match std::mem::replace(row, Row::Item(item.clone())) {
                    Row::Item(first) => vec![first, item],
                    Row::Group(mut group) => {
                        group.members.push(item);
                        group.members
                    }
                };
                *row = Row::Group(group_row(key, members));
            }
        }
    }
    out
}

fn group_row(key: String, members: Vec<ItemRow>) -> GroupRow {
    // Members arrive newest first.
    let newest = &members[0];
    GroupRow {
        key,
        source: newest.source.clone(),
        summary: newest.summary.clone(),
        count: members.len(),
        unread: members.iter().filter(|member| !member.read).count(),
        created: newest.created,
        members,
    }
}

/// The filter choices present in the snapshot, sorted and deduplicated:
/// `all`, `kind: <k>` and `source: <s>`.
pub fn filter_candidates(snapshot: &Snapshot) -> Vec<(String, Filter)> {
    let mut out: Vec<(String, Filter)> = vec![("all".to_string(), Filter::All)];
    let mut kinds = std::collections::BTreeSet::new();
    let mut sources = std::collections::BTreeSet::new();
    for (_, parsed) in snapshot {
        match parsed.as_ref() {
            Ok(item) => {
                kinds.insert(item.kind.clone());
                sources.insert(item.source.clone());
            }
            Err(_) => {
                kinds.insert(UNREADABLE_KIND.to_string());
            }
        }
    }
    out.extend(
        kinds
            .into_iter()
            .map(|kind| (format!("kind: {kind}"), Filter::Kind(kind))),
    );
    out.extend(
        sources
            .into_iter()
            .map(|source| (format!("source: {source}"), Filter::Source(source))),
    );
    out
}

/// `now` under a minute, then `3m`, `2h`, `4d`. Future times clamp to `now`.
pub fn age_text(created: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - created).num_seconds().max(0);
    match seconds {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Keeps the listed actions that are registered, then appends `open` and
/// `dismiss` when absent.
pub fn resolve_actions(actions: &[String], known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = actions
        .iter()
        .filter(|action| known(action) && seen.insert(action.as_str()))
        .cloned()
        .collect();
    for always in [OPEN, DISMISS] {
        if !out.iter().any(|action| action == always) {
            out.push(always.to_string());
        }
    }
    out
}

/// The first listed action that is registered.
pub fn primary_action(actions: &[String], known: &dyn Fn(&str) -> bool) -> Option<String> {
    actions.iter().find(|action| known(action)).cloned()
}

#[cfg(test)]
#[path = "view_model_tests.rs"]
mod tests;
