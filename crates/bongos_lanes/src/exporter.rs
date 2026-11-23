//! The `lanes` state exporter: the owner and lane records of a project, written into a snapshot as
//! `lanes.toml` (an `owners` table by task id and a `lanes` table by lane id) and read back on
//! restore. The records pass through as they are stored. The dispatcher record is never exported,
//! so a restore never claims a live host. An import reads every record before it replaces
//! anything, so a malformed snapshot leaves the state alone.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use bongos_project_repo::{StateExporter, StateExporters, StateRead, StateTx};
use util::rel_path::{RelPath, RelPathBuf};

use crate::store::{LANES_NAMESPACE, lane_from, owner_from, put_lane, put_owner, rows};

/// The namespace of the exporter: the one file of the snapshot it owns.
pub const EXPORT_NAMESPACE: &str = "lanes.toml";
/// The file of the snapshot that holds the records.
pub const STATE_FILE: &str = "lanes.toml";

const OWNERS_TABLE: &str = "owners";
const LANES_TABLE: &str = "lanes";

/// Writes the run state of lanes into a snapshot and reads it back.
pub struct LanesExporter;

impl LanesExporter {
    /// Registers the exporter in the app's registry.
    pub fn register() {
        StateExporters::global().register(Arc::new(LanesExporter));
    }
}

impl StateExporter for LanesExporter {
    fn namespace(&self) -> &str {
        EXPORT_NAMESPACE
    }

    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        let (owners, lanes) = rows(read)?;
        let mut file = toml::Table::new();
        for (table_name, rows) in [(OWNERS_TABLE, owners), (LANES_TABLE, lanes)] {
            if !rows.is_empty() {
                let rows: toml::Table = rows.into_iter().collect();
                file.insert(table_name.to_string(), toml::Value::Table(rows));
            }
        }
        if file.is_empty() {
            return Ok(Vec::new());
        }
        let text = toml::to_string(&file).context("serializing the lanes state")?;
        Ok(vec![(RelPath::unix(STATE_FILE)?.to_rel_path_buf(), text)])
    }

    fn import(&self, tx: &mut StateTx, files: &[(RelPathBuf, String)]) -> Result<()> {
        let mut owners = Vec::new();
        let mut lanes = Vec::new();
        for (path, text) in files {
            let shown = path.as_unix_str();
            if shown != STATE_FILE {
                bail!("{shown} is not a file of the lanes state");
            }
            let mut table: toml::Table =
                toml::from_str(text).with_context(|| format!("reading {shown}"))?;
            for (table_name, rows) in [(OWNERS_TABLE, &mut owners), (LANES_TABLE, &mut lanes)] {
                match table.remove(table_name) {
                    None => {}
                    Some(toml::Value::Table(found)) => rows.extend(found),
                    Some(_) => bail!("{shown}: `{table_name}` is not a table"),
                }
            }
            if let Some(unknown) = table.keys().next() {
                bail!("{shown} has the unknown key `{unknown}`");
            }
        }
        let owners = owners
            .into_iter()
            .map(|(task, value)| Ok((task.clone(), owner_from(&task, value)?)))
            .collect::<Result<Vec<_>>>()?;
        let lanes = lanes
            .into_iter()
            .map(|(id, value)| lane_from(&id, value))
            .collect::<Result<Vec<_>>>()?;
        // The restored state is exactly the snapshot's: no dispatcher record (no live host), no
        // interruption or notification marks of the replaced state.
        tx.kv(LANES_NAMESPACE).clear()?;
        for (task, owner) in owners {
            put_owner(tx, &bongos_lanes_model::TaskId::new(task), &owner)?;
        }
        for lane in lanes {
            put_lane(tx, &lane)?;
        }
        Ok(())
    }
}
