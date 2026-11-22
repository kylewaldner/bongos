//! The run state of lanes in a project's state store: one owner record per owned task in
//! [`OWNERS_NAMESPACE`], keyed by task id, and one lane record per lane in [`LANES_NAMESPACE`],
//! keyed by lane id (`build-2`).

use anyhow::{Context as _, Result, bail};
use bongos_lanes_model::{LaneId, LaneRecord, LanesState, OwnerRecord, TaskId};
use bongos_project_repo::{StateRead, StateTx};

/// The owner records, by task id.
pub const OWNERS_NAMESPACE: &str = "lanes.owners";
/// The lane records, by lane id.
pub const LANES_NAMESPACE: &str = "lanes.lanes";

/// Reads the owner record stored under `task` from `value`.
pub(crate) fn owner_from(task: &str, value: toml::Value) -> Result<OwnerRecord> {
    value
        .try_into()
        .with_context(|| format!("reading the lane owner of task {task}"))
}

/// Reads the lane record stored under `key` from `value`; the key must be the record's id.
pub(crate) fn lane_from(key: &str, value: toml::Value) -> Result<LaneRecord> {
    let record: LaneRecord = value
        .try_into()
        .with_context(|| format!("reading lane {key}"))?;
    let Some(id) = LaneId::parse(key) else {
        bail!("{key} is not a lane id");
    };
    if record.id() != id {
        bail!("lane {key} holds the record of lane {}", record.id());
    }
    Ok(record)
}

/// The lanes state of the project.
pub fn read_state(read: &StateRead) -> Result<LanesState> {
    let mut state = LanesState::default();
    for (key, value) in read.kv(OWNERS_NAMESPACE).scan("")? {
        let owner = owner_from(&key, value)?;
        state.owners.insert(TaskId::new(key), owner);
    }
    for (key, value) in read.kv(LANES_NAMESPACE).scan("")? {
        let lane = lane_from(&key, value)?;
        state.lanes.insert(lane.id(), lane);
    }
    Ok(state)
}

/// Writes the owner record of `task`.
pub fn put_owner(tx: &mut StateTx, task: &TaskId, owner: &OwnerRecord) -> Result<()> {
    let value = toml::Value::try_from(owner).context("serializing a lane owner")?;
    tx.kv(OWNERS_NAMESPACE).put(&task.0, value)
}

/// Writes the record of its lane.
pub fn put_lane(tx: &mut StateTx, lane: &LaneRecord) -> Result<()> {
    let value = toml::Value::try_from(lane).context("serializing a lane")?;
    tx.kv(LANES_NAMESPACE).put(&lane.id().to_string(), value)
}

/// Replaces the lanes state of the project with `state`.
pub fn write_state(tx: &mut StateTx, state: &LanesState) -> Result<()> {
    tx.kv(OWNERS_NAMESPACE).clear()?;
    tx.kv(LANES_NAMESPACE).clear()?;
    for (task, owner) in &state.owners {
        put_owner(tx, task, owner)?;
    }
    for lane in state.lanes.values() {
        put_lane(tx, lane)?;
    }
    Ok(())
}
