//! Evidence records and done records in the project's state store, as `kv` rows. A record of a
//! task lives under `checks.evidence` with the key `<task id>/<tree key>/<check>/<record id>`, a
//! record of a landing run under `landing/<tree>/<check>/<record id>`, so "the records of check C
//! on tree T of task X" is one prefix scan. Only code in the process that holds the state writer
//! writes them, inside its transaction; nothing here lets a script or an agent write one.

use anyhow::{Context as _, Result};
use bongos_checks_model::EvidenceRecord;
use bongos_checks_model::evidence::StartedBy;
use bongos_project_repo::{StateRead, StateTx};
use serde::{Deserialize, Serialize};

/// The records of check runs.
pub const EVIDENCE_NAMESPACE: &str = "checks.evidence";
/// Which finished tasks owe checks.
pub const OWED_NAMESPACE: &str = "checks.owed";
/// The last child of a group to become Done-looking, by group id.
pub const LAST_NAMESPACE: &str = "checks.last";
/// What a task's checks decided, by task id.
pub const DONE_NAMESPACE: &str = "checks.done";
/// The one row of the generation counter.
pub const SEQ_NAMESPACE: &str = "checks.seq";
/// Live state of runs; never exported.
pub const LIVE_NAMESPACE: &str = "checks.live";
/// The key of the generation counter in [`SEQ_NAMESPACE`].
pub const SEQ_KEY: &str = "seq";
/// The first key part of a landing run's records.
pub const LANDING_PREFIX: &str = "landing";

/// The key of the record `id` of check `check` on `tree_key` of task `task_id`.
pub fn evidence_key(task_id: &str, tree_key: &str, check: &str, id: &str) -> String {
    format!("{task_id}/{tree_key}/{check}/{id}")
}

/// The key of the record `id` of check `check` of a landing on `tree`.
pub fn landing_key(tree: &str, check: &str, id: &str) -> String {
    format!("{LANDING_PREFIX}/{tree}/{check}/{id}")
}

/// The prefix of the records of check `check` on `tree_key` of task `task_id`.
pub fn evidence_prefix(task_id: &str, tree_key: &str, check: &str) -> String {
    format!("{task_id}/{tree_key}/{check}/")
}

/// The prefix of the records of check `check` of a landing on `tree`.
pub fn landing_prefix(tree: &str, check: &str) -> String {
    format!("{LANDING_PREFIX}/{tree}/{check}/")
}

/// The prefix of every record of task `task_id`.
pub fn task_prefix(task_id: &str) -> String {
    format!("{task_id}/")
}

/// The key a record is stored under.
pub fn record_key(record: &EvidenceRecord) -> String {
    match &record.started_by {
        StartedBy::Landing { tree } => landing_key(tree, &record.check, &record.id),
        _ => evidence_key(&record.task_id, &record.tree_key, &record.check, &record.id),
    }
}

/// Writes `record` in `tx`, replacing the row of the same key (an unfinished record is updated
/// in place as parts finish). Returns the key.
pub fn store_record(tx: &mut StateTx, record: &EvidenceRecord) -> Result<String> {
    let key = record_key(record);
    let value = toml::Value::try_from(record).context("serializing an evidence record")?;
    tx.kv(EVIDENCE_NAMESPACE).put(&key, value)?;
    Ok(key)
}

/// Removes the record stored under `key`.
pub fn delete_record(tx: &mut StateTx, key: &str) -> Result<()> {
    tx.kv(EVIDENCE_NAMESPACE).delete(key)
}

fn decode(key: &str, value: toml::Value) -> Result<EvidenceRecord> {
    value
        .try_into()
        .with_context(|| format!("reading the evidence record {key}"))
}

/// The records whose key starts with `prefix`, ordered by key, as `tx` sees them.
pub fn scan_records_in(tx: &mut StateTx, prefix: &str) -> Result<Vec<EvidenceRecord>> {
    let rows = tx.kv(EVIDENCE_NAMESPACE).scan(prefix)?;
    rows.into_iter()
        .map(|(key, value)| decode(&key, value))
        .collect()
}

/// The records whose key starts with `prefix`, ordered by key.
pub fn scan_records(read: &StateRead, prefix: &str) -> Result<Vec<EvidenceRecord>> {
    let rows = read.kv(EVIDENCE_NAMESPACE).scan(prefix)?;
    rows.into_iter()
        .map(|(key, value)| decode(&key, value))
        .collect()
}

/// What a task's checks decided: written by the decider in its own transaction and deleted when
/// the task stops looking done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneRecord {
    pub tree_key: String,
    pub generation: u64,
    pub at: String,
    /// The ids of the evidence records the decision rests on.
    pub evidence: Vec<String>,
}

/// Writes the done record of `task_id`.
pub fn put_done(tx: &mut StateTx, task_id: &str, done: &DoneRecord) -> Result<()> {
    let value = toml::Value::try_from(done).context("serializing a done record")?;
    tx.kv(DONE_NAMESPACE).put(task_id, value)
}

/// Removes the done record of `task_id`, if any.
pub fn delete_done(tx: &mut StateTx, task_id: &str) -> Result<()> {
    tx.kv(DONE_NAMESPACE).delete(task_id)
}

/// The done record of `task_id`, if any.
pub fn done_record(read: &StateRead, task_id: &str) -> Result<Option<DoneRecord>> {
    read.kv(DONE_NAMESPACE)
        .get(task_id)?
        .map(|value| {
            value
                .try_into()
                .with_context(|| format!("reading the done record of {task_id}"))
        })
        .transpose()
}

/// The done record of `task_id` as `tx` sees it.
pub fn done_record_in(tx: &mut StateTx, task_id: &str) -> Result<Option<DoneRecord>> {
    tx.kv(DONE_NAMESPACE)
        .get(task_id)?
        .map(|value| {
            value
                .try_into()
                .with_context(|| format!("reading the done record of {task_id}"))
        })
        .transpose()
}

/// Increments the generation counter and returns the new value. The counter orders events and
/// is never a clock.
pub fn next_generation(tx: &mut StateTx) -> Result<u64> {
    let current = tx
        .kv(SEQ_NAMESPACE)
        .get(SEQ_KEY)?
        .map(|value| {
            value
                .as_integer()
                .and_then(|number| u64::try_from(number).ok())
                .context("the generation counter is not a number")
        })
        .transpose()?
        .unwrap_or(0);
    let next = current + 1;
    tx.kv(SEQ_NAMESPACE)
        .put(SEQ_KEY, toml::Value::Integer(i64::try_from(next)?))?;
    Ok(next)
}
