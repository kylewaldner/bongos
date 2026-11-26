//! Locations no job may write: the run-state database, the master repository, the engine host
//! socket and the approval keys.
//!
//! The process registers its protected locations once it knows them ([`set`]). Every plan reads
//! one snapshot ([`current`]) and refuses a writable root that equals, contains or lies inside one
//! of them, and the refusal annotations name the rule a write to one of them broke.

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
};

/// What a protected location holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectedKind {
    RunState,
    Master,
    HostSocket,
    Approvals,
}

impl ProtectedKind {
    /// The location's name in a refusal, as in "overlaps the run-state database".
    pub fn noun(self) -> &'static str {
        match self {
            Self::RunState => "the run-state database",
            Self::Master => "the master repository",
            Self::HostSocket => "the engine host socket",
            Self::Approvals => "the approval keys",
        }
    }

    /// The rule a job broke by touching the location.
    pub fn rule(self) -> &'static str {
        match self {
            Self::RunState => "writing run state from a job is refused",
            Self::Master => "writing the master repository from a job is refused",
            Self::HostSocket => "talking to the engine host from a job is refused",
            Self::Approvals => "writing approval keys from a job is refused",
        }
    }
}

/// One location no job may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedLocation {
    pub path: PathBuf,
    pub kind: ProtectedKind,
}

static LOCATIONS: RwLock<Vec<ProtectedLocation>> = RwLock::new(Vec::new());

/// Replaces this process's protected locations.
pub fn set(locations: Vec<ProtectedLocation>) {
    *LOCATIONS.write().unwrap_or_else(|error| error.into_inner()) = locations;
}

/// A snapshot of this process's protected locations.
pub fn current() -> Vec<ProtectedLocation> {
    LOCATIONS
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// The forms a path is compared in: as given and, when absolute, resolved. A path that cannot be
/// canonicalized is resolved as far as its deepest existing ancestor, with the rest appended.
pub(crate) fn forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = vec![path.to_path_buf()];
    if let Some(resolved) = resolve(path)
        && resolved != path
    {
        forms.push(resolved);
    }
    forms
}

fn resolve(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    let mut rest = Vec::new();
    let mut ancestor = path;
    loop {
        rest.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
        if let Ok(mut resolved) = std::fs::canonicalize(ancestor) {
            for name in rest.iter().rev() {
                resolved.push(name);
            }
            return Some(resolved);
        }
    }
}

/// Whether two paths are equal or one lies inside the other.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
