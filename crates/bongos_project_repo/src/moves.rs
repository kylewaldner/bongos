//! The rows a move keeps in each project's state store, and what the rest of the engine reads
//! from them: the `moving` claims that hold the moved tasks (a [`LiveSource`] for check 6 and a
//! refusal of every state op on a held task), the landing's step 8 of each of the move's own
//! changes, the hold of the source's landing until the target's landed, the `Bongos-Move`
//! trailers, and the refusal of `project: revert` of one side while the other stands.
//!
//! Both namespaces are the engine's: no state hook and no caller writes them, so a change counts
//! as the move's own only because the coordinator recorded its change id here.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::checks::{HoldReason, LiveSource, MoveHold};
use bongos_project_repo_model::moves::{MovePlan, is_plain_name};
use bongos_project_repo_model::{Author, TaskStateRecord};
use serde::{Deserialize, Serialize};

use crate::router::FileOp;
use crate::state::{StateRead, StateTx};
use crate::state_ops::StateOp;

/// The `kv` namespace of the moves' rows, keyed by move id.
pub const MOVES_NAMESPACE: &str = "moves";
/// The `kv` namespace of the `moving` claims: key a task id, value the move id.
pub const MOVE_CLAIMS_NAMESPACE: &str = "move";
/// The folder of the files root a move copies the moved tasks' files folders into before the
/// target lands: `.move-<move id>/<task id>/`.
pub(crate) const HOLDING_PREFIX: &str = ".move-";

/// Which project a row is kept in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MoveRole {
    Source,
    Target,
}

/// Where a move is. The source's row is the coordinator's record; the target's follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MovePhase {
    /// The preview was confirmed and the source's tasks are held.
    Confirmed,
    /// Both working copies are made.
    Prepared,
    /// The target landed: its tasks are imported and held.
    TargetLanded,
    /// The source landed: its tasks are gone.
    SourceLanded,
    /// Every claim is cleared.
    Done,
    /// Refused before the target landed; nothing changed.
    Refused,
    /// The source was refused after the target landed: the target is being rolled back.
    RollingBack,
    RolledBack,
    /// The rollback failed; the claims stay until a person resolves it.
    NeedsYou,
}

impl MovePhase {
    /// Whether the move is over (a move that needs a person is not).
    pub fn is_over(self) -> bool {
        matches!(
            self,
            MovePhase::Done | MovePhase::Refused | MovePhase::RolledBack
        )
    }
}

/// One move's row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MoveRecord {
    pub move_id: String,
    pub role: MoveRole,
    pub phase: MovePhase,
    /// The source's removal change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_change: Option<String>,
    /// The target's addition change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_change: Option<String>,
    /// The target's landing commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commit: Option<String>,
    /// The rollback of the target, when one was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_change: Option<String>,
    /// Why the move was refused, rolled back or needs a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    pub plan: MovePlan,
    pub author: Author,
    /// The target's row only: the moved tasks' state rows, imported when the target lands.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub states: BTreeMap<String, TaskStateRecord>,
}

