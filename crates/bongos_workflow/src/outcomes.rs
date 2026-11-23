//! Run outcomes (feature 20, D16): what a run leaves on its task besides the todo it completes.
//!
//! Outcomes are collected at run level from the fields marked `outcome` in the results of every
//! node and branch:
//! - `pr` (a URL): the task's `[result]` through 03's `set_result`, kind `pr`, state `open`, with
//!   `title`, `branch` and `draft` kept as extra keys. A `babysit` leaf adds it to its set of PRs
//!   rather than replacing anything (`set_result` does that).
//! - `document` (a path in the node's worktree): copied into the task directory under a free name,
//!   never overwriting a file, and recorded shared in `artifacts.toml` with the description
//!   "from run <short>, node <name>".
//! - `tasks` (a path to a proposal-format file): copied with `create_new` to
//!   `<task dir>/proposals/<n>.proposal.txt` with its header set to that number, the task and
//!   `by workflow <name>`, so the task builder lists it pending, needing Kyle, and never applies it
//!   by itself. No task is created here.
//!
//! The file work is plain Rust here; the `task.toml` and `artifacts.toml` writes are
//! [`Write`] changes the engine hands to the work store.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use bongos_work_model::artifacts::{artifact_key, read_task_artifacts};
use bongos_work_model::commands::{self, Change, Context, Outcome as CommandOutcome, Write};
use bongos_work_model::files::write_atomic;
use bongos_work_model::io::{read_artifacts, serialize_artifacts};
use bongos_work_model::layout::{self, PROPOSALS_DIR, proposal_file};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{
    Artifact, ArtifactsFile, Extra, HistoryEntry, ResultKind, ResultState, TaskResult, Visibility,
};
use bongos_workflow_model::definition::Outcome;
use bongos_workflow_model::{
    Definition, Failure, FailureReason, Limits, NodeKey, RunState, scheduler,
};
use serde_json::Value;

use crate::lifecycle::PROJECT_WORK_DIR;
use crate::worktrees::{RunWorktree, branch_worktree_path, short_id};

/// The extra `[result]` keys a `pr` outcome sets, from fields of the same result.
pub const PR_EXTRA_FIELDS: [&str; 3] = ["title", "branch", "draft"];

/// One outcome a run delivers.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Pr {
        url: String,
        title: Option<String>,
        branch: Option<String>,
        draft: Option<bool>,
    },
    /// Relative to the node's worktree.
    Document { path: String },
    /// Relative to the node's worktree: a proposal-format file.
    Tasks { path: String },
}

impl RunOutcome {
    pub fn kind(&self) -> Outcome {
        match self {
            Self::Pr { .. } => Outcome::Pr,
            Self::Document { .. } => Outcome::Document,
            Self::Tasks { .. } => Outcome::Tasks,
        }
    }
}

/// An outcome and the key whose result gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reported {
    pub key: NodeKey,
    pub outcome: RunOutcome,
}

/// What delivering one outcome left on the task, for the run's record.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivered {
    Pr {
        url: String,
    },
    /// The file name in the task directory (its artifact key).
    Document {
        name: String,
    },
    Proposal {
        path: PathBuf,
    },
}

/// The outcomes in the results of `state`: the last successful try of each key (with the edit of
/// a gate approved on it), in log order, every non-empty field marked `outcome`.
pub fn collect(definition: &Definition, state: &RunState) -> Vec<Reported> {
    let mut keys: Vec<&NodeKey> = Vec::new();
    for finished in &state.finished {
        if !keys.contains(&&finished.key) {
            keys.push(&finished.key);
        }
    }
    let mut found = Vec::new();
    for key in keys {
        let Some(last) = state
            .finished
            .iter()
            .rev()
            .find(|finished| &finished.key == key)
        else {
            continue;
        };
        let (Some(Value::Object(result)), None) = (&last.result, &last.failure) else {
            continue;
        };
        let mut result = result.clone();
        if let Some(Value::Object(edit)) = state
            .closed_gates
            .iter()
            .rev()
            .find(|gate| &gate.key == key)
            .and_then(|gate| gate.edit.clone())
        {
            result.extend(edit);
        }
        let Some(node) = definition.node(&key.node) else {
            continue;
        };
        for field in &node.output {
            let Some(kind) = field.outcome else {
                continue;
            };
            for value in texts(result.get(&field.name)) {
                let outcome = match kind {
                    Outcome::Pr => RunOutcome::Pr {
                        url: value,
                        title: text(result.get("title")),
                        branch: text(result.get("branch")),
                        draft: flag(result.get("draft")),
                    },
                    Outcome::Document => RunOutcome::Document { path: value },
                    Outcome::Tasks => RunOutcome::Tasks { path: value },
                };
                found.push(Reported {
                    key: key.clone(),
                    outcome,
                });
            }
        }
    }
    found
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn texts(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(|item| text(Some(item))).collect(),
        other => text(other).into_iter().collect(),
    }
}

