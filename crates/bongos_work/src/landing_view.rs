//! What `project: show changes` shows: the changes of a project repository as a list in sections
//! (master's warnings, the changes waiting to land, those that landed, those abandoned), and
//! what each key does on a row. Pure functions over a [`ChangeListing`]; nothing here reads a
//! file or touches the UI, so a row reads the same in the view and in the tests.
//!
//! A change an amending check edited has one extra row under it, "amended by <check>: <N>
//! files", whose `d` shows the amend commit's diff instead of the change's.

use bongos_project_repo::{
    AmendedBy, ChangeListing, ChangeState, ListedChange, MasterStatus, NOT_LANDED_MARK,
};
use chrono::{DateTime, Utc};

use crate::working_copy_commands::working_copy_label;

/// The parts of the view, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Master was changed outside bongos.
    Master,
    /// Open and requested changes.
    Waiting,
    Landed,
    Abandoned,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Master => "Master",
            Section::Waiting => "Waiting",
            Section::Landed => "Landed",
            Section::Abandoned => "Abandoned",
        }
    }
}

/// What a row stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowTarget {
    /// A master that is split: `r` resolves it.
    SplitMaster,
    /// An outside commit waiting for approval; the row only says so.
    OutsideCommit,
    Change {
        change_id: String,
        state: ChangeState,
    },
    /// The "amended by" row of a change: `d` opens the amend commit's diff.
    Amended {
        change_id: String,
        commit: String,
        state: ChangeState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub target: RowTarget,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub section: Section,
    pub rows: Vec<Row>,
}

/// The view: only the sections that have rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandingView {
    pub sections: Vec<Block>,
}

/// What a key does to a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingCommand {
    /// `d`: the change's diff in terms of tasks.
    OpenTaskDiff { change_id: String },
    /// `D`: the change's `git diff`.
    OpenRawDiff { change_id: String },
    /// `d` on an "amended by" row: the diff of the amend commits.
    OpenAmendDiff { change_id: String, commit: String },
    /// `o`: check the change out as a working copy, to repair it.
    CheckOut { change_id: String },
    /// `r` on a split master.
    ResolveSplit,
    /// `a`.
    Approve { change_id: String },
    /// `k`.
    Abandon { change_id: String },
    /// `l`.
    Land { change_id: String },
    /// `R` on a landed change.
    Revert { change_id: String },
}

