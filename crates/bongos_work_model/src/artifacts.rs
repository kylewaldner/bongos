//! Artifacts and the shared set (D22).
//!
//! The artifact list of a task is every file in its directory, recursively, except the model's
//! own (the reserved names of [`layout::is_reserved`] at the top, temporary files, journals and
//! `.git` anywhere), merged with `artifacts.toml`: a manifest entry gives a file its description
//! and visibility; a file not yet in the manifest gets the task's `default_visibility` and an empty
//! description (`prompts.txt` is shared whatever the default); an entry whose file is gone is
//! dropped.
//!
//! [`shared_artifacts`] and [`session_context`] are the only way chat sessions learn about a task.
//! They read the files fresh on every call, never from a cache, and leave out every human-only
//! artifact: its path and description never appear in what they return. When the manifest cannot
//! be read they fail closed and list nothing.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::io::{read_artifacts, read_task};
use crate::layout::{self, OP_FILE, TASKS_DIR, is_temp_file};
use crate::store::{Task, collect_files};
use crate::types::{Artifact, ArtifactsFile, Extra, Visibility};

/// The line the UI shows above a task's artifacts.
pub const GUARANTEE: &str = "Locked artifacts are left out of what bongos tells chat sessions. \
The files are not otherwise protected.";

/// An artifact a chat session may be told about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SharedArtifact {
    /// The absolute path of the file.
    pub path: PathBuf,
    pub description: String,
}

/// What a new chat session (or a resume) is told about its task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionContext {
    pub goal: String,
    /// The path of the task's `task.toml`.
    pub task_toml: PathBuf,
    pub task_dir: PathBuf,
    /// The task's worktree as `task.toml` records it.
    pub worktree: Option<PathBuf>,
    /// The goal of the group the task is in, when it is in one.
    pub parent_goal: Option<String>,
    /// The shared artifacts, by path.
    pub artifacts: Vec<SharedArtifact>,
}

/// The manifest key of `relative`, a path under the task directory, when the file is an
/// artifact: its components joined with `/`.
pub fn artifact_key(relative: &Path) -> Option<String> {
    let mut names = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_str()?;
        if is_temp_file(name) || name == OP_FILE || name == ".git" {
            return None;
        }
        names.push(name);
    }
    let first = names.first()?;
    if layout::is_reserved(first) {
        return None;
    }
    Some(names.join("/"))
}

