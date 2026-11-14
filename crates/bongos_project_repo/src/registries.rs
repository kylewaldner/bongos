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

    /// Asks the activity of the project to stop at its next job boundary (a run between two
    /// steps, a lane between two jobs), so a restore can follow. Returns at once; the
    /// activity ends later. A source that cannot be stopped from here says so.
    fn stop_at_job_boundary(&self, project_id: &str) -> Result<()> {
        let _ = project_id;
        anyhow::bail!(
            "{} cannot be stopped from here; stop it where it runs",
            self.name()
        )
    }

    /// Holds back new work in the project (a run or lane job that would start) until
    /// [`Self::release_new_work`]: a restore takes the hold before it checks the activity and
    /// releases it once its import ended, so no job starts between the check and the import
    /// and writes the state the restore replaces. Work held back starts after the release. A
    /// source that never starts work leaves the default, which holds nothing.
    fn hold_new_work(&self, project_id: &str) {
        let _ = project_id;
    }

    /// Ends a hold of [`Self::hold_new_work`]; held work may start again.
    fn release_new_work(&self, project_id: &str) {
        let _ = project_id;
    }
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
