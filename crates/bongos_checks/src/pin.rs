//! The pin of a check run: a read-only copy of the definitions at one commit of master. Every
//! script and pinned file a check names is read from it, so an edit in a working copy or a tree
//! never changes what a running check reads.

use std::path::{Path, PathBuf};

use bongos_project_repo::{PinGuard, ProjectRepo};
use bongos_project_repo_model::RelPath;
use bongos_project_repo_model::layout::DEFINITIONS_DIR;

/// A materialized commit of the definitions, removed when the last holder drops it.
#[derive(Debug)]
pub struct Pin {
    guard: PinGuard,
    commit: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinPathError {
    #[error("{0} is not a path inside definitions/ (no absolute path, no .. part)")]
    Escapes(String),
    #[error("{0} does not exist in the definitions at the pinned commit")]
    Missing(String),
}

impl Pin {
    /// Materializes `commit` of `repo` read-only.
    pub async fn materialize(repo: &ProjectRepo, commit: &str) -> anyhow::Result<Self> {
        let guard = repo.materialize(commit).await?;
        Ok(Self {
            guard,
            commit: commit.to_string(),
        })
    }

    /// The commit the pin holds, as given.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// The pin's `definitions/` folder; `BONGOS_CHECK_PINNED` points here.
    pub fn definitions(&self) -> PathBuf {
        self.guard.path().join(DEFINITIONS_DIR)
    }

    /// The file at `relative` (relative to `definitions/`) inside the pin. Refuses an absolute
    /// path, a `..` part and anything that resolves outside the pin's `definitions/`.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, PinPathError> {
        resolve_in(&self.definitions(), relative)
    }
}

pub(crate) fn resolve_in(definitions: &Path, relative: &str) -> Result<PathBuf, PinPathError> {
    let escapes = || PinPathError::Escapes(relative.to_string());
    let path = RelPath::new(relative).ok_or_else(escapes)?;
    let joined = definitions.join(path.as_str());
    let real = joined
        .canonicalize()
        .map_err(|_| PinPathError::Missing(relative.to_string()))?;
    let root = definitions
        .canonicalize()
        .map_err(|_| PinPathError::Missing(relative.to_string()))?;
    if !real.starts_with(&root) {
        return Err(escapes());
    }
    if !real.is_file() {
        return Err(PinPathError::Missing(relative.to_string()));
    }
    Ok(real)
}

/// `definitions/` of the project's master checkout; `BONGOS_CHECK_MASTER` points here.
pub fn master_definitions(repo: &ProjectRepo) -> PathBuf {
    repo.location().repository.join(DEFINITIONS_DIR)
}
