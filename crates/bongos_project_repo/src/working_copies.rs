//! Working copies: a branch `<prefix><name>` plus, while checked out, a worktree at
//! `<working_copies>/<name>`, both made from the verified commit, and the `changes` row that
//! records them. Also what protects master: its checkout is read-only on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::Author;
use bongos_work_model::new_id;
use bongos_work_model::slug::slugify;
use chrono::{DateTime, Utc};

use crate::changes::{ChangeFilter, ChangeRow, ChangeState};
use crate::create::ProjectRepo;
use crate::landing::LandingResult;

/// A working copy that is checked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingCopy {
    pub path: PathBuf,
    pub branch: String,
    pub change_id: String,
}

/// Why a change could not be abandoned.
#[derive(Debug, thiserror::Error)]
pub enum AbandonRefused {
    /// The change holds the landing lock.
    #[error("the change is being landed")]
    Landing,
    /// Too late: the change landed as `commit`.
    #[error("the change already landed as {commit}")]
    Landed { commit: String },
    #[error("there is no change {0}")]
    Unknown(String),
    #[error("{0}")]
    Failed(String),
}

const ADJECTIVES: [&str; 16] = [
    "amber", "brisk", "calm", "deft", "eager", "fond", "gentle", "hardy", "ivory", "jolly", "keen",
    "lucid", "mellow", "nimble", "plucky", "quiet",
];
const NOUNS: [&str; 16] = [
    "badger", "cedar", "dune", "egret", "fjord", "grove", "heron", "island", "juniper", "kestrel",
    "lagoon", "meadow", "otter", "pebble", "quail", "willow",
];

/// A short name for a working copy: `<adjective>-<noun>-<4 hex>`, from `entropy`.
pub fn generated_name(entropy: u64) -> String {
    let adjective = ADJECTIVES[(entropy % 16) as usize];
    let noun = NOUNS[((entropy >> 4) % 16) as usize];
    format!("{adjective}-{noun}-{:04x}", (entropy >> 8) & 0xffff)
}

/// A name for a working copy that nothing else has.
pub fn fresh_name() -> String {
    generated_name(uuid::Uuid::new_v4().as_u128() as u64)
}

/// The name `text` becomes: lowercase letters, digits and dashes. Empty text is refused.
pub fn clean_name(text: &str) -> Result<String> {
    if text.trim().is_empty() {
        bail!("a working copy needs a name");
    }
    Ok(slugify(text))
}

impl ProjectRepo {
    /// Where the working copy `name` is checked out.
    pub fn working_copy_path(&self, name: &str) -> PathBuf {
        self.location.working_copies.join(name)
    }

    /// Makes the branch `<branch_prefix><name>` and its worktree from the verified commit, and
    /// records the change as `Open`. A second call with another name makes an independent one.
    pub async fn create_working_copy(
        &self,
        name: &str,
        branch_prefix: &str,
        author: &Author,
        now: DateTime<Utc>,
    ) -> Result<WorkingCopy> {
        let name = clean_name(name)?;
        let writer = self
            .state
            .as_ref()
            .and_then(|state| state.writer())
            .context("this project is open in another bongos process; work there or close it")?;
        let branch = format!("{branch_prefix}{name}");
        let path = self.working_copy_path(&name);
        let repository = &self.location.repository;
        if self.git.branch_exists(repository, &branch).await || path.exists() {
            bail!("there is already a working copy named {name}");
        }
        let commit = self.shown_commit().await?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        self.git
            .worktree_add(repository, &path, &branch, &commit.0)
            .await?;
        let change_id = new_id();
        let row = ChangeRow {
            change_id: change_id.clone(),
            name,
            branch: branch.clone(),
            author: author.clone(),
            created: now,
            updated: now,
            state: ChangeState::Open,
            commit: None,
            amend_commits: Vec::new(),
        };
        if let Err(error) = writer.transact(|tx| tx.set_change(&row)) {
            self.git.worktree_remove(repository, &path).await.ok();
            self.git.branch_delete(repository, &branch).await.ok();
            return Err(error);
        }
        Ok(WorkingCopy {
            path,
            branch,
            change_id,
        })
    }

    pub(crate) fn read_state<T>(
        &self,
        f: impl FnOnce(&crate::StateRead) -> Result<T>,
    ) -> Result<T> {
        self.state
            .as_ref()
            .context("the project's state store is not open")?
            .read(f)
    }

    /// The changes `filter` lets through, oldest first.
    pub fn changes(&self, filter: &ChangeFilter) -> Result<Vec<ChangeRow>> {
        let rows = self.read_state(|read| read.changes())?;
        Ok(rows.into_iter().filter(|row| filter.matches(row)).collect())
    }

