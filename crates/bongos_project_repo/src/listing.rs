//! What the landing view lists: every change of the project with the facts its row shows, and
//! the state of master. Gathered here, with git, so the view itself is a pure function of it.

use anyhow::Result;

use crate::LandingEvent;
use crate::changes::{ChangeRow, ChangeState};
use crate::check_run::CheckResult;
use crate::create::ProjectRepo;
use crate::master::{HistoryKind, OutsideChange, SplitMaster};
use crate::quick_edit::NOT_LANDED_MARK;

/// What the amending checks did to a change: one fact however many checks amended it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmendedBy {
    /// The names of the checks that amended it, from the change's amend commit record, in order
    /// and without repeats. A commit's message never decides this.
    pub checks: Vec<String>,
    /// How many files the amendments changed.
    pub files: usize,
    /// The last amend commit, whose diff `d` shows when there is only one.
    pub commit: String,
}

/// One change as the landing view shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedChange {
    pub row: ChangeRow,
    /// The landing's subject, once landed.
    pub subject: Option<String>,
    /// What the change does to tasks, in the words of the change summary ("adds 1 task").
    pub summary: String,
    /// "not landed" when the last attempt to land it was refused, conflicted or gave no verdict,
    /// with the reason when one was kept ("not landed: check tests failed: ...").
    pub not_landed: Option<String>,
    /// The last landing event of the change in this process: in queue, checking, waiting,
    /// refused, in conflict, ended.
    pub landing: Option<LandingEvent>,
    /// The newest stored result of each check that ran on the change, by check name.
    pub checks: Vec<CheckResult>,
    pub amended: Option<AmendedBy>,
    /// "move <id> needs you" on the changes of a move whose rollback failed.
    pub move_note: Option<String>,
}

/// The project's changes and master, at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeListing {
    pub split: Option<SplitMaster>,
    pub outside: Option<OutsideChange>,
    /// Oldest first, as the `changes` table lists them.
    pub changes: Vec<ListedChange>,
}

impl ProjectRepo {
    /// Every change with its row facts.
    pub async fn change_listing(&self) -> Result<ChangeListing> {
        let subjects: std::collections::HashMap<String, String> = self
            .history()?
            .into_iter()
            .filter_map(|row| match row.kind {
                HistoryKind::Landing { change_id, subject } => Some((change_id, subject)),
                HistoryKind::SplitResolution { .. } => None,
            })
            .collect();
        let rows = self.read_state(|read| read.changes())?;
        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            let summary = if row.state == ChangeState::Abandoned {
                String::new()
            } else {
                self.task_diff_of(&row)
                    .await
                    .map(|diff| diff.summary.row_text())
                    .unwrap_or_else(|_| "diff unavailable".to_string())
            };
            let not_landed = self.not_landed_mark(&row);
            let amended = self.amended_by(&row).await;
            let move_note = self.read_state(|read| crate::moves::note(read, &row.change_id))?;
            let waiting = matches!(row.state, ChangeState::Open | ChangeState::Requested);
            let checks = if waiting {
                newest_per_check(self.check_results(&row.change_id).unwrap_or_default())
            } else {
                Vec::new()
            };
            changes.push(ListedChange {
                move_note,
                landing: waiting
                    .then(|| self.landing_status(&row.change_id))
                    .flatten(),
                checks,
                subject: subjects.get(&row.change_id).cloned(),
                row,
                summary,
                not_landed,
                amended,
            });
        }
        Ok(ChangeListing {
            split: self.split_master()?,
            outside: self.outside_change()?,
            changes,
        })
    }

    fn not_landed_mark(&self, row: &ChangeRow) -> Option<String> {
        if !matches!(row.state, ChangeState::Open | ChangeState::Requested) {
            return None;
        }
        let (label, reason) = self.quick_edit_label(&row.change_id)?;
        if label != NOT_LANDED_MARK {
            return None;
        }
        Some(match reason {
            Some(reason) => format!("{label}: {reason}"),
            None => label,
        })
    }

    async fn amended_by(&self, row: &ChangeRow) -> Option<AmendedBy> {
        let first = row.amend_commits.first()?;
        let last = row.amend_commits.last()?;
        let repository = &self.location.repository;
        let parent = self
            .git
            .parents_of(repository, &first.commit)
            .await
            .ok()?
            .into_iter()
            .next()?;
        let files = self
            .git
            .diff_names(repository, &parent, &last.commit)
            .await
            .ok()?
            .len();
        let mut checks: Vec<String> = Vec::new();
        for amend in &row.amend_commits {
            if !checks.contains(&amend.check) {
                checks.push(amend.check.clone());
            }
        }
        Some(AmendedBy {
            checks,
            files,
            commit: last.commit.clone(),
        })
    }
}

/// The newest of `results` for each check, in check name order. The store lists them by check
/// name, oldest first.
fn newest_per_check(results: Vec<CheckResult>) -> Vec<CheckResult> {
    let mut newest: Vec<CheckResult> = Vec::new();
    for result in results {
        match newest.last_mut() {
            Some(last) if last.check == result.check => *last = result,
            _ => newest.push(result),
        }
    }
    newest
}