impl MoveRecord {
    /// The change ids the coordinator made for this move.
    pub fn coordinator_changes(&self) -> BTreeSet<String> {
        [
            &self.source_change,
            &self.target_change,
            &self.rollback_change,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }

    /// The project on the other side of this row's.
    pub fn other_title(&self) -> &str {
        match self.role {
            MoveRole::Source => &self.plan.target.title,
            MoveRole::Target => &self.plan.source.title,
        }
    }

    /// "moving to <target>" in the source, "moving from <source>" in the target.
    pub fn label(&self) -> String {
        match self.role {
            MoveRole::Source => format!("moving to {}", self.plan.target.title),
            MoveRole::Target => format!("moving from {}", self.plan.source.title),
        }
    }

    fn validate(&self) -> Result<()> {
        if !is_plain_name(&self.move_id) {
            bail!("{:?} is not a move id", self.move_id);
        }
        self.plan
            .validate()
            .map_err(|reason| anyhow!("move {}: {reason}", self.move_id))
    }
}

/// The holding folder of a move's files, relative to the files root.
pub(crate) fn holding_dir(move_id: &str) -> String {
    format!("{HOLDING_PREFIX}{move_id}")
}

pub(crate) fn read_record(read: &StateRead, move_id: &str) -> Result<Option<MoveRecord>> {
    let Some(value) = read.kv(MOVES_NAMESPACE).get(move_id)? else {
        return Ok(None);
    };
    let record: MoveRecord = value
        .try_into()
        .with_context(|| format!("reading the row of move {move_id}"))?;
    record.validate()?;
    Ok(Some(record))
}

/// Every move's row in the store.
pub(crate) fn records(read: &StateRead) -> Result<Vec<MoveRecord>> {
    read.kv(MOVES_NAMESPACE)
        .scan("")?
        .into_iter()
        .map(|(move_id, value)| {
            let record: MoveRecord = value
                .try_into()
                .with_context(|| format!("reading the row of move {move_id}"))?;
            record.validate()?;
            Ok(record)
        })
        .collect()
}

pub(crate) fn write_record(tx: &mut StateTx, record: &MoveRecord) -> Result<()> {
    record.validate()?;
    let value = toml::Value::try_from(record).context("serializing a move's row")?;
    tx.engine_kv(MOVES_NAMESPACE).put(&record.move_id, value)
}

/// Writes a `moving` claim of `move_id` on `task_id`; refused when another move holds it.
pub(crate) fn claim(tx: &mut StateTx, task_id: &str, move_id: &str) -> Result<()> {
    if let Some(existing) = tx.engine_kv(MOVE_CLAIMS_NAMESPACE).get(task_id)? {
        if existing.as_str() != Some(move_id) {
            let holds = holds_in(&tx.as_read())?;
            match holds.get(task_id) {
                Some(hold) => bail!("task {task_id} is held: {hold}"),
                None => bail!("task {task_id} is held by another move"),
            }
        }
        return Ok(());
    }
    tx.engine_kv(MOVE_CLAIMS_NAMESPACE)
        .put(task_id, toml::Value::String(move_id.to_string()))
}

/// Removes every claim of `move_id`.
pub(crate) fn release_claims(tx: &mut StateTx, move_id: &str) -> Result<()> {
    let claims = tx.engine_kv(MOVE_CLAIMS_NAMESPACE).scan("")?;
    for (task_id, value) in claims {
        if value.as_str() == Some(move_id) {
            tx.engine_kv(MOVE_CLAIMS_NAMESPACE).delete(&task_id)?;
        }
    }
    Ok(())
}

/// The hold of every claimed task, by task id.
pub(crate) fn holds_in(read: &StateRead) -> Result<BTreeMap<String, HoldReason>> {
    let claims = read.kv(MOVE_CLAIMS_NAMESPACE).scan("")?;
    if claims.is_empty() {
        return Ok(BTreeMap::new());
    }
    let records: BTreeMap<String, MoveRecord> = records(read)?
        .into_iter()
        .map(|record| (record.move_id.clone(), record))
        .collect();
    let mut holds = BTreeMap::new();
    for (task_id, value) in claims {
        let move_id = value.as_str().unwrap_or_default().to_string();
        let record = records.get(&move_id);
        holds.insert(
            task_id,
            HoldReason::Moving(MoveHold {
                label: record.map_or_else(|| "moving".to_string(), MoveRecord::label),
                coordinator_changes: record
                    .map(MoveRecord::coordinator_changes)
                    .unwrap_or_default(),
                move_id,
            }),
        );
    }
    Ok(holds)
}

/// The `moving` claims of one store, read at one moment, as a live source.
pub(crate) struct MoveClaims(pub(crate) BTreeMap<String, HoldReason>);

impl LiveSource for MoveClaims {
    fn name(&self) -> &str {
        "moving claims"
    }

    fn started(&self, _task_id: &str) -> bool {
        false
    }

    fn held(&self, task_id: &str) -> Option<HoldReason> {
        self.0.get(task_id).cloned()
    }
}

/// The task ids an op writes: its tasks, and the files folder its paths are in.
fn op_tasks(op: &StateOp) -> Vec<String> {
    let folder = |path: &std::path::Path| {
        path.components()
            .next()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
    };
    match op {
        StateOp::MoveTodo {
            from_task, to_task, ..
        }
        | StateOp::MoveWork { from_task, to_task } => vec![from_task.clone(), to_task.clone()],
        StateOp::MoveFiles { from, to } => folder(from).into_iter().chain(folder(to)).collect(),
        StateOp::CreatePrompts { task } => vec![task.clone()],
        StateOp::WriteFile { path, .. } => folder(path).into_iter().collect(),
        StateOp::Set { task, .. }
        | StateOp::SetTodo { task, .. }
        | StateOp::AppendHistory { task, .. }
        | StateOp::Delete { task } => vec![task.clone()],
        StateOp::RecordProposal { root, .. } => vec![root.clone()],
    }
}

/// Refuses `ops` when one writes a held task, unless `change_id` is one the task's move made.
/// Read in the transaction that applies them, so a claim written first is always seen.
pub(crate) fn refuse_held(tx: &StateTx, ops: &[StateOp], change_id: Option<&str>) -> Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    let holds = holds_in(&tx.as_read())?;
    if holds.is_empty() {
        return Ok(());
    }
    for op in ops {
        for task in op_tasks(op) {
            let Some(hold) = holds.get(&task) else {
                continue;
            };
            if change_id.is_some_and(|change_id| hold.exempts(change_id)) {
                continue;
            }
            bail!("task {task} is held: {hold}; nothing changes it until the move ends");
        }
    }
    Ok(())
}

