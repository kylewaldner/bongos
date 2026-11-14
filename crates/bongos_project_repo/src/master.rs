//! Master against the verified commit: what the ref watch and every landing check before they
//! go on, the recovery of a landing a crash interrupted, the adoption of an approved outside
//! commit, and the resolution of a split master. `main` only ever fast-forwards: nothing here
//! moves it backwards, adopts a commit that is not a fast-forward, or resets state.
//!
//! "Ours" is always a hash the state store recorded (the verified commit, the landing in
//! flight, a resolution), never a trailer: an outside commit that carries `Bongos-` trailers is
//! still an outside commit.

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::PersonAtKeyboard;
use bongos_work_model::commands::Context as WorkContext;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::changes::ChangeState;
use crate::create::{MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, Identity, TreeRef};
use crate::landing::{
    InFlight, LANDING_IN_FLIGHT_KEY, LandingRecord, MAIN_REF, MASTER_IS_SPLIT, RepoEvent,
};
use crate::landing_queue::{Holder, LandingLock, LockRefused};
use crate::loader::load_definition_tree;
use crate::state::StateWriter;
use crate::working_copies::WorkingCopy;

pub(crate) const MASTER_SPLIT_KEY: &str = "master_split";
pub(crate) const OUTSIDE_CHANGE_KEY: &str = "outside_change";
pub(crate) const PUSH_FAILURE_KEY: &str = "push_failure";

/// The refusal of `project: revert` for anything that is not a landing.
pub const NOT_A_LANDING: &str =
    "this commit was not a landing; to undo an outside change, land a change that reverses it";
/// Why landings are refused while an outside change waits for approval.
pub const UNAPPROVED_OUTSIDE_CHANGE: &str =
    "master has an unapproved outside change; approve or reset it";

/// Master was changed outside bongos while `change_id` was landing. The state is at
/// `verified`; `main` is `main`; before the landing both were `old`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitMaster {
    pub change_id: String,
    pub main: String,
    pub verified: String,
    pub old: String,
}

/// A commit on master that bongos did not land, with the deterministic checks' verdict on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutsideChange {
    pub commit: String,
    pub passed: bool,
    #[serde(default)]
    pub problems: Vec<String>,
}

/// Where master is against the verified commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MasterStatus {
    Clean,
    /// A landing is between its state transaction and its ref move.
    InFlight,
    Split(SplitMaster),
    /// A fast-forward of the verified commit that bongos did not land; the engine keeps the
    /// verified commit until a person approves it.
    Outside(OutsideChange),
    /// `main` is not a fast-forward of the verified commit.
    Rewritten {
        main: String,
        verified: String,
    },
}

impl MasterStatus {
    /// Why a landing is refused in this state.
    pub fn refusal(&self) -> String {
        match self {
            MasterStatus::Clean => "master is clean".to_string(),
            MasterStatus::InFlight => {
                "a landing is still in flight; reopen the project to complete it".to_string()
            }
            MasterStatus::Split(_) => MASTER_IS_SPLIT.to_string(),
            MasterStatus::Outside(_) => UNAPPROVED_OUTSIDE_CHANGE.to_string(),
            MasterStatus::Rewritten { verified, .. } => format!(
                "master was rewritten outside bongos; reset it to {verified} or run project: add existing"
            ),
        }
    }

    /// The line the landing view and the Work view show, if any.
    pub fn warning(&self) -> Option<String> {
        match self {
            MasterStatus::Clean | MasterStatus::InFlight => None,
            MasterStatus::Split(split) => Some(format!(
                "master was changed outside bongos while {} was landing: main is {}, bongos is at {}",
                split.change_id,
                short(&split.main),
                short(&split.verified)
            )),
            MasterStatus::Outside(outside) => Some(format!(
                "master was changed outside landing; using the verified commit until {} is approved",
                short(&outside.commit)
            )),
            MasterStatus::Rewritten { .. } => Some(self.refusal()),
        }
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

/// What startup found of a landing a crash interrupted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    Nothing,
    /// The landing of `commit` was completed from the recorded hashes.
    Completed {
        commit: String,
    },
    Split(SplitMaster),
}

/// The two ways a person resolves a split master.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitResolution {
    KeepBongos,
    /// Keep bongos, then bring the outside change in as an unattributed working copy that lands
    /// through the normal path.
    KeepBongosAndBringIn,
}

