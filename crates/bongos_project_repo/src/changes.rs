//! Changes: the rows of the `changes` table, one per working copy, and the pure rules over them
//! (the filter, the stale flag). A change is `Open` until it is requested for landing, `Requested`
//! while the landing engine has it, `Landed` once its commit is in master and `Abandoned` after
//! `project: abandon`. Rows of landed and abandoned changes are kept.

use anyhow::{Context as _, Result, anyhow};
use bongos_project_repo_model::{Author, AuthorKind};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlez::connection::Connection;

/// Where a change is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeState {
    /// Until the change is requested for landing.
    Open,
    /// In or waiting for the landing engine.
    Requested,
    /// Its commit is in master.
    Landed,
    /// Removed by `project: abandon`.
    Abandoned,
}

/// One change as the `changes` table holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    pub change_id: String,
    /// The working copy's name, without the branch prefix.
    pub name: String,
    pub branch: String,
    pub author: Author,
    pub created: DateTime<Utc>,
    /// The last time the change was touched; [`Self::created`] until then.
    pub updated: DateTime<Utc>,
    pub state: ChangeState,
    /// The commit in master, once landed.
    pub commit: Option<String>,
    /// The commits the landing engine made on the branch for amending checks, oldest first.
    /// A commit is an amendment because it is listed here, never because of its message.
    pub amend_commits: Vec<AmendCommit>,
}

/// One commit an amending check made on a change's branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmendCommit {
    pub commit: String,
    /// The name of the amending check that made it.
    pub check: String,
}

impl ChangeRow {
    /// Whether the change is open or requested and has been idle for `stale_after_days` days or
    /// more at `now`. Nothing deletes a stale change; the views only show it as stale.
    pub fn is_stale(&self, now: DateTime<Utc>, stale_after_days: u32) -> bool {
        matches!(self.state, ChangeState::Open | ChangeState::Requested)
            && now.signed_duration_since(self.updated)
                >= Duration::days(i64::from(stale_after_days))
    }
}

/// Which changes [`crate::ProjectRepo::changes`] lists; every field set must match.
#[derive(Clone, Debug, Default)]
pub struct ChangeFilter {
    pub author_kind: Option<AuthorKind>,
    pub author_name: Option<String>,
    pub run: Option<String>,
    /// `None` lists the `Open` and `Requested` changes only.
    pub state: Option<ChangeState>,
}

impl ChangeFilter {
    pub fn matches(&self, row: &ChangeRow) -> bool {
        let state_matches = match self.state {
            Some(state) => row.state == state,
            None => matches!(row.state, ChangeState::Open | ChangeState::Requested),
        };
        state_matches
            && self
                .author_kind
                .is_none_or(|kind| same_kind(kind, row.author.kind()))
            && self
                .author_name
                .as_ref()
                .is_none_or(|name| *name == row.author.name)
            && self
                .run
                .as_ref()
                .is_none_or(|run| row.author.run.as_ref() == Some(run))
    }
}

/// A person and a claimed person are the same kind in storage.
pub(crate) fn same_kind(wanted: AuthorKind, found: AuthorKind) -> bool {
    let normal = |kind| match kind {
        AuthorKind::PersonClaimed => AuthorKind::Person,
        other => other,
    };
    normal(wanted) == normal(found)
}

#[derive(Serialize, Deserialize)]
struct ChangeRecord {
    name: String,
    state: ChangeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    updated: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    amend_commits: Vec<AmendCommit>,
}

fn time_text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text)
        .with_context(|| format!("reading the time {text:?}"))?
        .with_timezone(&Utc))
}

type Raw = (String, String, String, String, Option<String>);

const SELECT: &str = "SELECT change_id, branch, author, created, meta_toml FROM changes";

fn row_from(raw: Raw) -> Result<ChangeRow> {
    let (change_id, branch, author, created, meta) = raw;
    let author: Author =
        toml::from_str(&author).with_context(|| format!("reading the author of {change_id}"))?;
    let created = parse_time(&created)?;
    let meta = match meta {
        Some(text) => toml::from_str::<ChangeRecord>(&text)
            .with_context(|| format!("reading the record of change {change_id}"))?,
        None => ChangeRecord {
            name: branch.clone(),
            state: ChangeState::Open,
            commit: None,
            updated: time_text(created),
            amend_commits: Vec::new(),
        },
    };
    Ok(ChangeRow {
        change_id,
        name: meta.name,
        branch,
        author,
        created,
        updated: parse_time(&meta.updated)?,
        state: meta.state,
        commit: meta.commit,
        amend_commits: meta.amend_commits,
    })
}

pub(crate) fn read_change(connection: &Connection, change_id: &str) -> Result<Option<ChangeRow>> {
    connection.select_row_bound::<&str, Raw>(&format!("{SELECT} WHERE change_id = ?"))?(change_id)?
        .map(row_from)
        .transpose()
}

pub(crate) fn read_changes(connection: &Connection) -> Result<Vec<ChangeRow>> {
    connection.select::<Raw>(&format!("{SELECT} ORDER BY created, change_id"))?()?
        .into_iter()
        .map(row_from)
        .collect()
}

/// Inserts `row`, or replaces the row with its id.
pub(crate) fn write_change(connection: &Connection, row: &ChangeRow) -> Result<()> {
    let author = toml::to_string(&row.author).context("serializing a change author")?;
    let meta = toml::to_string(&ChangeRecord {
        name: row.name.clone(),
        state: row.state,
        commit: row.commit.clone(),
        updated: time_text(row.updated),
        amend_commits: row.amend_commits.clone(),
    })
    .context("serializing a change record")?;
    connection.exec_bound::<(&str, &str, String, String, String)>(
        "INSERT INTO changes (change_id, branch, author, created, meta_toml)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (change_id) DO UPDATE SET branch = excluded.branch,
             author = excluded.author, created = excluded.created,
             meta_toml = excluded.meta_toml",
    )?((
        &row.change_id,
        &row.branch,
        author,
        time_text(row.created),
        meta,
    ))
    .map_err(|error| anyhow!("writing change {}: {error:#}", row.change_id))
}
