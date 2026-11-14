//! Amending checks, which run first among a landing's checks (step 3), and dropping their
//! commits again. The checks run in a scratch checkout of the change's commit under
//! `state/amending/<name>` (never in the working copy, so what a person types meanwhile is
//! neither committed nor discarded). The engine diffs what a check wrote against the author's
//! tree, refuses an amendment outside its bounds with no verdict and commits nothing then, and
//! otherwise commits it as "amend: <check> <message>", records it in `changes.amend_commits`,
//! fast-forwards the working copy to it (taking the record back when that fails), and runs the
//! amending checks again on the amended tree. A second consecutive amendment by the same check
//! is no verdict.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bongos_project_repo_model::checks::{
    AmendOutcome, AmendmentInput, OUT_OF_BOUNDS, Stamp, bound_amendment,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_project_repo_model::{ChangeMeta, DefinitionTree};

use crate::changes::{AmendCommit, ChangeRow};
use crate::check_run::CheckRun;
use crate::create::ProjectRepo;
use crate::git::{Identity, RebaseOutcome};
use crate::landing::{LandingResult, UNSAVED_EDITS};
use crate::landing_checks::AmendingCheck;
use crate::state::StateWriter;

/// The folder under `state/` that holds the scratch copies the amending checks run in.
pub(crate) const AMENDING_SCRATCH_DIR: &str = "amending";

/// The reason of a check that amended twice in a row.
pub const KEEPS_AMENDING: &str = "check keeps amending";

/// How step 3's amending checks ended.
pub(crate) enum Amended {
    /// No check amends any more: the stamps each applying check gave the final tree, and the
    /// checks of that tree, ready for the other checks.
    Done {
        stamps: BTreeMap<String, Stamp>,
        run: CheckRun,
    },
    NoVerdict(String),
}

impl ProjectRepo {
    /// Takes the amend commits off the change's branch (they sit on top of the author's
    /// commits). A checked-out working copy is reset only while it is clean, and an author
    /// commit made on top is kept by a rebase; a quick edit's branch, which has no worktree,
    /// is moved back by compare-and-swap.
    pub(crate) async fn drop_amendments(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
        identity: &Identity,
    ) -> Result<Option<LandingResult>> {
        let Some(first) = row.amend_commits.first() else {
            return Ok(None);
        };
        let base = format!("{}^", first.commit);
        let repository = &self.location.repository;
        let reference = format!("refs/heads/{}", row.branch);
        let (at, tip) = if path.is_dir() {
            (path, self.git.rev_parse(path, "HEAD").await?)
        } else {
            (
                repository.as_path(),
                self.git.rev_parse(repository, &reference).await?,
            )
        };
        // The newest listed amend commit the branch holds: one recorded just before a crash
        // stopped the fast-forward to it is listed but not on the branch.
        let mut last = None;
        for amend in row.amend_commits.iter().rev() {
            if self.git.is_ancestor(at, &amend.commit, &tip).await {
                last = Some(amend.commit.clone());
                break;
            }
        }
        if let Some(last) = last {
            if !path.is_dir() {
                if tip == last {
                    let author = self.git.rev_parse(repository, &base).await?;
                    self.git
                        .update_ref_cas(repository, &reference, &author, &last)
                        .await?;
                } else {
                    return Ok(Some(LandingResult::NoVerdict {
                        reason: format!(
                            "{} has commits on top of its amendments and no working copy",
                            row.branch
                        ),
                    }));
                }
            } else {
                if !self.git.is_clean(path).await? {
                    return Ok(Some(LandingResult::NoVerdict {
                        reason: UNSAVED_EDITS.to_string(),
                    }));
                }
                if tip == last {
                    self.git.reset_keep(path, &base).await?;
                } else if let RebaseOutcome::Conflict { files } =
                    self.git.rebase_onto(path, &base, &last, identity).await?
                {
                    return Ok(Some(LandingResult::Conflict { files }));
                }
            }
        }
        row.amend_commits.clear();
        writer.transact(|tx| tx.set_change(row))?;
        Ok(None)
    }

