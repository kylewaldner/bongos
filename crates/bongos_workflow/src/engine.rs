//! The `WorkflowEngine` global: the definition layers and the commands that work on them. It
//! exists only once workflows are on: [`init`](crate::init) creates it when `workflows.enabled` is
//! already true, and a command creates it on first use. While `workflows.enabled` is false nothing
//! creates it, so nothing is read, written or materialized. There is no run machinery yet.

use std::path::PathBuf;

use bongos_workflow_model::LayerDirs;
use gpui::{App, Global};
use settings::Settings as _;

use crate::WorkflowsSettings;
use crate::definitions::{
    self, DefinitionError, NewDefinition, Report, SHIPPED_FILES, materialize, profile_dir,
    shipped_dir,
};

/// The layers of definitions, resolved fresh by every command. Cheap to clone, so a command takes a
/// copy to its background task.
#[derive(Clone, Debug)]
pub struct WorkflowEngine {
    dirs: LayerDirs,
}

impl Global for WorkflowEngine {}

impl WorkflowEngine {
    /// An engine over `dirs`; nothing is read or written.
    pub fn new(dirs: LayerDirs) -> Self {
        Self { dirs }
    }

    /// The engine of the running app: the shipped directory of this version (written when
    /// missing, empty set for now), the profile directory under the home directory. The project
    /// layer is not wired yet; see [`WorkflowEngine::with_project`].
    fn open(cx: &App) -> Self {
        let version = release_channel::AppVersion::global(cx);
        let version = format!("{}.{}.{}", version.major, version.minor, version.patch);
        let shipped = shipped_dir(paths::data_dir(), &version);
        if let Err(error) = materialize(&shipped, SHIPPED_FILES) {
            log::warn!("workflows: could not write the shipped definitions: {error}");
        }
        Self::new(LayerDirs {
            shipped: Some(shipped),
            profile: Some(profile_dir(util::paths::home_dir())),
            project: None,
        })
    }

    /// The same engine with the project layer set to `dir`.
    pub fn with_project(mut self, dir: Option<PathBuf>) -> Self {
        self.dirs.project = dir;
        self
    }

    /// The global engine, created on first use. `None` while `workflows.enabled` is false, when
    /// nothing may be read or written.
    pub fn ensure(cx: &mut App) -> Option<Self> {
        if !WorkflowsSettings::get_global(cx).enabled {
            return None;
        }
        if let Some(engine) = cx.try_global::<Self>() {
            return Some(engine.clone());
        }
        let engine = Self::open(cx);
        cx.set_global(engine.clone());
        Some(engine)
    }

    pub fn dirs(&self) -> &LayerDirs {
        &self.dirs
    }

    /// Checks the definition `name`, or every definition with no name.
    pub fn validate(&self, name: Option<&str>) -> Result<Report, DefinitionError> {
        definitions::validate_definitions(&self.dirs, name)
    }

    /// Creates a definition from the minimal template in the profile layer.
    pub fn new_definition(&self, name: Option<&str>) -> Result<NewDefinition, DefinitionError> {
        definitions::new_definition(self.dirs.profile.as_deref(), name)
    }

    /// The `workflow.toml` of the definition `name`, or of the first by name with no name (until
    /// the picker of step 17).
    pub fn definition_path(&self, name: Option<&str>) -> Result<PathBuf, DefinitionError> {
        definitions::definition_path(&self.dirs, name)
    }
}

/// Creates the global engine when `workflows.enabled` is true. Call after [`crate::init`].
pub(crate) fn init(cx: &mut App) {
    WorkflowEngine::ensure(cx);
}