fn flag(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.as_str() {
            "true" | "yes" | "draft" => Some(true),
            "false" | "no" | "ready" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The outcomes promised by the success ends the run reached.
pub fn promised(definition: &Definition, state: &RunState, limits: &Limits) -> Vec<Outcome> {
    let mut promised = Vec::new();
    for end in scheduler::plan(definition, state, limits).ends {
        for outcome in end.promised {
            if !promised.contains(&outcome) {
                promised.push(outcome);
            }
        }
    }
    promised
}

/// The task's `[result]` for a `pr` outcome: kind `pr`, state `open`, and the extra keys.
pub fn pr_result(
    url: &str,
    title: Option<&str>,
    branch: Option<&str>,
    draft: Option<bool>,
) -> TaskResult {
    let mut extra = Extra::new();
    if let Some(title) = title {
        extra.insert("title".to_string(), toml::Value::String(title.to_string()));
    }
    if let Some(branch) = branch {
        extra.insert(
            "branch".to_string(),
            toml::Value::String(branch.to_string()),
        );
    }
    if let Some(draft) = draft {
        extra.insert("draft".to_string(), toml::Value::Boolean(draft));
    }
    TaskResult {
        kind: ResultKind::Pr,
        url: Some(url.to_string()),
        state: Some(ResultState::Open),
        urls: Vec::new(),
        extra,
    }
}

/// The description of a document a run delivered.
pub fn document_description(run_id: &str, node: &str) -> String {
    format!("from run {}, node {node}", short_id(run_id))
}

/// The file `relative` names under `root` (a worktree), refusing a path that is absolute, climbs
/// out with `..`, or resolves (through a symbolic link) outside `root`.
pub fn source_file(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("'{relative}' is not a path inside the worktree"));
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", root.display()))?;
    let file = root
        .join(path)
        .canonicalize()
        .map_err(|error| format!("{relative}: {error}"))?;
    if !file.starts_with(&root) {
        return Err(format!("'{relative}' leads outside the worktree"));
    }
    if !file.is_file() {
        return Err(format!("'{relative}' is not a file"));
    }
    Ok(file)
}

/// Copies `source` into `task_dir` under its own name, or the first free `<stem>-<n>.<ext>`
/// (`n` from 2), never overwriting a file and never taking a name the task directory reserves.
/// The name it took.
pub fn copy_document(source: &Path, task_dir: &Path) -> Result<String, String> {
    copy_document_except(source, task_dir, &[])
}

/// [`copy_document`], never taking one of `taken` either.
fn copy_document_except(source: &Path, task_dir: &Path, taken: &[&str]) -> Result<String, String> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no file name", source.display()))?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let bytes = fs::read(source).map_err(|error| format!("{}: {error}", source.display()))?;
    for n in 1..=1000u32 {
        let candidate = match (n, extension) {
            (1, _) => name.to_string(),
            (n, Some(extension)) => format!("{stem}-{n}.{extension}"),
            (n, None) => format!("{stem}-{n}"),
        };
        if artifact_key(Path::new(&candidate)).as_deref() != Some(candidate.as_str())
            || taken.contains(&candidate.as_str())
        {
            continue;
        }
        match create_new(&task_dir.join(&candidate), &bytes) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", task_dir.join(&candidate).display())),
        }
    }
    Err(format!("no free name for {name} in {}", task_dir.display()))
}

fn create_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_data());
    if written.is_err() {
        drop(file);
        fs::remove_file(path).ok();
    }
    written
}

/// The task's artifact list as on disk now, with `name` shared and described as `description`.
pub fn shared_manifest(
    task_dir: &Path,
    name: &str,
    description: &str,
) -> Result<ArtifactsFile, String> {
    let mut list = read_task_artifacts(task_dir).map_err(|error| error.to_string())?;
    let artifact = list
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == name)
        .ok_or_else(|| format!("{name} is not an artifact of {}", task_dir.display()))?;
    artifact.visibility = Some(Visibility::Shared);
    artifact.description = description.to_string();
    Ok(list)
}

