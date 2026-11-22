//! The run state of lanes in a project's state store, all in the `kv` namespace
//! [`LANES_NAMESPACE`]: one owner record per owned top-level task under `owner/<task id>`, and one
//! lane record per lane under `lane/<lane id>` (`lane/build-2`). Later records of the namespace
//! (the dispatcher's, interruptions, notifications) use other key prefixes.

use anyhow::{Context as _, Result, bail};
use bongos_lanes_model::{LaneId, LaneRecord, LanesState, OwnerRecord, TaskId};
use bongos_project_repo::{StateRead, StateTx};

/// The `kv` namespace of the run state of lanes.
pub const LANES_NAMESPACE: &str = "lanes";
/// The key prefix of the owner records, followed by the task id.
pub const OWNER_PREFIX: &str = "owner/";
/// The key prefix of the lane records, followed by the lane id.
pub const LANE_PREFIX: &str = "lane/";

/// Reads the owner record of `task` from `value`.
pub(crate) fn owner_from(task: &str, value: toml::Value) -> Result<OwnerRecord> {
    if task.is_empty() {
        bail!("an owner record names no task");
    }
    value
        .try_into()
        .with_context(|| format!("reading the lane owner of task {task}"))
}

/// Reads the record of lane `id` from `value`; `id` must be the record's own id.
pub(crate) fn lane_from(id: &str, value: toml::Value) -> Result<LaneRecord> {
    let record: LaneRecord = value
        .try_into()
        .with_context(|| format!("reading lane {id}"))?;
    let Some(parsed) = LaneId::parse(id) else {
        bail!("{id} is not a lane id");
    };
    if record.id() != parsed {
        bail!("lane {id} holds the record of lane {}", record.id());
    }
    Ok(record)
}

/// The owner records, by task id, and the lane records, by lane id, as stored.
pub(crate) fn rows(
    read: &StateRead,
) -> Result<(Vec<(String, toml::Value)>, Vec<(String, toml::Value)>)> {
    let strip = |prefix: &str| -> Result<Vec<(String, toml::Value)>> {
        Ok(read
            .kv(LANES_NAMESPACE)
            .scan(prefix)?
            .into_iter()
            .map(|(key, value)| (key[prefix.len()..].to_string(), value))
            .collect())
    };
    Ok((strip(OWNER_PREFIX)?, strip(LANE_PREFIX)?))
}

/// The lanes state of the project.
pub fn read_state(read: &StateRead) -> Result<LanesState> {
    let (owners, lanes) = rows(read)?;
    state_from(owners, lanes)
}

/// The lanes state of the project as the transaction `tx` sees it.
pub fn read_state_in(tx: &mut StateTx) -> Result<LanesState> {
    let kv = tx.kv(LANES_NAMESPACE);
    let strip = |prefix: &str| -> Result<Vec<(String, toml::Value)>> {
        Ok(kv
            .scan(prefix)?
            .into_iter()
            .map(|(key, value)| (key[prefix.len()..].to_string(), value))
            .collect())
    };
    let owners = strip(OWNER_PREFIX)?;
    let lanes = strip(LANE_PREFIX)?;
    state_from(owners, lanes)
}

fn state_from(
    owners: Vec<(String, toml::Value)>,
    lanes: Vec<(String, toml::Value)>,
) -> Result<LanesState> {
    let mut state = LanesState::default();
    for (task, value) in owners {
        let owner = owner_from(&task, value)?;
        state.owners.insert(TaskId::new(task), owner);
    }
    for (id, value) in lanes {
        let lane = lane_from(&id, value)?;
        state.lanes.insert(lane.id(), lane);
    }
    Ok(state)
}

/// Writes the owner record of `task`.
pub fn put_owner(tx: &mut StateTx, task: &TaskId, owner: &OwnerRecord) -> Result<()> {
    let value = toml::Value::try_from(owner).context("serializing a lane owner")?;
    tx.kv(LANES_NAMESPACE)
        .put(&format!("{OWNER_PREFIX}{}", task.0), value)
}

/// Writes the record of its lane.
pub fn put_lane(tx: &mut StateTx, lane: &LaneRecord) -> Result<()> {
    let value = toml::Value::try_from(lane).context("serializing a lane")?;
    tx.kv(LANES_NAMESPACE)
        .put(&format!("{LANE_PREFIX}{}", lane.id()), value)
}

/// Removes the record of lane `id`.
pub fn delete_lane(tx: &mut StateTx, id: &LaneId) -> Result<()> {
    tx.kv(LANES_NAMESPACE).delete(&format!("{LANE_PREFIX}{id}"))
}

/// Removes every owner and lane record, leaving the other records of the namespace.
pub(crate) fn clear_owners_and_lanes(tx: &mut StateTx) -> Result<()> {
    let mut kv = tx.kv(LANES_NAMESPACE);
    let mut keys = Vec::new();
    for prefix in [OWNER_PREFIX, LANE_PREFIX] {
        keys.extend(kv.scan(prefix)?.into_iter().map(|(key, _)| key));
    }
    for key in keys {
        kv.delete(&key)?;
    }
    Ok(())
}

/// Replaces the owner and lane records of the project with those of `state`.
pub fn write_state(tx: &mut StateTx, state: &LanesState) -> Result<()> {
    clear_owners_and_lanes(tx)?;
    for (task, owner) in &state.owners {
        put_owner(tx, task, owner)?;
    }
    for lane in state.lanes.values() {
        put_lane(tx, lane)?;
    }
    Ok(())
}
