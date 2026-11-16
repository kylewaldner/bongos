//! The live rows of check runs in the project's state store, namespace `checks.live`, so every
//! process that reads the store (a window attached to a separate host too) sees what runs now
//! without an event relay.
//!
//! Two kinds of rows:
//! - `part/<task id>/<check>`: what the check's run does now, a [`LiveRow`] (waiting in line for a
//!   pool at a place, running part K of N, or ended with its verdict). Each row carries `since`,
//!   when that state began; elapsed time is never written, a view computes it.
//! - `hold/<task id>`: the reason the done rule holds the task. Written by the done decider,
//!   which does not exist yet; nothing here writes one.
//!
//! The rules that keep the namespace cheap: at most one transaction per task per
//! `task_checks.live_write_ms`, with later states of the same row replacing earlier ones until
//! the interval passes (last write wins); the `checks` state exporter never exports it; and the
//! process that holds the writer deletes the whole namespace the first time its engine uses a
//! store, so nothing stale survives a crash or a handover.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{StateRead, StateWriter};
use serde::{Deserialize, Serialize};

use crate::evidence_store::LIVE_NAMESPACE;

/// The first key part of a check's live row.
pub const PART_PREFIX: &str = "part";
/// The first key part of a task's hold reason, written by the done decider.
pub const HOLD_PREFIX: &str = "hold";

/// The key of the live row of check `check` of task `task_id`.
pub fn part_key(task_id: &str, check: &str) -> String {
    format!("{PART_PREFIX}/{task_id}/{check}")
}

/// The key of the hold reason of task `task_id`.
pub fn hold_key(task_id: &str) -> String {
    format!("{HOLD_PREFIX}/{task_id}")
}

/// The reason the done rule holds a task, as its `hold/` row holds it ("not done: 2 checks
/// missing").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldRow {
    pub reason: String,
}

/// What a check's run does now, as its live row holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum LiveRow {
    /// A part waits in line for `pool`, at `place` (1 first).
    Waiting {
        pool: String,
        place: u32,
        since: String,
    },
    /// Part `part` of `of` runs; `kept` parts of an earlier run are reused.
    Running {
        part: u32,
        of: u32,
        kept: u32,
        since: String,
    },
    /// The check ended with `verdict`.
    Ended { verdict: Verdict, since: String },
}

impl LiveRow {
    /// When this state began.
    pub fn since(&self) -> &str {
        match self {
            Self::Waiting { since, .. }
            | Self::Running { since, .. }
            | Self::Ended { since, .. } => since,
        }
    }

    /// Whether `other` is the same state, apart from when it began, so it keeps this `since`.
    pub(crate) fn same_state(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Waiting { pool, place, .. },
                Self::Waiting {
                    pool: other_pool,
                    place: other_place,
                    ..
                },
            ) => pool == other_pool && place == other_place,
            (
                Self::Running { part, of, .. },
                Self::Running {
                    part: other_part,
                    of: other_of,
                    ..
                },
            ) => part == other_part && of == other_of,
            (Self::Ended { verdict, .. }, Self::Ended { verdict: other, .. }) => verdict == other,
            _ => false,
        }
    }
}

/// Every `part/` row, by key, as `read` sees them.
pub fn live_rows(read: &StateRead) -> Result<BTreeMap<String, LiveRow>> {
    read.kv(LIVE_NAMESPACE)
        .scan(&format!("{PART_PREFIX}/"))?
        .into_iter()
        .map(|(key, value)| {
            let row = value
                .try_into()
                .with_context(|| format!("reading the live row {key}"))?;
            Ok((key, row))
        })
        .collect()
}

/// Deletes the whole `checks.live` namespace of the store `writer` holds.
pub fn clear_live(writer: &StateWriter) -> Result<()> {
    writer.transact(|tx| tx.kv(LIVE_NAMESPACE).clear())
}

/// What the caller of [`LiveRows::update`] does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flush {
    /// Write the task's pending rows now.
    Now,
    /// Write them after this long; nothing else is scheduled for the task.
    After(Duration),
    /// A write of the task is already scheduled and will carry this row.
    Scheduled,
    /// The row did not change.
    Unchanged,
}

struct TaskLive {
    /// Weak, so a closed project's store (and its lock) is never held open by a row.
    writer: Weak<StateWriter>,
    /// The rows as last updated, written or not.
    rows: BTreeMap<String, LiveRow>,
    /// The rows changed since the last write, by check.
    pending: BTreeMap<String, LiveRow>,
    last_write: Option<Instant>,
    scheduled: bool,
}

