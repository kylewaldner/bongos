//! The per-machine record of where each project's parts live. The only reader and writer of
//! `project-repositories.toml`.

use std::io;
use std::path::{Path, PathBuf};

use bongos_work_model::files::write_atomic;
use serde::{Deserialize, Serialize};

/// The file name inside the per-machine state directory.
pub const LOCATIONS_FILE: &str = "project-repositories.toml";
const SCHEMA: u32 = 1;

/// Where one project's parts are on this machine, resolved when it was created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// The id in the project's `project.toml`.
    pub id: String,
    pub folder: PathBuf,
    pub repository: PathBuf,
    pub working_copies: PathBuf,
    pub state: PathBuf,
    pub files: PathBuf,
    /// The backup remote, per machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Set only by conversion of an old-format project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum LocationError {
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not a valid project location record: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path} has schema {found}, which this version cannot read")]
    Schema { path: PathBuf, found: u32 },
    #[error("writing {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    #[serde(default, rename = "project")]
    projects: Vec<Location>,
}

/// Every project location on this machine.
#[derive(Clone, Debug)]
pub struct RepositoryLocations {
    path: PathBuf,
    locations: Vec<Location>,
}

impl gpui::Global for RepositoryLocations {}

impl RepositoryLocations {
    /// The record in the per-machine state directory.
    pub fn default_path() -> PathBuf {
        paths::state_dir().join(LOCATIONS_FILE)
    }

    /// Reads the record at `path`; a missing file is an empty record.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, LocationError> {
        let path = path.into();
        let locations = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let record: Record = toml::from_str(&text).map_err(|error| LocationError::Parse {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
                if record.schema != SCHEMA {
                    return Err(LocationError::Schema {
                        path,
                        found: record.schema,
                    });
                }
                record.projects
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(source) => return Err(LocationError::Read { path, source }),
        };
        Ok(Self { path, locations })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    pub fn location(&self, project_id: &str) -> Option<&Location> {
        self.locations.iter().find(|location| location.id == project_id)
    }

    /// Adds `location`, or replaces the one with its id, and writes the record atomically. On a
    /// failed write the record in memory is as it was.
    pub fn record(&mut self, location: Location) -> Result<(), LocationError> {
        let mut locations = self.locations.clone();
        match locations.iter_mut().find(|known| known.id == location.id) {
            Some(known) => *known = location,
            None => locations.push(location),
        }
        self.write(&locations)?;
        self.locations = locations;
        Ok(())
    }

    fn write(&self, locations: &[Location]) -> Result<(), LocationError> {
        let record = Record {
            schema: SCHEMA,
            projects: locations.to_vec(),
        };
        let text = toml::to_string(&record).map_err(|error| LocationError::Write {
            path: self.path.clone(),
            source: io::Error::other(error.to_string()),
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LocationError::Write {
                path: self.path.clone(),
                source,
            })?;
        }
        write_atomic(&self.path, text.as_bytes()).map_err(|source| LocationError::Write {
            path: self.path.clone(),
            source,
        })
    }
}
