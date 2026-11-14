//! `project: show history` and `project: revert`. History lists the changes that ended, landed
//! or abandoned, newest first, through a filter; the filter and the change ids come from other
//! processes as well as from the UI, so both are checked before they are used. A revert makes a
//! new change that undoes a landed one; it is checked and landed like any change, and nothing
//! in master is ever rewritten.

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::{Author, AuthorKind, ChangeMeta};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::changes::{ChangeRow, ChangeState, same_kind};
use crate::create::ProjectRepo;
use crate::diffs::check_change_id;
use crate::landing::author_identity;
use crate::master::HistoryKind;
use crate::working_copies::WorkingCopy;

/// The most days a date filter reaches back or forward; more is an error, not a wrap.
pub const MAX_HISTORY_DAYS: u32 = 36_500;
const MAX_TASK_ID_LEN: usize = 128;

/// The kind of author a filter asks for, as it is written on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthorFilter {
    Person,
    Agent,
    Workflow,
    Healer,
    Unattributed,
}

impl AuthorFilter {
    fn kind(self) -> AuthorKind {
        match self {
            AuthorFilter::Person => AuthorKind::Person,
            AuthorFilter::Agent => AuthorKind::Agent,
            AuthorFilter::Workflow => AuthorKind::Workflow,
            AuthorFilter::Healer => AuthorKind::Healer,
            AuthorFilter::Unattributed => AuthorKind::Unattributed,
        }
    }
}

/// Which ended changes a history lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndedState {
    #[default]
    Any,
    Landed,
    Abandoned,
}

/// What `project: show history` lists; every field set must match.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HistoryFilter {
    /// Only changes that touched this task (by id). Needs the change's commit, so an abandoned
    /// change never matches.
    pub task: Option<String>,
    pub author: Option<AuthorFilter>,
    /// Only changes that ended at most this many days before the time the history is read at.
    pub within_days: Option<u32>,
    /// Only changes that ended at least this many days before it.
    pub older_than_days: Option<u32>,
    pub state: EndedState,
}

impl HistoryFilter {
    /// Refuses a filter that could not have come from the UI: a task id that is not a plain id
    /// (it is compared with ids and never read as a path, but a peer must not be able to make it
    /// anything else), a date range with a bound over [`MAX_HISTORY_DAYS`] or an empty range.
    pub fn validate(&self) -> Result<()> {
        if let Some(task) = &self.task {
            let plain = !task.is_empty()
                && task.len() <= MAX_TASK_ID_LEN
                && task
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
            if !plain {
                bail!("{task:?} is not a task id");
            }
        }
        for days in [self.within_days, self.older_than_days]
            .into_iter()
            .flatten()
        {
            if days > MAX_HISTORY_DAYS {
                bail!("{days} days is more than the {MAX_HISTORY_DAYS} a history reaches");
            }
        }
        if let (Some(within), Some(older)) = (self.within_days, self.older_than_days)
            && older > within
        {
            bail!("no change is at most {within} days and at least {older} days old");
        }
        Ok(())
    }

    fn matches(&self, row: &ChangeRow, now: DateTime<Utc>) -> bool {
        let state = match self.state {
            EndedState::Any => matches!(row.state, ChangeState::Landed | ChangeState::Abandoned),
            EndedState::Landed => row.state == ChangeState::Landed,
            EndedState::Abandoned => row.state == ChangeState::Abandoned,
        };
        let age = now.signed_duration_since(row.updated);
        state
            && self
                .author
                .is_none_or(|wanted| same_kind(wanted.kind(), row.author.kind()))
            && self
                .within_days
                .is_none_or(|days| age <= Duration::days(i64::from(days)))
            && self
                .older_than_days
                .is_none_or(|days| age >= Duration::days(i64::from(days)))
    }
}

/// One ended change in a history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub change_id: String,
    pub name: String,
    /// The landing's subject; the working copy's name for an abandoned change.
    pub subject: String,
    pub author: Author,
    pub state: ChangeState,
    /// The commit in master, for a landed change.
    pub commit: Option<String>,
    pub ended: DateTime<Utc>,
    /// The tasks the change touched, filled only when the filter names a task.
    pub tasks: Vec<String>,
}