/// One write of a task's live rows, for tests.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct LiveWrite {
    pub task_id: String,
    pub at: Instant,
    pub rows: BTreeMap<String, LiveRow>,
}

/// The throttled writer of `part/` rows: one transaction per task per interval, the newest
/// state of each row winning. The caller supplies the time, so nothing here reads a clock.
#[derive(Default)]
pub(crate) struct LiveRows {
    tasks: BTreeMap<String, TaskLive>,
    /// The stores whose namespace was cleared at first use.
    cleared: Vec<Weak<StateWriter>>,
    #[cfg(test)]
    pub writes: Vec<LiveWrite>,
}

impl LiveRows {
    /// Clears the namespace of `writer` the first time it is seen: the writer process deletes it
    /// when it opens a store, before any row of this process is written.
    pub fn adopt(&mut self, writer: &Arc<StateWriter>) {
        self.cleared.retain(|known| known.strong_count() > 0);
        if self
            .cleared
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), Arc::as_ptr(writer)))
        {
            return;
        }
        if let Err(error) = clear_live(writer) {
            log::error!("clearing the live rows of checks: {error:#}");
        }
        self.cleared.push(Arc::downgrade(writer));
    }

    /// The row the task's check would have now, with `since` kept when the state is the same.
    pub fn next_row(&self, task_id: &str, check: &str, row: LiveRow) -> LiveRow {
        let previous = self
            .tasks
            .get(task_id)
            .and_then(|task| task.rows.get(check));
        match previous {
            Some(previous) if previous.same_state(&row) => previous.clone(),
            _ => row,
        }
    }

    /// Records the new row of `check` of `task_id` at `now` and says when to write.
    pub fn update(
        &mut self,
        writer: &Arc<StateWriter>,
        task_id: &str,
        check: &str,
        row: LiveRow,
        now: Instant,
        interval: Duration,
    ) -> Flush {
        self.adopt(writer);
        let task = self
            .tasks
            .entry(task_id.to_string())
            .or_insert_with(|| TaskLive {
                writer: Arc::downgrade(writer),
                rows: BTreeMap::new(),
                pending: BTreeMap::new(),
                last_write: None,
                scheduled: false,
            });
        task.writer = Arc::downgrade(writer);
        if task.rows.get(check) == Some(&row) {
            return Flush::Unchanged;
        }
        task.rows.insert(check.to_string(), row.clone());
        task.pending.insert(check.to_string(), row);
        if task.scheduled {
            return Flush::Scheduled;
        }
        match task.last_write {
            Some(last) if now < last + interval => {
                task.scheduled = true;
                Flush::After(last + interval - now)
            }
            _ => Flush::Now,
        }
    }

    /// Writes the pending rows of `task_id` in one transaction.
    pub fn flush(&mut self, task_id: &str, now: Instant) -> Result<()> {
        let Some(task) = self.tasks.get_mut(task_id) else {
            return Ok(());
        };
        task.scheduled = false;
        if task.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut task.pending);
        // The store is closed: nothing is left to write to.
        let Some(writer) = task.writer.upgrade() else {
            return Ok(());
        };
        task.last_write = Some(now);
        #[cfg(test)]
        self.writes.push(LiveWrite {
            task_id: task_id.to_string(),
            at: now,
            rows: pending.clone(),
        });
        writer.transact(|tx| {
            for (check, row) in &pending {
                let value = toml::Value::try_from(row).context("serializing a live row")?;
                tx.kv(LIVE_NAMESPACE)
                    .put(&part_key(task_id, check), value)?;
            }
            Ok(())
        })
    }

    /// Forgets the task once nothing of it waits to be written: called when its run has ended,
    /// so a task is kept only while it runs or has rows still to write.
    pub fn release(&mut self, task_id: &str) {
        if self
            .tasks
            .get(task_id)
            .is_some_and(|task| !task.scheduled && task.pending.is_empty())
        {
            self.tasks.remove(task_id);
        }
    }

    /// Whether the task is still kept, for tests.
    #[cfg(test)]
    pub fn has_task(&self, task_id: &str) -> bool {
        self.tasks.contains_key(task_id)
    }

    /// Forgets every pending row and every scheduled write, and deletes the namespace of every store seen.
    pub fn clear_all(&mut self) {
        self.tasks.clear();
        self.cleared.retain(|known| known.strong_count() > 0);
        for writer in self.cleared.iter().filter_map(Weak::upgrade) {
            if let Err(error) = clear_live(&writer) {
                log::error!("clearing the live rows of checks: {error:#}");
            }
        }
    }
}