fn amended_text(amended: &AmendedBy) -> String {
    format!(
        "amended by {}: {} files",
        amended.checks.join(", "),
        amended.files
    )
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

fn change_text(listed: &ListedChange, now: DateTime<Utc>, stale_after_days: u32) -> String {
    let row = &listed.row;
    let mut text = match row.state {
        ChangeState::Open | ChangeState::Requested => {
            working_copy_label(row, now, stale_after_days)
        }
        ChangeState::Landed => format!(
            "{} ({}, {})",
            listed.subject.as_deref().unwrap_or(&row.name),
            row.author.name,
            short(row.commit.as_deref().unwrap_or_default())
        ),
        ChangeState::Abandoned => format!("{} ({})", row.name, row.author.name),
    };
    if !listed.summary.is_empty() {
        text.push_str(": ");
        text.push_str(&listed.summary);
    }
    if row.state == ChangeState::Requested && listed.not_landed.is_none() {
        text.push_str(" [landing]");
    }
    if listed.not_landed.is_some() {
        text.push_str(&format!(" [{NOT_LANDED_MARK}]"));
    }
    text
}

impl LandingView {
    /// The view of `listing`, ages taken at `now`. The waiting section is oldest first, as the
    /// changes are listed; landed and abandoned changes are newest first.
    pub fn build(listing: &ChangeListing, now: DateTime<Utc>, stale_after_days: u32) -> Self {
        let mut master = Vec::new();
        if let Some(split) = &listing.split
            && let Some(text) = MasterStatus::Split(split.clone()).warning()
        {
            master.push(Row {
                target: RowTarget::SplitMaster,
                text,
            });
        }
        if let Some(outside) = &listing.outside
            && let Some(text) = MasterStatus::Outside(outside.clone()).warning()
        {
            master.push(Row {
                target: RowTarget::OutsideCommit,
                text,
            });
        }
        let rows_of = |keep: fn(ChangeState) -> bool, newest_first: bool| {
            let mut listed: Vec<&ListedChange> = listing
                .changes
                .iter()
                .filter(|listed| keep(listed.row.state))
                .collect();
            if newest_first {
                listed.sort_by(|a, b| {
                    b.row
                        .updated
                        .cmp(&a.row.updated)
                        .then_with(|| a.row.change_id.cmp(&b.row.change_id))
                });
            }
            let mut rows = Vec::new();
            for listed in listed {
                rows.push(Row {
                    target: RowTarget::Change {
                        change_id: listed.row.change_id.clone(),
                        state: listed.row.state,
                    },
                    text: change_text(listed, now, stale_after_days),
                });
                if let Some(amended) = &listed.amended {
                    rows.push(Row {
                        target: RowTarget::Amended {
                            change_id: listed.row.change_id.clone(),
                            commit: amended.commit.clone(),
                            state: listed.row.state,
                        },
                        text: format!("  {}", amended_text(amended)),
                    });
                }
            }
            rows
        };
        let sections = [
            (Section::Master, master),
            (
                Section::Waiting,
                rows_of(
                    |state| matches!(state, ChangeState::Open | ChangeState::Requested),
                    false,
                ),
            ),
            (
                Section::Landed,
                rows_of(|state| state == ChangeState::Landed, true),
            ),
            (
                Section::Abandoned,
                rows_of(|state| state == ChangeState::Abandoned, true),
            ),
        ];
        Self {
            sections: sections
                .into_iter()
                .filter(|(_, rows)| !rows.is_empty())
                .map(|(section, rows)| Block { section, rows })
                .collect(),
        }
    }

    /// Every row, in the order they are shown.
    pub fn rows(&self) -> Vec<&Row> {
        self.sections
            .iter()
            .flat_map(|block| block.rows.iter())
            .collect()
    }

    /// The view as lines of text: a heading for each section, then its rows indented.
    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for block in &self.sections {
            lines.push(format!("{} ({})", block.section.title(), block.rows.len()));
            lines.extend(block.rows.iter().map(|row| format!("  {}", row.text)));
        }
        lines
    }

    /// What `key` does on the row at `index` (an index into [`Self::rows`]), or why it does
    /// nothing.
    pub fn command(&self, key: &str, index: usize) -> Result<LandingCommand, String> {
        let row = *self
            .rows()
            .get(index)
            .ok_or_else(|| "there is no row here".to_string())?;
        command_for(&row.target, key)
    }
}

/// What `key` does on a row that stands for `target`.
pub fn command_for(target: &RowTarget, key: &str) -> Result<LandingCommand, String> {
    let (change_id, state) = match target {
        RowTarget::SplitMaster => {
            return match key {
                "r" => Ok(LandingCommand::ResolveSplit),
                _ => Err("master is split: press r to resolve it".to_string()),
            };
        }
        RowTarget::OutsideCommit => {
            return Err("run project: approve to approve the outside commit".to_string());
        }
        RowTarget::Amended {
            change_id, commit, ..
        } if key == "d" => {
            return Ok(LandingCommand::OpenAmendDiff {
                change_id: change_id.clone(),
                commit: commit.clone(),
            });
        }
        // Any other key on the "amended by" row acts on its change.
        RowTarget::Amended {
            change_id, state, ..
        } => (change_id, *state),
        RowTarget::Change { change_id, state } => (change_id, *state),
    };
    let change_id = change_id.clone();
    let waiting = |what: &str| match state {
        ChangeState::Open | ChangeState::Requested => Ok(()),
        _ => Err(format!("a change that is not waiting cannot be {what}")),
    };
    match key {
        "d" => Ok(LandingCommand::OpenTaskDiff { change_id }),
        "D" => Ok(LandingCommand::OpenRawDiff { change_id }),
        "o" => waiting("checked out").map(|()| LandingCommand::CheckOut { change_id }),
        "a" => waiting("approved").map(|()| LandingCommand::Approve { change_id }),
        "k" => waiting("abandoned").map(|()| LandingCommand::Abandon { change_id }),
        "l" => waiting("landed").map(|()| LandingCommand::Land { change_id }),
        "R" => match state {
            ChangeState::Landed => Ok(LandingCommand::Revert { change_id }),
            _ => Err("only a landed change can be reverted".to_string()),
        },
        other => Err(format!("{other} does nothing here")),
    }
}