    /// Step 3's amending checks on the change at `path`, based on master at `master`.
    pub(crate) async fn run_amending_checks(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
        master: &str,
        meta: &ChangeMeta,
    ) -> Result<Amended> {
        let head = self.git.rev_parse(path, "HEAD").await?;
        let run = self
            .prepare_checks(&row.change_id, master, &head, meta, &row.author)
            .await?;
        if run.plan.amending.is_empty() {
            return Ok(Amended::Done {
                stamps: BTreeMap::new(),
                run,
            });
        }
        let scratch = self
            .location
            .state
            .join(AMENDING_SCRATCH_DIR)
            .join(&row.name);
        self.remove_scratch(&scratch).await?;
        if let Some(parent) = scratch.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let result = self
            .amend_in_scratch(writer, row, path, &scratch, run, master, meta)
            .await;
        if let Err(error) = self.remove_scratch(&scratch).await {
            log::error!("removing the amending scratch copy: {error:#}");
        }
        result
    }

    /// Removes a scratch copy: as a worktree when git knows it as one, otherwise as a folder.
    pub(crate) async fn remove_scratch(&self, scratch: &Path) -> Result<()> {
        if !scratch.exists() {
            return Ok(());
        }
        let repository = &self.location.repository;
        if let Err(error) = self.git.worktree_remove(repository, scratch).await {
            log::info!(
                "{} is not a worktree git knows ({error}); removing the folder",
                scratch.display()
            );
        }
        if scratch.exists() {
            std::fs::remove_dir_all(scratch)
                .with_context(|| format!("removing {}", scratch.display()))?;
            self.git.run(repository, &["worktree", "prune"]).await?;
        }
        Ok(())
    }