/// What resolving a split master made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The "ours" merge: the tree of bongos' commit, parents that commit and `outside`.
    pub merge: String,
    /// The tip of master the merge took in.
    pub outside: String,
    /// Every outside commit, saved on `refs/bongos/outside/<hash>`.
    pub saved: Vec<String>,
    pub brought_in: Option<WorkingCopy>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResolutionRecord {
    change_id: String,
    outside: String,
    saved: Vec<String>,
    keep: SplitResolution,
    by: String,
    generation: u64,
}

/// One entry of `project: show history`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub commit: String,
    pub kind: HistoryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    Landing {
        change_id: String,
        subject: String,
    },
    /// A split master's "ours" merge, which is not a landing.
    SplitResolution {
        outside: String,
    },
}

/// The ref an outside commit is saved on when a split is resolved.
pub fn outside_ref(commit: &str) -> String {
    format!("refs/bongos/outside/{commit}")
}

fn read_toml<T: for<'de> Deserialize<'de>>(text: Option<String>, what: &str) -> Result<Option<T>> {
    text.map(|text| toml::from_str(&text).with_context(|| format!("reading {what}")))
        .transpose()
}

impl ProjectRepo {
    /// The split master, while there is one.
    pub fn split_master(&self) -> Result<Option<SplitMaster>> {
        read_toml(
            self.read_state(|read| read.meta(MASTER_SPLIT_KEY))?,
            "the split master",
        )
    }

    /// The outside change of master waiting for approval, if one was seen.
    pub fn outside_change(&self) -> Result<Option<OutsideChange>> {
        read_toml(
            self.read_state(|read| read.meta(OUTSIDE_CHANGE_KEY))?,
            "the outside change",
        )
    }

    /// Why the last push of master failed, until one succeeds.
    pub fn push_failure(&self) -> Result<Option<String>> {
        self.read_state(|read| read.meta(PUSH_FAILURE_KEY))
    }

    fn in_flight(&self) -> Result<Option<InFlight>> {
        read_toml(
            self.read_state(|read| read.meta(LANDING_IN_FLIGHT_KEY))?,
            "the landing in flight",
        )
    }

    pub(crate) fn record_split(
        &self,
        writer: &StateWriter,
        change_id: &str,
        main: &str,
        verified: &str,
        old: &str,
    ) -> Result<SplitMaster> {
        let split = SplitMaster {
            change_id: change_id.to_string(),
            main: main.to_string(),
            verified: verified.to_string(),
            old: old.to_string(),
        };
        let text = toml::to_string(&split).context("serializing the split master")?;
        writer.transact(|tx| tx.set_meta(MASTER_SPLIT_KEY, &text))?;
        self.engine.emit_repo(RepoEvent::MasterSplit {
            change_id: split.change_id.clone(),
            main: split.main.clone(),
            verified: split.verified.clone(),
        });
        self.engine.signal.notify();
        Ok(split)
    }

    /// Compares `main` with the verified commit and records what it finds: a split master, or an
    /// outside commit with the deterministic checks' verdict on it. The ref watch calls it on
    /// every change of the repository's refs, and every landing before it goes on.
    pub async fn check_master(&self) -> Result<MasterStatus> {
        if let Some(split) = self.split_master()? {
            return Ok(MasterStatus::Split(split));
        }
        let repository = &self.location.repository;
        let main = self.git.rev_parse(repository, MAIN_BRANCH).await?;
        if let Some(in_flight) = self.in_flight()? {
            if main == in_flight.old || main == in_flight.commit {
                return Ok(MasterStatus::InFlight);
            }
            let writer = self.writer_or_error()?;
            let split = self.record_split(
                &writer,
                &in_flight.change_id,
                &main,
                &in_flight.commit,
                &in_flight.old,
            )?;
            return Ok(MasterStatus::Split(split));
        }
        let Some(verified) = self.verified_commit()?.map(|commit| commit.0) else {
            // Nothing landed yet: master is bongos' first commit.
            return Ok(MasterStatus::Clean);
        };
        if main == verified {
            if self.outside_change()?.is_some()
                && let Ok(writer) = self.writer_or_error()
            {
                writer.transact(|tx| tx.delete_meta(OUTSIDE_CHANGE_KEY))?;
            }
            return Ok(MasterStatus::Clean);
        }
        if !self.git.is_ancestor(repository, &verified, &main).await {
            return Ok(MasterStatus::Rewritten { main, verified });
        }
        if let Some(known) = self.outside_change()?
            && known.commit == main
        {
            return Ok(MasterStatus::Outside(known));
        }
        let problems: Vec<String> = self
            .schema_problems_at(&main)
            .await?
            .iter()
            .map(ToString::to_string)
            .collect();
        let outside = OutsideChange {
            commit: main,
            passed: problems.is_empty(),
            problems,
        };
        if let Ok(writer) = self.writer_or_error() {
            let text = toml::to_string(&outside).context("serializing the outside change")?;
            writer.transact(|tx| tx.set_meta(OUTSIDE_CHANGE_KEY, &text))?;
        }
        Ok(MasterStatus::Outside(outside))
    }