impl ProjectRepo {
    /// The changes that ended and pass `filter`, newest first, with their ages taken at `now`.
    pub async fn change_history(
        &self,
        filter: &HistoryFilter,
        now: DateTime<Utc>,
    ) -> Result<Vec<HistoryEntry>> {
        filter.validate()?;
        let subjects: std::collections::HashMap<String, String> = self
            .history()?
            .into_iter()
            .filter_map(|row| match row.kind {
                HistoryKind::Landing { change_id, subject } => Some((change_id, subject)),
                HistoryKind::SplitResolution { .. } => None,
            })
            .collect();
        let mut rows: Vec<ChangeRow> = self
            .read_state(|read| read.changes())?
            .into_iter()
            .filter(|row| filter.matches(row, now))
            .collect();
        rows.sort_by(|a, b| {
            b.updated
                .cmp(&a.updated)
                .then_with(|| a.change_id.cmp(&b.change_id))
        });
        let mut entries = Vec::new();
        for row in rows {
            let mut tasks = Vec::new();
            if let Some(task) = &filter.task {
                if row.state != ChangeState::Landed {
                    continue;
                }
                tasks = self
                    .task_diff_of(&row)
                    .await
                    .map(|diff| diff.tasks())
                    .unwrap_or_default();
                if !tasks.contains(task) {
                    continue;
                }
            }
            entries.push(HistoryEntry {
                subject: subjects
                    .get(&row.change_id)
                    .cloned()
                    .unwrap_or_else(|| row.name.clone()),
                change_id: row.change_id,
                name: row.name,
                author: row.author,
                state: row.state,
                commit: row.commit,
                ended: row.updated,
                tasks,
            });
        }
        Ok(entries)
    }

    /// `project: revert`: makes a working copy that undoes the landed change `change_id` on top
    /// of the verified commit, committed as `author`, and returns it. The caller lands it like
    /// any change; it goes through every check and approval. Refused when the change is
    /// unknown, did not land, was not a landing of bongos (an outside commit is not revertible
    /// here), has no single parent, when a later change touches the same files, or when it was
    /// already undone. Nothing is rewritten.
    pub async fn revert(
        &self,
        change_id: &str,
        author: &Author,
        now: DateTime<Utc>,
    ) -> Result<WorkingCopy> {
        check_change_id(change_id)?;
        let row = self
            .change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
        if row.state != ChangeState::Landed {
            bail!(
                "change {} is not landed, so there is nothing to revert; abandon it instead",
                row.name
            );
        }
        let commit = row
            .commit
            .clone()
            .ok_or_else(|| anyhow!("change {} has no commit", row.name))?;
        self.revertible(&commit).map_err(|reason| anyhow!(reason))?;
        let repository = &self.location.repository;
        let parents = self.git.parents_of(repository, &commit).await?;
        if parents.len() != 1 {
            bail!("{commit} is not a plain change; undo it by hand");
        }
        let verified = self.shown_commit().await?.0;
        let tree = self
            .git
            .revert_tree(repository, &commit, &verified)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "later changes touch the same files as {}; undo them first or edit by hand",
                    row.name
                )
            })?;
        if tree == self.git.tree_of(repository, &verified).await? {
            bail!("{} is already undone", row.name);
        }
        let short = &commit[..commit.len().min(8)];
        let prefix = self.land_options().branch_prefix;
        let copy = self
            .create_working_copy(&format!("revert-{short}"), &prefix, author, now)
            .await?;
        let made = async {
            let patch = self.git.diff_binary(repository, &verified, &tree).await?;
            self.git.apply_index(&copy.path, patch).await?;
            self.git
                .commit_if_dirty(
                    &copy.path,
                    &format!("Revert \"{}\"\n\nThis undoes {commit}.", row.name),
                    &author_identity(author),
                )
                .await
                .context("committing the revert")?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = made {
            self.abandon_now(&copy.change_id, now).await.ok();
            return Err(error);
        }
        Ok(copy)
    }

    /// The meta a revert of `change_id` lands with.
    pub fn revert_meta(&self, change_id: &str, author: &Author) -> Result<ChangeMeta> {
        check_change_id(change_id)?;
        let subject = self
            .history()?
            .into_iter()
            .find_map(|row| match row.kind {
                HistoryKind::Landing {
                    change_id: id,
                    subject,
                } if id == change_id => Some(subject),
                _ => None,
            })
            .ok_or_else(|| anyhow!("there is no landing of change {change_id}"))?;
        Ok(ChangeMeta::new(
            format!("Revert \"{subject}\""),
            author.clone(),
        ))
    }
}
