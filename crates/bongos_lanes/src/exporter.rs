//! The `lanes` state exporter: the owner and lane records of a project, written into a snapshot as
//! `lanes/state.toml` (an `owners` table by task id and a `lanes` table by lane id) and read back
//! on restore. The records pass through as they are stored; an import reads every record before it
//! replaces anything, so a malformed snapshot leaves the state alone.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use bongos_project_repo::{StateExporter, StateExporters, StateRead, StateTx};
use util::rel_path::{RelPath, RelPathBuf};

use crate::store::{LANES_NAMESPACE, OWNERS_NAMESPACE, lane_from, owner_from};

/// The namespace of the exporter, and the folder of the snapshot it owns.
pub const EXPORT_NAMESPACE: &str = "lanes";
/// The file of the snapshot that holds the records.
pub const STATE_FILE: &str = "lanes/state.toml";

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
        let mut file = toml::Table::new();
        for (namespace, table_name) in [
            (OWNERS_NAMESPACE, OWNERS_TABLE),
            (LANES_NAMESPACE, LANES_TABLE),
        ] {
            let rows: toml::Table = read.kv(namespace).scan("")?.into_iter().collect();
            if !rows.is_empty() {
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
        let mut owners: Vec<(String, toml::Value)> = Vec::new();
        let mut lanes: Vec<(String, toml::Value)> = Vec::new();
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
        for (task, value) in &owners {
            owner_from(task, value.clone())?;
        }
        for (key, value) in &lanes {
            lane_from(key, value.clone())?;
        }
        tx.kv(OWNERS_NAMESPACE).clear()?;
        tx.kv(LANES_NAMESPACE).clear()?;
        for (task, value) in owners {
            tx.kv(OWNERS_NAMESPACE).put(&task, value)?;
        }
        for (key, value) in lanes {
            tx.kv(LANES_NAMESPACE).put(&key, value)?;
        }
        Ok(())
    }
}