/// The change writing `manifest` as the `artifacts.toml` of the task with `id` (after its write
/// guard). Like 03's artifact commands it records no history.
pub fn manifest_change(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    manifest: ArtifactsFile,
) -> CommandOutcome {
    let task = tree
        .task_by_id(id)
        .ok_or_else(|| format!("no task {id} is loaded"))?;
    task.check_writable()
        .map_err(|refused| refused.to_string())?;
    let dir = task.dir.clone();
    Ok(Change {
        dir: dir.clone(),
        writes: vec![Write::Artifacts {
            dir,
            file: manifest,
        }],
        entry: HistoryEntry {
            at: ctx.at,
            by: ctx.by.clone(),
            event: "artifact shared".to_string(),
            detail: None,
            extra: Extra::new(),
        },
        moves: Vec::new(),
    })
}

/// The change setting the `pr` outcome as the result of the task with `id`.
pub fn pr_change(tree: &WorkTree, ctx: &Context, id: &str, result: TaskResult) -> CommandOutcome {
    commands::set_result(tree, ctx, id, result)
}

/// The number the next proposal of `task` gets: one more than the highest recorded in its
/// `task.toml` or found in its `proposals/` directory on disk now.
pub fn next_proposal_number(task: &Task) -> u32 {
    let recorded = task
        .file
        .iter()
        .flat_map(|file| file.proposals.iter().map(|record| record.number));
    let on_disk: Vec<u32> = fs::read_dir(task.dir.join(PROPOSALS_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".proposal.txt")?
                .parse::<u32>()
                .ok()
        })
        .collect();
    recorded.chain(on_disk).max().unwrap_or(0) + 1
}

const HEADER_WORDS: [&str; 5] = ["format", "proposal", "for", "by", "why"];

/// `text` with its header naming proposal `number` of task `task_id`, written by the workflow
/// `workflow`: the `proposal`, `for` and `by` lines are replaced, or added after the header's
/// first line when missing. The body is left as it is.
pub fn proposal_text(text: &str, number: u32, task_id: &str, workflow: &str) -> String {
    let mut header = Vec::new();
    let mut body = Vec::new();
    let mut in_header = true;
    for line in text.lines() {
        let content = line.trim();
        let word = content.split_whitespace().next().unwrap_or_default();
        if in_header
            && (content.is_empty() || content.starts_with('#') || HEADER_WORDS.contains(&word))
            && !line.starts_with([' ', '\t'])
        {
            header.push(line.to_string());
        } else {
            in_header = false;
            body.push(line.to_string());
        }
    }
    let wanted = [
        ("proposal", format!("proposal {number}")),
        ("for", format!("for {task_id}")),
        ("by", format!("by workflow {workflow}")),
    ];
    for (word, line) in wanted.iter().rev() {
        let at = header
            .iter()
            .position(|existing| existing.split_whitespace().next() == Some(*word));
        match at {
            Some(at) => header[at] = line.clone(),
            None => {
                let after = header
                    .iter()
                    .position(|existing| {
                        existing
                            .split_whitespace()
                            .next()
                            .is_some_and(|first| first == "format")
                    })
                    .map_or(0, |at| at + 1);
                header.insert(after, line.clone());
            }
        }
    }
    let mut out = header;
    out.extend(body);
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Copies the proposal `source` into `task`'s `proposals/` as the next number with `create_new`
/// (taking the number after it when another writer took that one), rewritten by
/// [`proposal_text`]. Never creates a task and never records the proposal settled, so the task
/// builder shows it pending. The path written.
pub fn copy_proposal(task: &Task, source: &Path, workflow: &str) -> Result<PathBuf, String> {
    let text =
        fs::read_to_string(source).map_err(|error| format!("{}: {error}", source.display()))?;
    let task_id = task
        .id()
        .ok_or_else(|| format!("{} has no id", task.dir.display()))?;
    let mut number = next_proposal_number(task);
    for _ in 0..1000 {
        let path = proposal_file(&task.dir, number);
        let rewritten = proposal_text(&text, number, task_id, workflow);
        match create_new(&path, rewritten.as_bytes()) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err(format!("no free proposal number in {}", task.dir.display()))
}

/// The worktree an outcome path of `key` is relative to: its fan-out branch worktree while that
/// still holds the file, else the run worktree (where the join merged it). A snapshot or a work
/// directory, which has no branch, is the only tree of its run.
pub fn source_root(worktree: &RunWorktree, key: &NodeKey, path: &str) -> PathBuf {
    if let Some((fan, index)) = key.branch.last().filter(|_| !worktree.branch.is_empty()) {
        let branch = branch_worktree_path(&worktree.run_dir, fan, *index);
        if branch.join(path).exists() {
            return branch;
        }
    }
    worktree.path.clone()
}

/// One `document` outcome of a run with no task: the key that reported it, the tree its path is
/// relative to ([`source_root`]) and the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDocument {
    pub key: NodeKey,
    pub root: PathBuf,
    pub path: String,
}

/// The `document` outcomes the success end of a run with no task copies into its project's files
/// folder ([`deliver_project_documents`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDocuments {
    /// The project's files folder, which the documents are copied into.
    pub files_dir: PathBuf,
    pub run_id: String,
    pub documents: Vec<ProjectDocument>,
    /// A success end the run reached promised a document.
    pub promised: bool,
    /// The file recording the delivery in the attempt directory: once it is there, nothing is
    /// copied again.
    pub marker: PathBuf,
}