/// The landing's step 8 for a change of a move, in the landing's state transaction: the target's
/// addition imports the moved tasks' rows and claims them, the source's removal deletes its rows
/// and claims, and the target's rollback deletes the imported rows and claims. Any other change
/// is left alone.
pub(crate) fn landing_step8(tx: &mut StateTx, change_id: &str) -> Result<()> {
    for mut record in records(&tx.as_read())? {
        let is = |id: &Option<String>| id.as_deref() == Some(change_id);
        match record.role {
            MoveRole::Target if is(&record.target_change) => {
                if record.phase != MovePhase::Prepared {
                    bail!(
                        "move {} is {:?}; its target cannot land now",
                        record.move_id,
                        record.phase
                    );
                }
                for task_id in record.plan.task_ids() {
                    if tx.task(&task_id)?.is_some() {
                        bail!(
                            "task {task_id} already has state in {}",
                            record.plan.target.title
                        );
                    }
                }
                for (task_id, state) in &record.states {
                    tx.set_task(task_id, state.clone())?;
                }
                for task_id in record.plan.task_ids() {
                    claim(tx, &task_id, &record.move_id)?;
                }
                record.phase = MovePhase::TargetLanded;
                write_record(tx, &record)?;
            }
            MoveRole::Source if is(&record.source_change) => {
                if record.phase != MovePhase::TargetLanded {
                    bail!(
                        "move {} has not landed in {} yet",
                        record.move_id,
                        record.plan.target.title
                    );
                }
                for task_id in record.plan.task_ids() {
                    tx.delete_task(&task_id)?;
                }
                release_claims(tx, &record.move_id)?;
                record.phase = MovePhase::SourceLanded;
                write_record(tx, &record)?;
            }
            MoveRole::Target if is(&record.rollback_change) => {
                for task_id in record.plan.task_ids() {
                    tx.delete_task(&task_id)?;
                }
                release_claims(tx, &record.move_id)?;
                record.phase = MovePhase::RolledBack;
                write_record(tx, &record)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The file ops a landing of `change_id` makes for its move: the target's addition moves the
/// held copies of the files folders into place.
pub(crate) fn landing_files(read: &StateRead, change_id: &str) -> Result<Vec<FileOp>> {
    let mut ops = Vec::new();
    for record in records(read)? {
        if record.role == MoveRole::Target && record.target_change.as_deref() == Some(change_id) {
            let holding = holding_dir(&record.move_id);
            for task_id in record.plan.task_ids() {
                ops.push(FileOp::Move {
                    from: std::path::Path::new(&holding).join(&task_id),
                    to: std::path::PathBuf::from(&task_id),
                });
            }
        }
    }
    Ok(ops)
}

/// The trailers the landing of `change_id` carries when it is a change of a move.
pub(crate) fn trailers(read: &StateRead, change_id: &str) -> Result<Vec<String>> {
    for record in records(read)? {
        let is = |id: &Option<String>| id.as_deref() == Some(change_id);
        if is(&record.rollback_change) {
            return Ok(vec![
                format!("Bongos-Move: {}", record.move_id),
                format!("Bongos-Move-Rollback: {}", record.move_id),
            ]);
        }
        if is(&record.source_change) || is(&record.target_change) {
            return Ok(vec![format!("Bongos-Move: {}", record.move_id)]);
        }
    }
    Ok(Vec::new())
}

/// What the source's landing does at its gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Go,
    /// The target has not landed yet: the reason shown while waiting.
    Wait(String),
    Refuse(String),
}

/// The gate of `change_id`: a move's source removal goes on only once its target landed.
pub(crate) fn gate(read: &StateRead, change_id: &str) -> Result<Gate> {
    for record in records(read)? {
        if record.role != MoveRole::Source || record.source_change.as_deref() != Some(change_id) {
            continue;
        }
        return Ok(match record.phase {
            MovePhase::TargetLanded | MovePhase::SourceLanded | MovePhase::Done => Gate::Go,
            MovePhase::Confirmed | MovePhase::Prepared => Gate::Wait(format!(
                "waiting: move {} lands in {} first",
                record.move_id, record.plan.target.title
            )),
            MovePhase::Refused
            | MovePhase::RollingBack
            | MovePhase::RolledBack
            | MovePhase::NeedsYou => Gate::Refuse(format!(
                "move {} did not land in {}{}",
                record.move_id,
                record.plan.target.title,
                record
                    .problem
                    .as_ref()
                    .map(|problem| format!(": {problem}"))
                    .unwrap_or_default()
            )),
        });
    }
    Ok(Gate::Go)
}

/// Why `project: revert` of `change_id` is refused: it is one side of a move whose other side
/// stands. Undo is a move back.
pub(crate) fn revert_refusal(read: &StateRead, change_id: &str) -> Result<Option<String>> {
    for record in records(read)? {
        let side = record.source_change.as_deref() == Some(change_id)
            || record.target_change.as_deref() == Some(change_id);
        if side && !matches!(record.phase, MovePhase::Refused | MovePhase::RolledBack) {
            return Ok(Some(format!(
                "change {change_id} is one side of move {}, and the other side stands in {}; \
                 move the tasks back instead",
                record.move_id,
                record.other_title()
            )));
        }
    }
    Ok(None)
}

/// What the landing view shows on a change of a move that needs a person.
pub(crate) fn note(read: &StateRead, change_id: &str) -> Result<Option<String>> {
    for record in records(read)? {
        if record.phase == MovePhase::NeedsYou && record.coordinator_changes().contains(change_id) {
            return Ok(Some(format!("move {} needs you", record.move_id)));
        }
    }
    Ok(None)
}
