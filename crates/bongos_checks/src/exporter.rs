//! The `checks` state exporter: the evidence records, owed markers, done records and last-child
//! rows of every task, and the generation counter, written into a snapshot and read back on
//! restore, so generations keep their order and every group keeps its last child after a
//! restore. The live state (`checks.live`) is never exported, and output files are never in a
//! snapshot.
//!
//! The snapshot gets `checks/<task id>.toml` for each task (its `evidence` table keyed by the
//! rest of the record key, and its `owed`, `done` and `last` rows), `checks/landing.toml` for the
//! records of landing runs, and `checks/seq.toml` for the counter. The rows are passed through as
//! they are stored; nothing is recomputed on import.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use bongos_project_repo::{StateExporter, StateExporters, StateRead, StateTx};
use util::rel_path::{RelPath, RelPathBuf};

use crate::evidence_store::{
    DONE_NAMESPACE, EVIDENCE_NAMESPACE, LAST_NAMESPACE, OWED_NAMESPACE, SEQ_KEY, SEQ_NAMESPACE,
};

/// The namespace of the exporter, and the folder of the snapshot it owns.
pub const EXPORT_NAMESPACE: &str = "checks";

const SEQ_FILE: &str = "seq";
const EVIDENCE_TABLE: &str = "evidence";
/// The namespaces that hold one row per task or group, by the table name in a task's file.
const ROW_TABLES: [(&str, &str); 3] = [
    (OWED_NAMESPACE, "owed"),
    (DONE_NAMESPACE, "done"),
    (LAST_NAMESPACE, "last"),
];

/// Writes the state of task checks into a snapshot and reads it back.
pub struct ChecksExporter;

impl ChecksExporter {
    /// Registers the exporter in the app's registry.
    pub fn register() {
        StateExporters::global().register(Arc::new(ChecksExporter));
    }
}

fn file_path(name: &str) -> Result<RelPathBuf> {
    Ok(RelPath::unix(&format!("{EXPORT_NAMESPACE}/{name}.toml"))?.to_rel_path_buf())
}

impl StateExporter for ChecksExporter {
    fn namespace(&self) -> &str {
        EXPORT_NAMESPACE
    }

    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        let mut files: BTreeMap<String, toml::Table> = BTreeMap::new();
        for (key, value) in read.kv(EVIDENCE_NAMESPACE).scan("")? {
            let Some((owner, rest)) = key.split_once('/') else {
                continue;
            };
            let table = files.entry(owner.to_string()).or_default();
            let evidence = table
                .entry(EVIDENCE_TABLE.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(evidence) = evidence {
                evidence.insert(rest.to_string(), value);
            }
        }
        for (namespace, table_name) in ROW_TABLES {
            for (key, value) in read.kv(namespace).scan("")? {
                files
                    .entry(key)
                    .or_default()
                    .insert(table_name.to_string(), value);
            }
        }
        let mut out = Vec::new();
        for (name, table) in files {
            if name == SEQ_FILE {
                continue;
            }
            let text = toml::to_string(&table)
                .with_context(|| format!("serializing the checks state of {name}"))?;
            out.push((file_path(&name)?, text));
        }
        if let Some(value) = read.kv(SEQ_NAMESPACE).get(SEQ_KEY)? {
            let mut table = toml::Table::new();
            table.insert(SEQ_KEY.to_string(), value);
            out.push((file_path(SEQ_FILE)?, toml::to_string(&table)?));
        }
        Ok(out)
    }

    fn import(&self, tx: &mut StateTx, files: &[(RelPathBuf, String)]) -> Result<()> {
        // Read everything before the state is replaced, so a malformed file leaves it alone.
        let mut evidence: Vec<(String, toml::Value)> = Vec::new();
        let mut rows: Vec<(&str, String, toml::Value)> = Vec::new();
        let mut seq = None;
        for (path, text) in files {
            let shown = path.as_unix_str();
            let name = shown
                .strip_prefix("checks/")
                .and_then(|rest| rest.strip_suffix(".toml"))
                .filter(|name| !name.is_empty() && !name.contains('/'))
                .with_context(|| format!("{shown} is not a file of the checks state"))?;
            let mut table: toml::Table =
                toml::from_str(text).with_context(|| format!("reading {shown}"))?;
            if name == SEQ_FILE {
                let value = table
                    .remove(SEQ_KEY)
                    .with_context(|| format!("{shown} has no `{SEQ_KEY}`"))?;
                if value.as_integer().is_none() {
                    bail!("{shown}: `{SEQ_KEY}` is not a number");
                }
                seq = Some(value);
                continue;
            }
            if let Some(toml::Value::Table(records)) = table.remove(EVIDENCE_TABLE) {
                for (rest, value) in records {
                    evidence.push((format!("{name}/{rest}"), value));
                }
            }
            for (namespace, table_name) in ROW_TABLES {
                if let Some(value) = table.remove(table_name) {
                    rows.push((namespace, name.to_string(), value));
                }
            }
            if let Some(unknown) = table.keys().next() {
                bail!("{shown} has the unknown key `{unknown}`");
            }
        }
        for namespace in [
            EVIDENCE_NAMESPACE,
            OWED_NAMESPACE,
            DONE_NAMESPACE,
            LAST_NAMESPACE,
            SEQ_NAMESPACE,
        ] {
            tx.kv(namespace).clear()?;
        }
        for (key, value) in evidence {
            tx.kv(EVIDENCE_NAMESPACE).put(&key, value)?;
        }
        for (namespace, key, value) in rows {
            tx.kv(namespace).put(&key, value)?;
        }
        if let Some(value) = seq {
            tx.kv(SEQ_NAMESPACE).put(SEQ_KEY, value)?;
        }
        Ok(())
    }
}