/// Copies every document of `documents` into the project's files folder under a free name, never
/// overwriting a file, never taking a name the folder reserves or its `work` directory, and
/// records each shared in the folder's `artifacts.toml` ([`record_project_artifact`]), then
/// writes the marker. A path that is absolute, climbs out or leads outside its tree through a
/// link is refused ([`source_file`]). Nothing is done when the marker is there already. The names
/// taken; the failure when a document was promised and none was delivered.
pub fn deliver_project_documents(documents: &ProjectDocuments) -> Result<Vec<String>, Failure> {
    if documents.marker.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    let mut first_failure: Option<(NodeKey, String)> = None;
    for document in &documents.documents {
        match deliver_project_document(&documents.files_dir, &documents.run_id, document) {
            Ok(name) => names.push(name),
            Err(detail) => {
                log::warn!(
                    "workflows: run {} could not deliver the document of {}: {detail}",
                    documents.run_id,
                    document.key
                );
                first_failure.get_or_insert((document.key.clone(), detail));
            }
        }
    }
    if documents.promised && names.is_empty() {
        let (key, detail) = match first_failure {
            Some((key, detail)) => (Some(key), detail),
            None => (None, "no node delivered it".to_string()),
        };
        return Err(Failure::new(
            FailureReason::OutcomePromisedNotDelivered,
            key,
            detail,
        ));
    }
    let record: Vec<String> = names
        .iter()
        .map(|name| format!("{:?}", Delivered::Document { name: name.clone() }))
        .collect();
    let text = serde_json::to_string_pretty(&record).unwrap_or_default();
    if let Err(error) = fs::write(&documents.marker, text) {
        log::warn!(
            "workflows: could not write {}: {error}",
            documents.marker.display()
        );
    }
    Ok(names)
}

fn deliver_project_document(
    files_dir: &Path,
    run_id: &str,
    document: &ProjectDocument,
) -> Result<String, String> {
    let source = source_file(&document.root, &document.path)?;
    let name = copy_document_except(&source, files_dir, &[PROJECT_WORK_DIR])?;
    record_project_artifact(
        files_dir,
        &name,
        &document_description(run_id, &document.key.node),
    )?;
    Ok(name)
}

/// Serializes the `artifacts.toml` updates of [`record_project_artifact`]: two runs ending at once
/// each re-read the file under it, so neither loses the other's entry.
static PROJECT_ARTIFACTS: Mutex<()> = Mutex::new(());

/// Records `name` shared with `description` in the `artifacts.toml` of the project files folder
/// `files_dir`: the file is read again under a lock, the entry added (or an entry for the same
/// path updated), and the file replaced whole through a temporary file and a rename. Every other
/// entry and key is kept; a file that does not parse is an error and is left as it is.
pub fn record_project_artifact(
    files_dir: &Path,
    name: &str,
    description: &str,
) -> Result<(), String> {
    let _guard = PROJECT_ARTIFACTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let path = layout::artifacts_file(files_dir);
    let mut manifest = match read_artifacts(&path) {
        Ok(manifest) => manifest,
        Err(bongos_work_model::Error {
            kind: bongos_work_model::ErrorKind::Io(error),
            ..
        }) if error.kind() == io::ErrorKind::NotFound => ArtifactsFile::default(),
        Err(error) => return Err(error.to_string()),
    };
    match manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == name)
    {
        Some(artifact) => {
            artifact.description = description.to_string();
            artifact.visibility = Some(Visibility::Shared);
        }
        None => manifest.artifacts.push(Artifact {
            path: name.to_string(),
            description: description.to_string(),
            visibility: Some(Visibility::Shared),
            extra: Extra::new(),
        }),
    }
    let text = serialize_artifacts(&manifest).map_err(|error| error.to_string())?;
    write_atomic(&path, text.as_bytes()).map_err(|error| format!("{}: {error}", path.display()))
}