    /// Run once when the project opens, after the writer and before the loader: empties the
    /// landing queue of the former process and completes a landing a crash interrupted, by the
    /// hashes its state transaction recorded. The hooks already ran in that transaction and do
    /// not run again.
    pub async fn recover_landing(&self) -> Result<Recovery> {
        let writer = self.writer_or_error()?;
        if !writer.read(|read| read.landing_queue())?.is_empty() {
            writer.transact(|tx| tx.queue_clear())?;
        }
        let Some(in_flight) = self.in_flight()? else {
            return Ok(Recovery::Nothing);
        };
        let repository = &self.location.repository;
        let main = self.git.rev_parse(repository, MAIN_BRANCH).await?;
        if main == in_flight.old {
            self.git
                .update_ref_cas(repository, MAIN_REF, &in_flight.commit, &in_flight.old)
                .await?;
        } else if main != in_flight.commit {
            if let Some(split) = self.split_master()? {
                return Ok(Recovery::Split(split));
            }
            let split = self.record_split(
                &writer,
                &in_flight.change_id,
                &main,
                &in_flight.commit,
                &in_flight.old,
            )?;
            return Ok(Recovery::Split(split));
        }
        self.finish_checkout(&writer).await?;
        if let Some(row) = self.change(&in_flight.change_id)?
            && row.state == ChangeState::Landed
        {
            self.remove_working_copy(&row).await;
        }
        self.push_main().await;
        Ok(Recovery::Completed {
            commit: in_flight.commit,
        })
    }

    /// Takes the landing lock for work that is not a landing; fails while a landing holds it.
    fn take_lock_for(&self, holder_id: &str) -> Result<()> {
        let mut holder = self.engine.holder.lock();
        if let Some(held) = holder.as_ref() {
            if held.change_id == holder_id {
                return Ok(());
            }
            bail!("a landing is in progress; try again when it is done");
        }
        let lock = LandingLock::try_take(
            &self
                .location
                .state
                .join(bongos_project_repo_model::layout::LANDING_LOCK_FILE),
        )
        .map_err(|refused| match refused {
            LockRefused::HeldBy(pid) => anyhow!("another process holds the landing lock ({pid:?})"),
            LockRefused::Failed(error) => error,
        })?;
        *holder = Some(Holder::new(holder_id, lock));
        Ok(())
    }

    fn release_lock(&self, holder_id: &str) {
        self.engine.release(holder_id);
        self.engine.signal.notify();
    }

    /// `project: approve` of an outside commit: once its checks passed, it becomes the verified
    /// commit in one state transaction that runs the state apply hooks. A hook error adopts
    /// nothing. Needs a person at the keyboard.
    pub async fn approve_outside(&self, marker: PersonAtKeyboard) -> Result<String> {
        let writer = self.writer_or_error()?;
        let MasterStatus::Outside(outside) = self.check_master().await? else {
            bail!("master has no outside change to approve");
        };
        if !outside.passed {
            bail!(
                "the outside commit {} fails the checks: {}",
                short(&outside.commit),
                outside.problems.join("; ")
            );
        }
        const APPROVAL: &str = "approval of an outside change";
        self.take_lock_for(APPROVAL)?;
        let adopted = self.adopt_outside(&writer, &outside, marker).await;
        self.release_lock(APPROVAL);
        adopted?;
        self.engine.emit_repo(RepoEvent::VerifiedCommitChanged {
            commit: outside.commit.clone(),
        });
        self.push_main().await;
        Ok(outside.commit)
    }

