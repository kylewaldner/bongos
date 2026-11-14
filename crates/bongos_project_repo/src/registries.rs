//! Registries other features fill from their `init`: exporters of their state into a snapshot,
//! and sources of activity that a restore must wait for.

use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use util::rel_path::RelPathBuf;

use crate::state::{StateRead, StateTx};

/// Writes one feature's `kv` state into a snapshot and reads it back on restore.
pub trait StateExporter: Send + Sync + 'static {
    /// The file or folder of the snapshot this exporter owns.
    fn namespace(&self) -> &str;

    /// The files of the snapshot, relative to its root.
    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>>;

    /// Replaces the feature's state with the files of a snapshot.
    fn import(&self, tx: &mut StateTx, files: &[(RelPathBuf, String)]) -> Result<()>;
}

/// Reports work running against a project (runs, lanes), which a restore refuses to replace.
pub trait ActivitySource: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// A line for each activity running in the project, empty when idle.
    fn activity(&self, project_id: &str) -> Vec<String>;
}

pub struct StateExporters {
    exporters: Mutex<Vec<Arc<dyn StateExporter>>>,
}

static GLOBAL_EXPORTERS: StateExporters = StateExporters {
    exporters: Mutex::new(Vec::new()),
};

impl StateExporters {
    pub fn global() -> &'static Self {
        &GLOBAL_EXPORTERS
    }

    pub fn register(&self, exporter: Arc<dyn StateExporter>) {
        self.exporters.lock().push(exporter);
    }

    pub fn all(&self) -> Vec<Arc<dyn StateExporter>> {
        self.exporters.lock().clone()
    }
}

pub struct ActivitySources {
    sources: Mutex<Vec<Arc<dyn ActivitySource>>>,
}

static GLOBAL_ACTIVITY_SOURCES: ActivitySources = ActivitySources {
    sources: Mutex::new(Vec::new()),
};

impl ActivitySources {
    pub fn global() -> &'static Self {
        &GLOBAL_ACTIVITY_SOURCES
    }

    pub fn register(&self, source: Arc<dyn ActivitySource>) {
        self.sources.lock().push(source);
    }

    pub fn all(&self) -> Vec<Arc<dyn ActivitySource>> {
        self.sources.lock().clone()
    }
}