    pub fn change(&self, change_id: &str) -> Result<Option<ChangeRow>> {
        self.read_state(|read| read.change(change_id))
    }

    /// The result of `change_id` when it is known at once: `Landed` and `Abandoned` changes and
    /// an `Open` change that was never requested. `None` for a `Requested` change, which has to
    /// wait for the landing engine.
    pub fn landing_result(&self, change_id: &str) -> Result<Option<LandingResult>> {
        let row = self
            .change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
        Ok(match row.state {
            ChangeState::Landed => Some(LandingResult::Landed {
                commit: row.commit.unwrap_or_default(),
            }),
            ChangeState::Abandoned => Some(LandingResult::Abandoned),
            ChangeState::Open => Some(LandingResult::NotRequested),
            ChangeState::Requested => None,
        })
    }

    /// Marks the row of `change_id` `Abandoned`, then removes its worktree and its branch. Master
    /// is untouched. Abandoning an abandoned change only removes what is left of it.
    pub fn abandon(
        &self,
        change_id: &str,
        cx: &gpui::App,
    ) -> gpui::Task<Result<(), AbandonRefused>> {
        let repo = self.clone();
        let change_id = change_id.to_string();
        gpui::AppContext::background_spawn(cx, async move {
            repo.abandon_now(&change_id, Utc::now()).await
        })
    }

    /// [`Self::abandon`] without an app, at `now`.
    pub async fn abandon_now(
        &self,
        change_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), AbandonRefused> {
        let failed = |error: anyhow::Error| AbandonRefused::Failed(format!("{error:#}"));
        let writer = self
            .state
            .as_ref()
            .and_then(|state| state.writer())
            .ok_or_else(|| {
                AbandonRefused::Failed(
                    "this project is open in another bongos process; work there or close it"
                        .to_string(),
                )
            })?;
        // The row is marked first, in one transaction that reads it again, so a landing can
        // never land the change after this: its own state transaction refuses a row that is no
        // longer `Requested`, and a landing that committed first leaves the row `Landed` here.
        let marked = writer
            .transact(|tx| {
                let Some(mut row) = tx.change(change_id)? else {
                    return Ok(Err(AbandonRefused::Unknown(change_id.to_string())));
                };
                match row.state {
                    ChangeState::Landed => {
                        return Ok(Err(AbandonRefused::Landed {
                            commit: row.commit.unwrap_or_default(),
                        }));
                    }
                    ChangeState::Abandoned => return Ok(Ok(row)),
                    ChangeState::Open | ChangeState::Requested => {}
                }
                if self.engine.holder_id().as_deref() == Some(change_id) {
                    return Ok(Err(AbandonRefused::Landing));
                }
                row.state = ChangeState::Abandoned;
                row.updated = now;
                tx.queue_remove(change_id)?;
                if tx.staged(change_id)?.is_some() {
                    tx.drop_staged(change_id)?;
                }
                tx.set_change(&row)?;
                Ok(Ok(row))
            })
            .map_err(failed)?;
        self.engine.signal.notify();
        let row = marked?;
        let repository = &self.location.repository;
        let path = self.working_copy_path(&row.name);
        // A landing that was checking the working copy out when it was abandoned holds git's
        // lock on it until it notices; wait that out.
        let mut waited = 0;
        loop {
            match self.git.worktree_remove(repository, &path).await {
                Ok(()) => break,
                Err(error) if waited < 100 && error.to_string().contains("locked") => {
                    waited += 1;
                    crate::landing::backoff(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => return Err(failed(error.into())),
            }
        }
        if self.git.branch_exists(repository, &row.branch).await {
            self.git
                .branch_delete(repository, &row.branch)
                .await
                .map_err(|error| failed(error.into()))?;
        }
        Ok(())
    }

    /// Makes every file of the checkout `repository/` read-only, so a save that bypassed the
    /// offer of a working copy fails. Call after every update of the checkout: git recreates
    /// the files it changes.
    pub fn protect_master_files(&self) -> std::io::Result<()> {
        protect_files(&self.location.repository)
    }
}

/// Clears the write bits of every file under `directory`, leaving `.git` alone.
pub fn protect_files(directory: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if entry.file_name() != ".git" {
                protect_files(&path)?;
            }
        } else if kind.is_file() {
            let mut permissions = std::fs::metadata(&path)?.permissions();
            if !permissions.readonly() {
                permissions.set_readonly(true);
                std::fs::set_permissions(&path, permissions)?;
            }
        }
    }
    Ok(())
}