    async fn adopt_outside(
        &self,
        writer: &StateWriter,
        outside: &OutsideChange,
        marker: PersonAtKeyboard,
    ) -> Result<()> {
        let author = WorkContext::at_keyboard(marker).author;
        let verified = self
            .verified_commit()?
            .context("nothing has landed in this project yet")?;
        self.ensure_definitions(writer, &verified.0).await?;
        let definitions = std::sync::Arc::new(
            load_definition_tree(
                &self.git,
                &self.location.repository,
                &TreeRef::Commit(CommitId(outside.commit.clone())),
            )
            .await?,
        );
        writer.transact_as(&author, |tx| {
            tx.set_verified_commit(&outside.commit, definitions.clone())?;
            tx.delete_meta(OUTSIDE_CHANGE_KEY)
        })?;
        self.git.update_checkout(&self.location.repository).await?;
        self.protect_master_files()
            .context("making master read-only")?;
        Ok(())
    }

    /// `project: resolve split`: keeps bongos' state over the outside commits with an "ours"
    /// merge whose first parent is bongos' commit and whose tree is exactly its tree, so `main`
    /// fast-forwards from the outside tip to it and nothing is rewritten. The outside commits are
    /// saved on `refs/bongos/outside/`. Needs a person at the keyboard.
    pub async fn resolve_split(
        &self,
        keep: SplitResolution,
        marker: PersonAtKeyboard,
        now: DateTime<Utc>,
    ) -> Result<Resolved> {
        let writer = self.writer_or_error()?;
        let split = self.split_master()?.context("master is not split")?;
        let author = WorkContext::at_keyboard(marker).author;
        self.take_lock_for(&split.change_id)?;
        let resolved = self.resolve_locked(&writer, &split, keep, &author).await;
        self.release_lock(&split.change_id);
        let (merge, outside, saved) = resolved?;
        self.engine.emit_repo(RepoEvent::VerifiedCommitChanged {
            commit: merge.clone(),
        });
        let brought_in = match keep {
            SplitResolution::KeepBongos => None,
            SplitResolution::KeepBongosAndBringIn => {
                Some(self.bring_in(&split.old, &outside, now).await?)
            }
        };
        self.push_main().await;
        Ok(Resolved {
            merge,
            outside,
            saved,
            brought_in,
        })
    }

    async fn resolve_locked(
        &self,
        writer: &StateWriter,
        split: &SplitMaster,
        keep: SplitResolution,
        author: &bongos_project_repo_model::Author,
    ) -> Result<(String, String, Vec<String>)> {
        let repository = &self.location.repository;
        let new = &split.verified;
        let tree = self.git.tree_of(repository, new).await?;
        self.ensure_definitions(writer, new).await?;
        loop {
            let tip = self.git.rev_parse(repository, MAIN_BRANCH).await?;
            let saved = self
                .git
                .commits_between(repository, &split.old, &tip)
                .await?;
            for commit in &saved {
                self.git
                    .update_ref(repository, &outside_ref(commit), commit)
                    .await?;
            }
            let message = format!("Keep bongos state over outside commit {tip}");
            let merge = self
                .git
                .commit_tree(
                    repository,
                    &tree,
                    &[new, &tip],
                    &message,
                    &Identity::default(),
                )
                .await?;
            let definitions = std::sync::Arc::new(
                load_definition_tree(
                    &self.git,
                    repository,
                    &TreeRef::Commit(CommitId(merge.clone())),
                )
                .await?,
            );
            let in_flight = toml::to_string(&InFlight {
                change_id: split.change_id.clone(),
                commit: merge.clone(),
                old: tip.clone(),
            })
            .context("serializing the landing in flight")?;
            writer.transact_as(author, |tx| {
                let record = ResolutionRecord {
                    change_id: split.change_id.clone(),
                    outside: tip.clone(),
                    saved: saved.clone(),
                    keep,
                    by: author.name.clone(),
                    generation: tx.generation()?,
                };
                tx.add_resolution(
                    &merge,
                    &toml::to_string(&record).context("serializing a resolution")?,
                )?;
                tx.set_verified_commit(&merge, definitions.clone())?;
                tx.set_meta(LANDING_IN_FLIGHT_KEY, &in_flight)
            })?;
            match self
                .git
                .update_ref_cas(repository, MAIN_REF, &merge, &tip)
                .await
            {
                Ok(()) => {}
                // `main` moved again since it was read: start over from the new tip.
                Err(_) if self.git.rev_parse(repository, MAIN_BRANCH).await? != merge => {
                    continue;
                }
                Err(_) => {}
            }
            self.finish_checkout(writer).await?;
            log::info!(
                "kept bongos over outside commit {tip}, resolved by {}",
                author.name
            );
            return Ok((merge, tip, saved));
        }
    }