    /// Removes the scratch copies a former process left under `state/amending/`.
    pub(crate) async fn sweep_amending_scratch(&self) {
        let folder = self.location.state.join(AMENDING_SCRATCH_DIR);
        let Ok(entries) = std::fs::read_dir(&folder) else {
            return;
        };
        for entry in entries.flatten() {
            if let Err(error) = self.remove_scratch(&entry.path()).await {
                log::error!("removing {}: {error:#}", entry.path().display());
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn amend_in_scratch(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
        scratch: &Path,
        author_run: CheckRun,
        master: &str,
        meta: &ChangeMeta,
    ) -> Result<Amended> {
        if !self.git.is_clean(path).await? {
            return Ok(Amended::NoVerdict(UNSAVED_EDITS.to_string()));
        }
        let author_tip = self.git.rev_parse(path, "HEAD").await?;
        let Ok(author_tree) = author_run.facts.new.definitions.clone() else {
            // The schema check refuses the change; nothing is amended.
            return Ok(Amended::Done {
                stamps: BTreeMap::new(),
                run: author_run,
            });
        };
        let author_part = author_run.facts.summary.clone();
        let started = author_run
            .live
            .started_set(author_run.facts.task_ids().iter().map(String::as_str));
        let checks: Vec<Arc<dyn AmendingCheck>> = author_run.plan.amending.clone();
        let mut head = author_tip.clone();
        self.git
            .worktree_add_detached(&self.location.repository, scratch, &head)
            .await?;
        let mut current = author_run;
        let mut last: Option<String> = None;
        let mut rounds = 0;
        'rounds: loop {
            rounds += 1;
            if rounds > checks.len() * 2 + 2 {
                return Ok(Amended::NoVerdict(
                    "the amending checks keep amending each other".to_string(),
                ));
            }
            for check in &checks {
                let name = check.name().to_string();
                if !check.applies(&current.facts.summary) {
                    continue;
                }
                self.pin(&mut current).await?;
                let outcome = check.amend(current.input(), scratch).await;
                let message = match outcome {
                    AmendOutcome::Unchanged => {
                        self.reset_scratch(scratch, &head).await?;
                        continue;
                    }
                    AmendOutcome::NoVerdict { reason, .. } => {
                        return Ok(Amended::NoVerdict(format!("{name}: {reason}")));
                    }
                    AmendOutcome::Amended { message } => message,
                };
                if last.as_deref() == Some(name.as_str()) {
                    return Ok(Amended::NoVerdict(format!("{name}: {KEEPS_AMENDING}")));
                }
                let bounded = self
                    .bound(
                        scratch,
                        &author_tip,
                        &author_tree,
                        &author_part,
                        &started,
                        check,
                        message,
                    )
                    .await?;
                let message = match bounded {
                    AmendOutcome::Amended { message } => message,
                    AmendOutcome::NoVerdict { reason, .. } => {
                        return Ok(Amended::NoVerdict(format!("{name}: {reason}")));
                    }
                    AmendOutcome::Unchanged => continue,
                };
                let subject = format!("amend: {name} {message}");
                let Some(commit) = self
                    .git
                    .commit_if_dirty(scratch, &subject, &Identity::default())
                    .await?
                else {
                    continue;
                };
                // The working copy follows by a fast-forward only when the person committed
                // nothing meanwhile and has no file in the way; their other edits stay as
                // they are.
                if self.git.rev_parse(path, "HEAD").await? != head {
                    return Ok(Amended::NoVerdict(UNSAVED_EDITS.to_string()));
                }
                // Recorded before the branch moves, so a crash in between never leaves an amend
                // commit on the branch that the change does not list (a listed commit that is
                // not on the branch is skipped when the amendments are dropped).
                row.amend_commits.push(AmendCommit {
                    commit: commit.clone(),
                    check: name.clone(),
                });
                writer.transact(|tx| tx.set_change(row))?;
                if self.git.merge_ff_only(path, &commit).await.is_err() {
                    row.amend_commits.pop();
                    writer.transact(|tx| tx.set_change(row))?;
                    return Ok(Amended::NoVerdict(UNSAVED_EDITS.to_string()));
                }
                head = commit;
                last = Some(name);
                current = self
                    .prepare_checks(&row.change_id, master, &head, meta, &row.author)
                    .await?;
                continue 'rounds;
            }
            break;
        }
        // Edits typed while the checks ran are still in the working copy, uncommitted.
        if !self.git.is_clean(path).await? {
            return Ok(Amended::NoVerdict(UNSAVED_EDITS.to_string()));
        }
        self.pin(&mut current).await?;
        let stamps = checks
            .iter()
            .filter(|check| check.applies(&current.facts.summary))
            .map(|check| (check.name().to_string(), check.stamp(&current.input())))
            .collect();
        Ok(Amended::Done {
            stamps,
            run: current,
        })
    }

    /// Puts the scratch copy back to `head`, dropping what a check wrote and did not report.
    async fn reset_scratch(&self, scratch: &Path, head: &str) -> Result<()> {
        if !self.git.is_clean(scratch).await? {
            self.git
                .run(scratch, &["reset", "-q", "--hard", head])
                .await?;
            self.git.run(scratch, &["clean", "-fdq"]).await?;
        }
        Ok(())
    }

    /// Whether what `check` wrote into `scratch` stays inside the bounds of an amendment,
    /// diffed against the author's tree.
    #[allow(clippy::too_many_arguments)]
    async fn bound(
        &self,
        scratch: &Path,
        author_tip: &str,
        author_tree: &DefinitionTree,
        author_part: &bongos_project_repo_model::ChangeSummary,
        started: &std::collections::BTreeSet<String>,
        check: &Arc<dyn AmendingCheck>,
        message: String,
    ) -> Result<AmendOutcome> {
        self.git.run(scratch, &["add", "-A"]).await?;
        let changed: Vec<String> = self
            .git
            .run(
                scratch,
                &[
                    "diff",
                    "--cached",
                    "--name-only",
                    "--no-renames",
                    author_tip,
                ],
            )
            .await?
            .lines()
            .map(str::to_string)
            .collect();
        if changed.is_empty() {
            return Ok(AmendOutcome::Unchanged);
        }
        let writable = match DefinitionTree::parse_dir(&scratch.join(DEFINITIONS_DIR)) {
            Ok(tree) => tree,
            Err(problems) => {
                let list: Vec<String> = problems.iter().map(ToString::to_string).collect();
                return Ok(AmendOutcome::no_verdict(format!(
                    "{OUT_OF_BOUNDS}: {}",
                    list.join("; ")
                )));
            }
        };
        let stamp_paths = check.stamp_paths();
        Ok(bound_amendment(
            AmendOutcome::Amended { message },
            &AmendmentInput {
                author: author_tree,
                writable: &writable,
                author_part,
                changed_paths: &changed,
                stamp_paths: &stamp_paths,
                started,
            },
        ))
    }
}