/// The manifest keys of the artifacts among `files` (paths relative to the task directory, such
/// as the keys of [`Task::files`]), sorted and without duplicates.
pub fn artifact_keys<'a>(files: impl IntoIterator<Item = &'a PathBuf>) -> Vec<String> {
    let mut keys: Vec<String> = files
        .into_iter()
        .filter_map(|path| artifact_key(path))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// A manifest path as written by hand, normalized to a key: `./notes//a.md` is `notes/a.md`.
/// Only `/` separates names, as in [`artifact_key`]: on disk `a\b.md` is one file name.
fn normalize(path: &str) -> String {
    path.split('/')
        .filter(|name| !name.is_empty() && *name != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// The visibility of a file that is not in the manifest yet.
fn visibility_of_new(key: &str, default: Visibility) -> Visibility {
    if key == layout::PROMPTS_FILE {
        Visibility::Shared
    } else {
        default
    }
}

/// The artifact list: one entry per key in `keys`, in that order, from `manifest` when it lists
/// the file and otherwise with `default` visibility and no description. An entry without a
/// visibility takes `default` too. Entries whose file is not in `keys` are dropped. When the
/// manifest lists a file twice, the first entry gives the description, and the file is human-only
/// if any entry is. Every entry of the result has its visibility set. The manifest's other keys
/// are kept.
pub fn merge(
    keys: &[String],
    manifest: Option<&ArtifactsFile>,
    default: Visibility,
) -> ArtifactsFile {
    let mut merged = manifest.cloned().unwrap_or_default();
    let listed = std::mem::take(&mut merged.artifacts);
    merged.artifacts = keys
        .iter()
        .map(|key| {
            let mut entries = listed.iter().filter(|entry| normalize(&entry.path) == *key);
            let resolve = |entry: &Artifact| {
                entry
                    .visibility
                    .unwrap_or_else(|| visibility_of_new(key, default))
            };
            match entries.next() {
                Some(first) => {
                    let mut artifact = first.clone();
                    artifact.path = key.clone();
                    let mut visibility = resolve(first);
                    if entries.any(|entry| resolve(entry) == Visibility::HumanOnly) {
                        visibility = Visibility::HumanOnly;
                    }
                    artifact.visibility = Some(visibility);
                    artifact
                }
                None => Artifact {
                    path: key.clone(),
                    description: String::new(),
                    visibility: Some(visibility_of_new(key, default)),
                    extra: Extra::new(),
                },
            }
        })
        .collect();
    merged
}

/// The artifact list of `task` as the snapshot has it: its files merged with its last good parse
/// of `artifacts.toml`. A task with no `task.toml` gives new files the human-only visibility.
pub fn task_artifacts(task: &Task) -> ArtifactsFile {
    let default = task
        .file
        .as_ref()
        .map_or(Visibility::HumanOnly, |file| file.default_visibility);
    merge(
        &artifact_keys(task.files.keys()),
        task.artifacts.as_ref(),
        default,
    )
}

/// The manifest keys of the artifacts in `task_dir`, read from disk now.
fn keys_on_disk(task_dir: &Path) -> Vec<String> {
    let mut files = std::collections::BTreeMap::new();
    collect_files(task_dir, Path::new(""), &mut files, &mut Vec::new());
    artifact_keys(files.keys())
}

/// The artifact list of the task in `task_dir`, read from disk now. An `artifacts.toml` that does
/// not parse is an error; a missing one is an empty manifest. Without a readable `task.toml`, new
/// files are human-only.
pub fn read_task_artifacts(task_dir: &Path) -> Result<ArtifactsFile> {
    let default = read_task(&layout::task_file(task_dir))
        .map_or(Visibility::HumanOnly, |file| file.default_visibility);
    let manifest_path = layout::artifacts_file(task_dir);
    let manifest = match read_artifacts(&manifest_path) {
        Ok(manifest) => Some(manifest),
        Err(Error {
            kind: crate::ErrorKind::Io(error),
            ..
        }) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(merge(&keys_on_disk(task_dir), manifest.as_ref(), default))
}

/// The shared artifacts of the task in `task_dir`, read from disk now: absolute paths and
/// descriptions. A human-only artifact is never in the list. When `artifacts.toml` does not parse,
/// the list is empty.
pub fn shared_artifacts(task_dir: &Path) -> Vec<SharedArtifact> {
    let Ok(list) = read_task_artifacts(task_dir) else {
        return Vec::new();
    };
    list.artifacts
        .into_iter()
        .filter(|artifact| artifact.visibility == Some(Visibility::Shared))
        .map(|artifact| SharedArtifact {
            path: task_dir.join(&artifact.path),
            description: artifact.description,
        })
        .collect()
}

/// The context of a chat session on the task in `task_dir`, read from disk now: its goal, the
/// paths of its `task.toml`, directory and worktree, its group's goal, and its
/// [`shared_artifacts`]. Fails when `task.toml` cannot be read.
pub fn session_context(task_dir: &Path) -> Result<SessionContext> {
    let task_toml = layout::task_file(task_dir);
    let file = read_task(&task_toml)?;
    // A nested task lives in `<parent>/tasks/<slug>/`.
    let parent_goal = task_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == TASKS_DIR))
        .and_then(Path::parent)
        .and_then(|parent| read_task(&layout::task_file(parent)).ok())
        .map(|parent| parent.goal);
    Ok(SessionContext {
        goal: file.goal,
        task_toml,
        task_dir: task_dir.to_path_buf(),
        worktree: file.worktree.map(PathBuf::from),
        parent_goal,
        artifacts: shared_artifacts(task_dir),
    })
}
