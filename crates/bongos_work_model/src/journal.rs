//! The `.op.toml` journal of a multi-file operation (`split`, `move`, `move to project`,
//! `make one-off`, `drop` of a group).
//!
//! An operation first writes its journal atomically into the source parent, then runs the steps,
//! then deletes the journal. Every step is idempotent, so the store finishes any journal it finds
//! at load by running all of its steps again from the first. A crash therefore leaves the old tree
//! (no journal yet), a journal that completes the operation, or the new tree; never a task in
//! neither place or in two, because a task directory only ever moves by one `rename`.
//!
//! ```toml
//! schema = 1
//! operation = "move"
//! ids = ["0196f1c2-..."]
//!
//! [[step]]
//! action = "create-dir"
//! path = "projects/billing/tasks"
//!
//! [[step]]
//! action = "rename"
//! from = "projects/invoicing/tasks/backfill"
//! to = "projects/billing/tasks/backfill"
//!
//! [[step]]
//! action = "write-file"
//! path = "projects/billing/tasks/backfill/task.toml"
//! content = "..."
//! ```
//!
//! Paths are relative to the work root and may not leave it. A journal that does not parse, has
//! another schema or names a path outside the root is never run and never deleted.
//!
//! The writer of a journal must check, before writing it, that every `rename` target is absent:
//! a `rename` whose target exists is taken as already done.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::files::write_atomic;
use crate::layout::op_file;
use crate::types::{Extra, SCHEMA};

fn schema_one() -> u32 {
    SCHEMA
}

/// `.op.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default = "schema_one")]
    pub schema: u32,
    /// The operation's name, such as `split` or `move`; informational.
    pub operation: String,
    /// The ids of the tasks the operation touches; informational.
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Journal {
    pub fn new(operation: impl Into<String>, ids: Vec<String>, steps: Vec<Step>) -> Self {
        Self {
            schema: SCHEMA,
            operation: operation.into(),
            ids,
            steps,
            extra: Extra::new(),
        }
    }
}

/// One idempotent step. Paths are relative to the work root.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Step {
    /// Create a directory and its parents if missing.
    CreateDir { path: String },
    /// Rename a file or directory if `from` exists and `to` does not; otherwise nothing.
    Rename { from: String, to: String },
    /// Write a file atomically if its content differs. The parent directory must exist.
    WriteFile { path: String, content: String },
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidJournal(message.into()))
}

/// Resolves a journal path under `root`, refusing absolute paths and `..`.
fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid(format!(
            "path {relative:?} must be relative to the work root and stay inside it"
        )));
    }
    Ok(root.join(path))
}

/// Parses a journal and checks its schema and every path, without touching the disk.
pub fn parse_journal(text: &str, root: &Path) -> Result<Journal> {
    let journal: Journal = toml::from_str(text).map_err(|error| Error::syntax(text, &error))?;
    if journal.schema != SCHEMA {
        return Err(Error::new(ErrorKind::UnsupportedSchema {
            found: Some(journal.schema.to_string()),
        }));
    }
    for step in &journal.steps {
        match step {
            Step::CreateDir { path } | Step::WriteFile { path, .. } => {
                resolve(root, path)?;
            }
            Step::Rename { from, to } => {
                resolve(root, from)?;
                resolve(root, to)?;
            }
        }
    }
    Ok(journal)
}

pub fn serialize_journal(journal: &Journal) -> Result<String> {
    toml::to_string(journal).map_err(|error| Error::new(ErrorKind::Serialize(error.to_string())))
}

/// Writes `journal` atomically as `.op.toml` in `dir`, the source parent of the operation.
pub fn write_journal(root: &Path, dir: &Path, journal: &Journal) -> Result<PathBuf> {
    let text = serialize_journal(journal)?;
    // Refuse to write a journal the loader would not run.
    parse_journal(&text, root)?;
    let path = op_file(dir);
    write_atomic(&path, text.as_bytes()).map_err(|error| Error::io(&path, error))?;
    Ok(path)
}

fn run_step(root: &Path, step: &Step) -> Result<()> {
    match step {
        Step::CreateDir { path } => {
            let path = resolve(root, path)?;
            std::fs::create_dir_all(&path).map_err(|error| Error::io(&path, error))
        }
        Step::Rename { from, to } => {
            let from = resolve(root, from)?;
            let to = resolve(root, to)?;
            let source_exists = from
                .symlink_metadata()
                .map(|_| true)
                .or_else(|error| missing_is_false(&from, error))?;
            let target_exists = to
                .symlink_metadata()
                .map(|_| true)
                .or_else(|error| missing_is_false(&to, error))?;
            if source_exists && !target_exists {
                std::fs::rename(&from, &to).map_err(|error| Error::io(&from, error))?;
                if let Some(parent) = to.parent() {
                    sync_dir(parent)?;
                }
                if let Some(parent) = from.parent() {
                    sync_dir(parent)?;
                }
            }
            Ok(())
        }
        Step::WriteFile { path, content } => {
            let path = resolve(root, path)?;
            match std::fs::read(&path) {
                Ok(existing) if existing == content.as_bytes() => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io(&path, error)),
            }
            write_atomic(&path, content.as_bytes()).map_err(|error| Error::io(&path, error))
        }
    }
}

fn missing_is_false(path: &Path, error: std::io::Error) -> Result<bool> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(Error::io(path, error))
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::io(dir, error))
}

/// Runs the first `count` steps of `journal`. The test seam for a crash between steps.
pub(crate) fn run_steps(root: &Path, journal: &Journal, count: usize) -> Result<()> {
    for step in journal.steps.iter().take(count) {
        run_step(root, step)?;
    }
    Ok(())
}

/// Finishes the journal at `path`: runs every step, then deletes it. A journal that cannot be
/// read, parsed or completed is left in place and the error names it.
pub fn finish_journal(root: &Path, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let journal = parse_journal(&text, root).map_err(|error| error.with_path(Some(path)))?;
    run_steps(root, &journal, journal.steps.len()).map_err(|error| {
        let reason = error.to_string();
        Error::new(ErrorKind::InvalidJournal(format!(
            "could not finish operation {:?}: {reason}",
            journal.operation
        )))
        .with_path(Some(path))
    })?;
    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Writes `journal` into `dir`, runs it and deletes it: the whole of a multi-file operation.
pub fn run_operation(root: &Path, dir: &Path, journal: &Journal) -> Result<()> {
    let path = write_journal(root, dir, journal)?;
    finish_journal(root, &path)
}