    /// A working copy on the verified commit with the diff from `old` to `outside` applied,
    /// committed as an unattributed change.
    async fn bring_in(&self, old: &str, outside: &str, now: DateTime<Utc>) -> Result<WorkingCopy> {
        let author = bongos_project_repo_model::Author::unattributed(format!(
            "outside commit {}",
            short(outside)
        ));
        let name = format!("outside-{}", short(outside));
        let prefix = self.land_options().branch_prefix;
        let copy = self
            .create_working_copy(&name, &prefix, &author, now)
            .await?;
        let patch = self
            .git
            .diff_binary(&self.location.repository, old, outside)
            .await?;
        self.git.apply_index(&copy.path, patch).await?;
        self.git
            .commit_if_dirty(
                &copy.path,
                &format!("Bring in outside commit {outside}"),
                &crate::landing::author_identity(&author),
            )
            .await?;
        Ok(copy)
    }

    /// The landings, newest first, with the resolutions of split masters marked as such. Only
    /// commits the state store recorded are listed; trailers only fill in details.
    pub fn history(&self) -> Result<Vec<HistoryRow>> {
        let (landings, resolutions) =
            self.read_state(|read| Ok((read.landings()?, read.resolutions()?)))?;
        let mut rows: Vec<(u64, HistoryRow)> = Vec::new();
        for (change_id, text) in landings {
            let record: LandingRecord = toml::from_str(&text)
                .with_context(|| format!("reading the landing {change_id}"))?;
            rows.push((
                record.generation,
                HistoryRow {
                    commit: record.commit,
                    kind: HistoryKind::Landing {
                        change_id,
                        subject: record.subject,
                    },
                },
            ));
        }
        for (commit, text) in resolutions {
            let record: ResolutionRecord = toml::from_str(&text)
                .with_context(|| format!("reading the resolution {commit}"))?;
            rows.push((
                record.generation,
                HistoryRow {
                    commit,
                    kind: HistoryKind::SplitResolution {
                        outside: record.outside,
                    },
                },
            ));
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
        Ok(rows.into_iter().map(|(_, row)| row).collect())
    }

    /// Whether `project: revert` may undo `commit`: only a landing can be reverted.
    pub fn revertible(&self, commit: &str) -> Result<(), String> {
        let landed = self
            .history()
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .any(|row| row.commit == commit && matches!(row.kind, HistoryKind::Landing { .. }));
        if landed {
            Ok(())
        } else {
            Err(NOT_A_LANDING.to_string())
        }
    }

    /// Pushes master to the remote, if one is set. A failure is recorded for the backup status
    /// and retried with the next landing; it never fails a landing.
    pub async fn push_main(&self) {
        let Some(remote) = self.location.remote.clone() else {
            return;
        };
        let pushed = self
            .git
            .push(
                &self.location.repository,
                &remote,
                "refs/heads/main:refs/heads/main",
            )
            .await;
        let Ok(writer) = self.writer_or_error() else {
            return;
        };
        let recorded = match pushed {
            Ok(()) => writer.transact(|tx| {
                if tx.meta(PUSH_FAILURE_KEY)?.is_some() {
                    tx.delete_meta(PUSH_FAILURE_KEY)?;
                }
                Ok(())
            }),
            Err(error) => {
                log::warn!("pushing master to {remote}: {error}");
                writer.transact(|tx| tx.set_meta(PUSH_FAILURE_KEY, &error.to_string()))
            }
        };
        if let Err(error) = recorded {
            log::error!("recording the push of master: {error:#}");
        }
    }
}
