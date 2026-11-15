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
                let record: Record =
                    toml::from_str(&text).map_err(|error| LocationError::Parse {
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
        self.locations
            .iter()
            .find(|location| location.id == project_id)
    }

    /// Reads the record from disk again, replacing the copy in memory.
    pub fn reload(&mut self) -> Result<(), LocationError> {
        *self = Self::load(self.path.clone())?;
        Ok(())
    }

    /// Adds `location`, or replaces the one with its id, and writes the record atomically. The
    /// record is read again from disk under a lock first, so what another copy recorded since
    /// this one was loaded is kept; the copy in memory is then the record on disk. On a failed
    /// write the record in memory is as it was.
    pub fn record(&mut self, location: Location) -> Result<(), LocationError> {
        self.change_on_disk(|locations| {
            match locations.iter_mut().find(|known| known.id == location.id) {
                Some(known) => *known = location,
                None => locations.push(location),
            }
        })
    }

    /// Removes the entry equal to `location` from the record on disk (read again under the
    /// lock, as `record` does), leaving any other entry with its id alone.
    pub fn forget(&mut self, location: &Location) -> Result<(), LocationError> {
        self.change_on_disk(|locations| locations.retain(|known| known != location))
    }

    /// Under an exclusive lock on the record's lock file: reads the record from disk, applies
    /// `change`, and writes it atomically; then keeps the result in memory.
    fn change_on_disk(
        &mut self,
        change: impl FnOnce(&mut Vec<Location>),
    ) -> Result<(), LocationError> {
        let write_error = |source| LocationError::Write {
            path: self.path.clone(),
            source,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(write_error)?;
        }
        let _lock = self.lock().map_err(write_error)?;
        let mut locations = Self::load(self.path.clone())?.locations;
        change(&mut locations);
        self.write(&locations)?;
        self.locations = locations;
        Ok(())
    }

    /// Takes the exclusive lock beside the record, waiting while another writer holds it. The
    /// lock file stays; the lock is released when the returned file is closed.
    fn lock(&self) -> io::Result<std::fs::File> {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(name))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            loop {
                // SAFETY: the descriptor is open for the duration of the call.
                let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if result == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Ok(file)
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
        write_atomic(&self.path, text.as_bytes()).map_err(|source| LocationError::Write {
            path: self.path.clone(),
            source,
        })
    }
}
