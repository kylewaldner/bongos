//! `project: restore`: rebuilds a project's live state from one snapshot (the newer of this
//! machine's state branch and the remote's, a snapshot picked by the person, or a state an
//! earlier restore replaced). A snapshot is read whole and checked before anything changes: its
//! `FORMAT` version, its paths, every file it holds. Then, under the landing lock and while no
//! activity runs, the store is copied to `state/replaced/<time>.db` and every file of the
//! snapshot is imported in one transaction that runs no state apply hook, replacing the task
//! state, the runs and every exporter's state. A value the snapshot withheld is never imported:
//! the local value is kept, or the place is left empty. Worktrees and run records the restored
//! state names but this machine lacks are marked; nothing is recreated.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::Status;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use util::rel_path::{RelPath, RelPathBuf};

use crate::backup::{remote_problem, shown_remote};
use crate::create::ProjectRepo;
use crate::registries::StateExporter;
use crate::snapshot::{
    self, FORMAT_FILE, MAX_EXPORTED_FILE_BYTES, RUNS_FILE, RUNS_NAMESPACE, Secrets, TASKS_DIR,
    WITHHELD_FILE, WITHHELD_SECRET, Withheld, is_safe_component, namespace_problem,
};
use crate::state::{ImportReason, StateStore, StateTx};
use crate::state_branch;

/// The folder of `state/` where a restore keeps the state it replaced.
pub const REPLACED_DIR: &str = "replaced";
/// Where the remote's state branch is fetched to: outside `refs/heads/`, so it never moves a
/// branch of this machine, and fetched whatever its history (thinning rewrites it).
pub const REMOTE_STATE_REF: &str = "refs/bongos/remote-state";
/// The mark of an open task whose worktree is not on this machine.
pub const WORKTREE_MISSING: &str = "worktree missing";
/// The mark of a task whose run was active and whose record is not on this machine.
pub const RUN_RECORD_MISSING: &str = "interrupted, record not on this machine";
/// The kind of snapshot `FORMAT` names, and the newest version this build reads.
pub const FORMAT_KIND: &str = "bongos-state";
pub const FORMAT_VERSION: u32 = 1;
/// The holder name of the landing lock while a restore runs.
const RESTORE_HOLDER: &str = "restore";
/// How a replaced state's file is named: its time, then `.db`.
const REPLACED_TIME_FORMAT: &str = "%Y%m%dT%H%M%SZ";
const REPLACED_TIME_LENGTH: usize = 16;
/// The most files a snapshot may hold.
const MAX_SNAPSHOT_FILES: usize = 200_000;
/// The states of a run that ended; any other state is a run that was active.
const FINISHED_RUN_STATES: &[&str] = &[
    "ended",
    "done",
    "finished",
    "succeeded",
    "failed",
    "cancelled",
    "canceled",
    "halted",
    "interrupted",
];
/// The longest piece of a snapshot quoted in a message.
const MAX_QUOTED: usize = 80;

/// What to restore from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum RestoreSource {
    /// The newer of this machine's state branch and the remote's.
    Newest,
    /// A snapshot on this machine's state branch or on the remote's, by its commit.
    Snapshot { commit: String },
    /// A state an earlier restore replaced, by its file name in `state/replaced/`.
    Replaced { name: String },
}

/// Where a snapshot comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    Local,
    Remote,
    Replaced,
}

/// One state a restore can start from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreCandidate {
    pub kind: SourceKind,
    pub source: RestoreSource,
    pub taken_at: Option<DateTime<Utc>>,
}

impl RestoreCandidate {
    /// One line for a picker.
    pub fn label(&self) -> String {
        source_label(self.kind, &self.source, self.taken_at)
    }
}

fn source_label(
    kind: SourceKind,
    source: &RestoreSource,
    taken_at: Option<DateTime<Utc>>,
) -> String {
    let time = taken_at.map_or_else(
        || "at an unknown time".to_string(),
        |at| format!("of {}", at.format("%Y-%m-%d %H:%M UTC")),
    );
    match (kind, source) {
        (SourceKind::Local, RestoreSource::Snapshot { commit }) => {
            format!("snapshot {time} on this machine ({})", short(commit))
        }
        (SourceKind::Remote, RestoreSource::Snapshot { commit }) => {
            format!("snapshot {time} on the remote ({})", short(commit))
        }
        (_, RestoreSource::Replaced { name }) => {
            format!("state replaced by a restore, kept {time} ({name})")
        }
        (_, source) => format!("{source:?}"),
    }
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// The states a restore can start from, newest first, and which one it takes by default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreCandidates {
    pub candidates: Vec<RestoreCandidate>,
    /// The newer of the local and the remote snapshot.
    pub newest: Option<RestoreSource>,
    /// Why the remote's snapshots are not listed, when they are not.
    pub notes: Vec<String>,
}

/// A worktree the restored state names that this machine lacks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingWorktree {
    pub task: String,
    pub path: String,
}

/// A run whose record this machine lacks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingRunRecord {
    pub run: String,
    pub task: String,
    /// The run was active when the snapshot was taken: it shows as interrupted.
    pub active: bool,
}

/// A value the snapshot withheld, and what the restore does instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithheldNote {
    pub file: String,
    pub location: String,
    /// The value of this machine is kept; otherwise the place is left empty.
    pub kept_local: bool,
}

/// The state of one feature in the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceChange {
    pub namespace: String,
    pub files: usize,
    /// The snapshot's files differ from this machine's.
    pub differs: bool,
    /// A feature of this build restores it; otherwise it is left out.
    pub restored: bool,
}

/// What a restore will do, shown before it is confirmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePreview {
    /// The source resolved: confirming restores exactly this.
    pub source: RestoreSource,
    pub kind: SourceKind,
    pub taken_at: Option<DateTime<Utc>>,
    /// The tasks with a state in the snapshot.
    pub tasks: usize,
    /// The tasks whose live state the restore changes.
    pub tasks_differing: Vec<String>,
    pub runs: usize,
    pub namespaces: Vec<NamespaceChange>,
    pub missing_worktrees: Vec<MissingWorktree>,
    pub missing_run_records: Vec<MissingRunRecord>,
    pub withheld: Vec<WithheldNote>,
    pub notes: Vec<String>,
    /// Work running in the project: while there is any, the restore refuses.
    pub activity: Vec<String>,
    /// How long the replaced state is kept.
    pub keep_days: u32,
}

impl RestorePreview {
    /// The text of the preview buffer.
    pub fn render(&self) -> String {
        let mut lines = vec!["Restore the project's state".to_string(), String::new()];
        lines.push(format!(
            "From: the {}",
            source_label(self.kind, &self.source, self.taken_at)
        ));
        let mut parts = vec![
            format!("{} tasks", self.tasks),
            format!("{} runs", self.runs),
        ];
        parts.extend(
            self.namespaces
                .iter()
                .filter(|namespace| namespace.restored)
                .map(|namespace| namespace.namespace.clone()),
        );
        lines.push(format!(
            "Rebuilds the live state of this machine from it: {}.",
            parts.join(", ")
        ));
        lines.push(format!(
            "{} worktrees and {} run records are not on this machine.",
            self.missing_worktrees.len(),
            self.missing_run_records.len()
        ));
        lines.push(format!(
            "The state it replaces is kept for {} days in state/{REPLACED_DIR}/ and can itself be restored.",
            self.keep_days
        ));
        lines.push(String::new());
        if self.tasks_differing.is_empty() {
            lines.push("Tasks whose state changes: none".to_string());
        } else {
            lines.push(format!(
                "Tasks whose state changes ({}):",
                self.tasks_differing.len()
            ));
            lines.extend(self.tasks_differing.iter().map(|task| format!("  {task}")));
        }
        for namespace in &self.namespaces {
            lines.push(match (namespace.restored, namespace.differs) {
                (false, _) => format!(
                    "{}: not restored, no feature of this bongos reads it",
                    namespace.namespace
                ),
                (true, true) => format!(
                    "{}: replaced ({} files)",
                    namespace.namespace, namespace.files
                ),
                (true, false) => format!("{}: unchanged", namespace.namespace),
            });
        }
        if !self.missing_worktrees.is_empty() {
            lines.push(String::new());
            lines
                .push("Worktrees not on this machine (shown as \"worktree missing\"):".to_string());
            lines.extend(
                self.missing_worktrees
                    .iter()
                    .map(|missing| format!("  {}: {}", missing.task, missing.path)),
            );
        }
        if !self.missing_run_records.is_empty() {
            lines.push(String::new());
            lines.push("Run records not on this machine:".to_string());
            lines.extend(self.missing_run_records.iter().map(|missing| {
                if missing.active {
                    format!("  {} ({}): {RUN_RECORD_MISSING}", missing.run, missing.task)
                } else {
                    format!("  {} ({})", missing.run, missing.task)
                }
            }));
        }
        if !self.withheld.is_empty() {
            lines.push(String::new());
            lines.push("Values the snapshot withheld (never restored as written):".to_string());
            lines.extend(self.withheld.iter().map(|note| {
                format!(
                    "  {} {}: {}",
                    note.file,
                    note.location,
                    if note.kept_local {
                        "the value of this machine is kept"
                    } else {
                        "left empty"
                    }
                )
            }));
        }
        if !self.notes.is_empty() {
            lines.push(String::new());
            lines.extend(self.notes.iter().map(|note| format!("Note: {note}")));
        }
        if !self.activity.is_empty() {
            lines.push(String::new());
            lines.push(
                "Work is running in this project; the restore waits until it stops:".to_string(),
            );
            lines.extend(self.activity.iter().map(|line| format!("  {line}")));
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

/// How a restore ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum RestoreOutcome {
    /// The state is the snapshot's; the replaced one is `state/replaced/<replaced>`.
    Restored {
        preview: Box<RestorePreview>,
        replaced: String,
    },
    /// Work runs in the project; nothing changed.
    Refused { activity: Vec<String> },
}

/// A snapshot read and checked, ready to import.
struct Plan {
    preview: RestorePreview,
    tasks: BTreeMap<String, TaskStateRecord>,
    runs: Vec<(String, toml::Table)>,
    namespaces: BTreeMap<String, Vec<(RelPathBuf, String)>>,
    /// The local tasks no snapshot can hold (their ids are not file names or are withheld):
    /// kept as they are.
    keep: BTreeSet<String>,
}

/// The files of a source, read whole.
struct Resolved {
    source: RestoreSource,
    kind: SourceKind,
    taken_at: Option<DateTime<Utc>>,
    files: BTreeMap<String, String>,
}

/// The time in the name of a replaced state, `None` when the name is not one.
pub(crate) fn replaced_time(name: &str) -> Option<DateTime<Utc>> {
    let stem = name.strip_suffix(".db")?;
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '.')
    {
        return None;
    }
    let time = stem.get(..REPLACED_TIME_LENGTH)?;
    let rest = &stem[REPLACED_TIME_LENGTH..];
    if !rest.is_empty()
        && !rest
            .strip_prefix('-')
            .is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    NaiveDateTime::parse_from_str(time, REPLACED_TIME_FORMAT)
        .ok()
        .map(|time| time.and_utc())
}

/// `text` cut to a length fit for a message, without control characters.
fn quoted(text: &str) -> String {
    let mut shown: String = text.chars().take(MAX_QUOTED).collect();
    if shown.len() < text.len() {
        shown.push_str("...");
    }
    shown.escape_debug().to_string()
}

/// Checks the `FORMAT` of a snapshot: its kind, and a version this build reads.
pub(crate) fn check_format(files: &BTreeMap<String, String>) -> Result<()> {
    let text = files
        .get(FORMAT_FILE)
        .context("this is not a state snapshot: it has no FORMAT file")?;
    let line = text.lines().next().unwrap_or_default().trim();
    let Some((kind, version)) = line.split_once(' ') else {
        bail!(
            "this is not a state snapshot: FORMAT reads \"{}\"",
            quoted(line)
        );
    };
    if kind != FORMAT_KIND {
        bail!(
            "this is not a state snapshot: FORMAT reads \"{}\"",
            quoted(line)
        );
    }
    let Ok(version) = version.trim().parse::<u32>() else {
        bail!(
            "the snapshot's FORMAT version \"{}\" is not a number",
            quoted(version)
        );
    };
    if version > FORMAT_VERSION {
        bail!(
            "the snapshot is format version {version}, newer than this bongos reads \
             (version {FORMAT_VERSION}); update bongos to restore it"
        );
    }
    if version == 0 {
        bail!("the snapshot's FORMAT version 0 is not one bongos wrote");
    }
    Ok(())
}

/// Why `path`, a file of a snapshot, is refused, if it is.
pub(crate) fn snapshot_path_problem(path: &str) -> Option<&'static str> {
    if path.starts_with('/') || path.starts_with('\\') {
        return Some("is absolute");
    }
    let bad = path
        .split('/')
        .find(|component| !is_safe_component(component))?;
    Some(if bad == ".." {
        "climbs out with `..`"
    } else if bad.to_ascii_lowercase().starts_with(".git") {
        "names a git file"
    } else {
        "has an empty or invalid component"
    })
}

/// Whether `stem`, a task file's name without `.toml`, is one an export gives a task whose id
/// it could not write: `withheld-<n>` or `unnamed-<n>`.
fn is_placeholder_name(stem: &str) -> bool {
    ["withheld-", "unnamed-"].iter().any(|prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
    })
}

/// Whether a task with `id` can be written into a snapshot under its own name.
fn has_snapshot_name(id: &str, secrets: Option<&Secrets>) -> bool {
    is_safe_component(id)
        && !id.ends_with(".toml")
        && secrets.is_none_or(|secrets| secrets.redact(id).is_none())
}

fn holds_withheld(text: &str) -> bool {
    text.contains(WITHHELD_SECRET)
}

/// Replaces each withheld value in `value` with the value at the same place in `local`, or
/// leaves the place empty (a key removed, an array item an empty string) when this machine has
/// none. Notes each.
fn resolve_withheld(
    value: &mut toml::Value,
    local: Option<&toml::Value>,
    file: &str,
    path: &mut Vec<String>,
    notes: &mut Vec<WithheldNote>,
) {
    let location = |path: &[String]| {
        if path.is_empty() {
            "(whole file)".to_string()
        } else {
            path.join(".")
        }
    };
    if matches!(value, toml::Value::String(text) if holds_withheld(text)) {
        let kept_local = match local {
            Some(local) => {
                *value = local.clone();
                true
            }
            None => {
                *value = toml::Value::String(String::new());
                false
            }
        };
        notes.push(WithheldNote {
            file: file.to_string(),
            location: location(path),
            kept_local,
        });
        return;
    }
    match value {
        toml::Value::Table(table) => {
            let keys: Vec<String> = table.keys().cloned().collect();
            for key in keys {
                path.push(key.clone());
                if holds_withheld(&key) {
                    // A key that was itself withheld: its real name is unknown here.
                    table.remove(&key);
                    notes.push(WithheldNote {
                        file: file.to_string(),
                        location: format!("{} (key)", location(path)),
                        kept_local: false,
                    });
                    path.pop();
                    continue;
                }
                let local_item = local
                    .and_then(toml::Value::as_table)
                    .and_then(|local| local.get(&key));
                let withheld = matches!(table.get(&key), Some(toml::Value::String(text)) if holds_withheld(text));
                if withheld {
                    let kept_local = match local_item {
                        Some(local_item) => {
                            table.insert(key.clone(), local_item.clone());
                            true
                        }
                        None => {
                            table.remove(&key);
                            false
                        }
                    };
                    notes.push(WithheldNote {
                        file: file.to_string(),
                        location: location(path),
                        kept_local,
                    });
                } else if let Some(item) = table.get_mut(&key) {
                    resolve_withheld(item, local_item, file, path, notes);
                }
                path.pop();
            }
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                path.push(format!("[{index}]"));
                let local_item = local
                    .and_then(toml::Value::as_array)
                    .and_then(|local| local.get(index));
                if matches!(item, toml::Value::String(text) if holds_withheld(text)) {
                    let kept_local = match local_item {
                        Some(local_item) => {
                            *item = local_item.clone();
                            true
                        }
                        None => {
                            *item = toml::Value::String(String::new());
                            false
                        }
                    };
                    notes.push(WithheldNote {
                        file: file.to_string(),
                        location: location(path),
                        kept_local,
                    });
                } else {
                    resolve_withheld(item, local_item, file, path, notes);
                }
                path.pop();
            }
        }
        _ => {}
    }
}

/// A TOML file of the snapshot with its withheld values resolved against `local`.
fn resolve_withheld_toml(
    text: &str,
    local: Option<&str>,
    file: &str,
    notes: &mut Vec<WithheldNote>,
) -> Result<toml::Value> {
    let table: toml::Table = toml::from_str(text).with_context(|| {
        format!("the snapshot's {file} is not valid TOML; it was cut short or edited")
    })?;
    let mut value = toml::Value::Table(table);
    if holds_withheld(text) {
        let local = local
            .and_then(|text| text.parse::<toml::Table>().ok())
            .map(toml::Value::Table);
        resolve_withheld(&mut value, local.as_ref(), file, &mut Vec::new(), notes);
    }
    Ok(value)
}

/// The runs of `runs.toml` by id, each without its `id` key.
fn parse_runs(value: toml::Value, file: &str) -> Result<Vec<(String, toml::Table)>> {
    let toml::Value::Table(mut table) = value else {
        bail!("the snapshot's {file} is not a table");
    };
    let runs = match table.remove("run") {
        None => Vec::new(),
        Some(toml::Value::Array(runs)) => runs,
        Some(_) => bail!("the snapshot's {file} has a `run` that is not a list"),
    };
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(runs.len());
    for run in runs {
        let toml::Value::Table(mut run) = run else {
            bail!("the snapshot's {file} has a run that is not a table");
        };
        let Some(toml::Value::String(id)) = run.remove("id") else {
            bail!("the snapshot's {file} has a run without an id");
        };
        if !seen.insert(id.clone()) {
            bail!("the snapshot's {file} has the run {} twice", quoted(&id));
        }
        parsed.push((id, run));
    }
    Ok(parsed)
}

/// Reads and checks every file of a snapshot against this machine's state (`local`, exported
/// without withholding). Fails, naming the file, on anything a snapshot bongos wrote would not
/// hold, so a cut-short or edited snapshot changes nothing.
fn read_plan(
    resolved: Resolved,
    local: &BTreeMap<String, String>,
    local_tasks: &BTreeMap<String, TaskStateRecord>,
    exporters: &[(String, Arc<dyn StateExporter>)],
    secrets: Option<&Secrets>,
    keep_days: u32,
) -> Result<Plan> {
    let files = resolved.files;
    if files.len() > MAX_SNAPSHOT_FILES {
        bail!(
            "the snapshot holds {} files, more than a snapshot can",
            files.len()
        );
    }
    check_format(&files)?;
    for (path, text) in &files {
        if let Some(problem) = snapshot_path_problem(path) {
            bail!(
                "the snapshot's file \"{}\" {problem}; nothing was restored",
                quoted(path)
            );
        }
        if text.len() > MAX_EXPORTED_FILE_BYTES {
            bail!(
                "the snapshot's file {path} is larger than {MAX_EXPORTED_FILE_BYTES} bytes; nothing was restored"
            );
        }
    }
    let withheld_list: Vec<Withheld> = match files.get(WITHHELD_FILE) {
        Some(text) => {
            #[derive(Deserialize)]
            struct WithheldFile {
                #[serde(default)]
                withheld: Vec<Withheld>,
            }
            toml::from_str::<WithheldFile>(text)
                .with_context(|| {
                    format!(
                        "the snapshot's {WITHHELD_FILE} is not valid; it was cut short or edited"
                    )
                })?
                .withheld
        }
        None => Vec::new(),
    };
    let withheld_names: BTreeSet<&str> = withheld_list
        .iter()
        .filter(|withheld| withheld.location == "task id" || withheld.location == "file name")
        .map(|withheld| withheld.file.as_str())
        .collect();

    let mut notes = Vec::new();
    let mut withheld = Vec::new();
    let mut tasks = BTreeMap::new();
    let mut runs = Vec::new();
    let mut skipped_tasks = 0;
    let mut by_namespace: BTreeMap<String, Vec<(RelPathBuf, String)>> = BTreeMap::new();
    for (path, text) in &files {
        let mut components = path.split('/');
        let top = components.next().unwrap_or_default();
        match top {
            FORMAT_FILE | WITHHELD_FILE if !path.contains('/') => {}
            RUNS_FILE if !path.contains('/') => {
                let value = toml::Value::Table(toml::from_str::<toml::Table>(text).with_context(
                    || format!("the snapshot's {RUNS_FILE} is not valid TOML; it was cut short or edited"),
                )?);
                // Withheld run values are resolved run by run, against the local run of the
                // same id.
                let local_runs: BTreeMap<String, toml::Table> = local
                    .get(RUNS_FILE)
                    .and_then(|text| text.parse::<toml::Table>().ok())
                    .map(toml::Value::Table)
                    .and_then(|value| parse_runs(value, RUNS_FILE).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                for (id, run) in parse_runs(value, RUNS_FILE)? {
                    if holds_withheld(&id) {
                        withheld.push(WithheldNote {
                            file: RUNS_FILE.to_string(),
                            location: "run id".to_string(),
                            kept_local: false,
                        });
                        continue;
                    }
                    let mut run = toml::Value::Table(run);
                    if toml::to_string(&run).is_ok_and(|text| holds_withheld(&text)) {
                        let local_run = local_runs.get(&id).cloned().map(toml::Value::Table);
                        resolve_withheld(
                            &mut run,
                            local_run.as_ref(),
                            RUNS_FILE,
                            &mut vec![format!("run {id}")],
                            &mut withheld,
                        );
                    }
                    let toml::Value::Table(run) = run else {
                        continue;
                    };
                    runs.push((id, run));
                }
            }
            TASKS_DIR => {
                let (Some(name), None) = (components.next(), components.next()) else {
                    bail!(
                        "the snapshot's {TASKS_DIR}/ holds \"{}\", which is not a task file; nothing was restored",
                        quoted(path)
                    );
                };
                let Some(id) = name.strip_suffix(".toml") else {
                    bail!(
                        "the snapshot's {TASKS_DIR}/ holds \"{}\", which is not a task file; nothing was restored",
                        quoted(path)
                    );
                };
                if withheld_names.contains(path.as_str()) || is_placeholder_name(id) {
                    skipped_tasks += 1;
                    continue;
                }
                let local_text = local.get(path).map(String::as_str);
                let mut task_notes = Vec::new();
                let value = match resolve_withheld_toml(text, local_text, path, &mut task_notes) {
                    Ok(value) => value,
                    // A withheld match across the TOML's structure: the local state of the
                    // task is kept whole, or the task is left out.
                    Err(_) if holds_withheld(text) => match local_tasks.get(id) {
                        Some(record) => {
                            withheld.push(WithheldNote {
                                file: path.clone(),
                                location: "(whole file)".to_string(),
                                kept_local: true,
                            });
                            tasks.insert(id.to_string(), record.clone());
                            continue;
                        }
                        None => {
                            withheld.push(WithheldNote {
                                file: path.clone(),
                                location: "(whole file)".to_string(),
                                kept_local: false,
                            });
                            continue;
                        }
                    },
                    Err(error) => return Err(error),
                };
                withheld.extend(task_notes);
                let record: TaskStateRecord = value.try_into().with_context(|| {
                    format!("the snapshot's {path} is not a task state; it was cut short or edited")
                })?;
                tasks.insert(id.to_string(), record);
            }
            namespace => {
                if withheld_names.contains(path.as_str())
                    || path.split('/').any(|component| {
                        component.starts_with("withheld-") && is_placeholder_name(component)
                    })
                {
                    notes.push(format!(
                        "the file {path} had its name withheld and is not restored"
                    ));
                    continue;
                }
                let relative = RelPath::unix(path)
                    .map_err(|error| anyhow!("the snapshot's file {path}: {error}"))?
                    .to_rel_path_buf();
                let text = if holds_withheld(text) {
                    resolve_withheld_text(path, text, local.get(path), &mut withheld)?
                } else {
                    text.clone()
                };
                by_namespace
                    .entry(namespace.to_string())
                    .or_default()
                    .push((relative, text));
            }
        }
    }
    if skipped_tasks > 0 {
        notes.push(format!(
            "{skipped_tasks} task states had their ids withheld or unwritable in the snapshot and are not restored"
        ));
    }

    let keep: BTreeSet<String> = local_tasks
        .keys()
        .filter(|id| !has_snapshot_name(id, secrets))
        .cloned()
        .collect();
    if !keep.is_empty() {
        notes.push(format!(
            "{} tasks of this machine have ids no snapshot can hold; their state is kept",
            keep.len()
        ));
    }

    let exporter_names: BTreeSet<&str> = exporters.iter().map(|(name, _)| name.as_str()).collect();
    let mut namespaces = Vec::new();
    for (name, _) in exporters {
        let theirs: BTreeMap<String, &String> = by_namespace
            .get(name)
            .map(|files| {
                files
                    .iter()
                    .map(|(path, text)| (path.as_unix_str().to_string(), text))
                    .collect()
            })
            .unwrap_or_default();
        let prefix = format!("{name}/");
        let ours: BTreeMap<String, &String> = local
            .iter()
            .filter(|(path, _)| *path == name || path.starts_with(&prefix))
            .map(|(path, text)| (path.clone(), text))
            .collect();
        namespaces.push(NamespaceChange {
            namespace: name.clone(),
            files: theirs.len(),
            differs: theirs != ours,
            restored: true,
        });
    }
    for (name, files) in &by_namespace {
        if !exporter_names.contains(name.as_str()) {
            namespaces.push(NamespaceChange {
                namespace: name.clone(),
                files: files.len(),
                differs: true,
                restored: false,
            });
        }
    }

    let mut differing: BTreeSet<String> = tasks
        .iter()
        .filter(|(id, record)| local_tasks.get(*id) != Some(*record))
        .map(|(id, _)| id.clone())
        .collect();
    differing.extend(
        local_tasks
            .keys()
            .filter(|id| !tasks.contains_key(*id) && !keep.contains(*id))
            .cloned(),
    );

    let preview = RestorePreview {
        source: resolved.source,
        kind: resolved.kind,
        taken_at: resolved.taken_at,
        tasks: tasks.len(),
        tasks_differing: differing.into_iter().collect(),
        runs: runs.len(),
        namespaces,
        missing_worktrees: Vec::new(),
        missing_run_records: Vec::new(),
        withheld,
        notes,
        activity: Vec::new(),
        keep_days,
    };
    Ok(Plan {
        preview,
        tasks,
        runs,
        namespaces: by_namespace,
        keep,
    })
}

/// An exporter's text file with its withheld values resolved: this machine's file when it has
/// one, else the text with each withheld value left empty.
fn resolve_withheld_text(
    path: &str,
    text: &str,
    local: Option<&String>,
    notes: &mut Vec<WithheldNote>,
) -> Result<String> {
    if path.ends_with(".toml")
        && let Ok(value) = resolve_withheld_toml(text, local.map(String::as_str), path, notes)
    {
        return toml::to_string(&value).with_context(|| format!("writing {path}"));
    }
    Ok(match local {
        Some(local) => {
            notes.push(WithheldNote {
                file: path.to_string(),
                location: "(text)".to_string(),
                kept_local: true,
            });
            local.clone()
        }
        None => {
            notes.push(WithheldNote {
                file: path.to_string(),
                location: "(text)".to_string(),
                kept_local: false,
            });
            text.replace(WITHHELD_SECRET, "")
        }
    })
}

/// Whether the record of run `run` of task `task` is under `files`: `<task>/runs/<run>` or a
/// file named `<run>.<extension>` there. A name that is not a plain file name is never looked
/// up, and counts as missing.
fn run_record_exists(files: &Path, task: &str, run: &str) -> bool {
    if !is_safe_component(task) || !is_safe_component(run) {
        return false;
    }
    let runs = files.join(task).join("runs");
    if runs.join(run).exists() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(&runs) else {
        return false;
    };
    let prefix = format!("{run}.");
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
}

fn worktree_missing(record: &TaskStateRecord) -> Option<&str> {
    let path = record.worktree.as_deref()?;
    if record.status != Status::Open {
        return None;
    }
    let exists = Path::new(path).is_absolute() && Path::new(path).exists();
    (!exists).then_some(path)
}

fn run_field<'a>(run: &'a toml::Table, key: &str) -> Option<&'a str> {
    run.get(key).and_then(toml::Value::as_str)
}

fn run_is_active(run: &toml::Table) -> bool {
    run_field(run, "state").is_none_or(|state| !FINISHED_RUN_STATES.contains(&state))
}

/// The worktrees and run records `tasks` and `runs` name that are not under `files` or on this
/// machine.
fn missing_on_this_machine(
    tasks: &BTreeMap<String, TaskStateRecord>,
    runs: &[(String, toml::Table)],
    files: &Path,
) -> (Vec<MissingWorktree>, Vec<MissingRunRecord>) {
    let worktrees = tasks
        .iter()
        .filter_map(|(task, record)| {
            worktree_missing(record).map(|path| MissingWorktree {
                task: task.clone(),
                path: path.to_string(),
            })
        })
        .collect();
    let records = runs
        .iter()
        .filter_map(|(run, table)| {
            let task = run_field(table, "task").unwrap_or_default();
            (!run_record_exists(files, task, run)).then(|| MissingRunRecord {
                run: run.clone(),
                task: task.to_string(),
                active: run_is_active(table),
            })
        })
        .collect();
    (worktrees, records)
}

/// Writes the plan in the import's transaction: the task state, the runs, then each
/// exporter's state from its files (none when the snapshot has none).
fn apply_plan(
    tx: &mut StateTx,
    plan: &Plan,
    exporters: &[(String, Arc<dyn StateExporter>)],
) -> Result<()> {
    tx.clear_task_state(|id| plan.keep.contains(id))?;
    for (id, record) in &plan.tasks {
        tx.set_task(id, record.clone())?;
    }
    {
        let mut runs = tx.kv(RUNS_NAMESPACE);
        runs.clear()?;
        for (id, run) in &plan.runs {
            runs.put(id, toml::Value::Table(run.clone()))?;
        }
    }
    let none = Vec::new();
    for (name, exporter) in exporters {
        let files = plan.namespaces.get(name).unwrap_or(&none);
        match std::panic::catch_unwind(AssertUnwindSafe(|| exporter.import(tx, files))) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => bail!("the {name} state could not be restored: {error:#}"),
            Err(_) => bail!("the {name} state could not be restored: its importer panicked"),
        }
    }
    Ok(())
}

impl ProjectRepo {
    /// The work running in the project, as the activity sources report it.
    pub fn activity(&self) -> Vec<String> {
        let project = self.location.id.clone();
        self.activity_sources()
            .iter()
            .flat_map(|source| {
                match std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let name = source.name().to_string();
                    let lines = source.activity(&project);
                    (name, lines)
                })) {
                    Ok((name, lines)) => lines
                        .into_iter()
                        .map(|line| format!("{name}: {line}"))
                        .collect::<Vec<_>>(),
                    Err(_) => vec!["an activity source panicked; assuming work runs".to_string()],
                }
            })
            .collect()
    }

    /// Asks every activity source to stop the project's work at its next job boundary. Returns
    /// what could not be asked.
    pub fn stop_at_job_boundary(&self) -> Vec<String> {
        let project = self.location.id.clone();
        self.activity_sources()
            .iter()
            .filter_map(|source| {
                match std::panic::catch_unwind(AssertUnwindSafe(|| {
                    source.stop_at_job_boundary(&project)
                })) {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("an activity source panicked when asked to stop".to_string()),
                }
            })
            .collect()
    }

    fn replaced_dir(&self) -> PathBuf {
        self.location.state.join(REPLACED_DIR)
    }

    /// The states earlier restores replaced, newest first, with their times.
    pub fn replaced_states(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        let dir = self.replaced_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(anyhow!(error).context(format!("reading {}", dir.display()))),
        };
        let mut states: Vec<(String, DateTime<Utc>)> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                let time = replaced_time(&name)?;
                Some((name, time))
            })
            .collect();
        states.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        Ok(states)
    }

    /// Removes the replaced states older than `replaced_keep_days` at `now`. Returns how many.
    pub fn prune_replaced(&self, now: DateTime<Utc>) -> Result<usize> {
        let keep_days = self.backup_options().replaced_keep_days.max(1);
        let cutoff = now - chrono::Duration::days(i64::from(keep_days));
        let mut removed = 0;
        for (name, time) in self.replaced_states()? {
            if time < cutoff {
                let path = self.replaced_dir().join(&name);
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// A path in `state/replaced/` for a state replaced at `now` that no file has yet.
    fn fresh_replaced_path(&self, now: DateTime<Utc>) -> Result<(String, PathBuf)> {
        let dir = self.replaced_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let time = now.format(REPLACED_TIME_FORMAT).to_string();
        for count in 1..1000 {
            let name = if count == 1 {
                format!("{time}.db")
            } else {
                format!("{time}-{count}.db")
            };
            let path = dir.join(&name);
            if std::fs::symlink_metadata(&path).is_err() {
                return Ok((name, path));
            }
        }
        bail!("too many replaced states at {time}")
    }

    /// Fetches the remote's state branch to [`REMOTE_STATE_REF`], whatever its history. `None`
    /// without a remote.
    async fn fetch_remote_state(&self) -> Result<Option<String>> {
        let Some(remote) = self.location.remote.clone() else {
            return Ok(None);
        };
        if let Some(problem) = remote_problem(&remote) {
            bail!("the remote {problem}");
        }
        let options = self.backup_options();
        state_branch::state_ref(&options.state_branch)?;
        let refspec = format!("+refs/heads/{}:{REMOTE_STATE_REF}", options.state_branch);
        let fetched = self
            .git
            .fetch_refspecs(
                &self.location.repository,
                &remote,
                &[refspec],
                self.network_timeout(),
            )
            .await;
        if let Err(error) = fetched {
            let message = error.to_string().replace(&remote, &shown_remote(&remote));
            let message = match Secrets::new(&options.secret_patterns, &options.secret_keys) {
                Ok(secrets) => secrets.clean(&message),
                Err(_) => "the fetch failed".to_string(),
            };
            bail!("{message}");
        }
        Ok(state_branch::tip(&self.git, &self.location.repository, REMOTE_STATE_REF).await)
    }

    async fn snapshot_time_of(&self, commit: &str) -> Option<DateTime<Utc>> {
        let message = self
            .git
            .message_of(&self.location.repository, commit)
            .await
            .ok()?;
        state_branch::snapshot_time(&message)
    }

    /// The states a restore can start from: this machine's snapshot, the remote's (fetched
    /// now), and the states earlier restores replaced; and the newer snapshot of the first two.
    pub async fn restore_candidates(&self) -> Result<RestoreCandidates> {
        let options = self.backup_options();
        let reference = state_branch::state_ref(&options.state_branch)?;
        let repository = &self.location.repository;
        let mut candidates = RestoreCandidates::default();
        let local = match state_branch::tip(&self.git, repository, &reference).await {
            Some(commit) => Some(RestoreCandidate {
                kind: SourceKind::Local,
                taken_at: self.snapshot_time_of(&commit).await,
                source: RestoreSource::Snapshot { commit },
            }),
            None => None,
        };
        let remote = match self.fetch_remote_state().await {
            Ok(Some(commit)) => Some(RestoreCandidate {
                kind: SourceKind::Remote,
                taken_at: self.snapshot_time_of(&commit).await,
                source: RestoreSource::Snapshot { commit },
            }),
            Ok(None) => None,
            Err(error) => {
                candidates.notes.push(format!(
                    "the remote's snapshots could not be fetched: {error:#}"
                ));
                None
            }
        };
        let remote = remote.filter(|remote| {
            local
                .as_ref()
                .is_none_or(|local| local.source != remote.source)
        });
        candidates.newest = match (&local, &remote) {
            (Some(local), Some(remote)) if remote.taken_at > local.taken_at => {
                Some(remote.source.clone())
            }
            (Some(local), _) => Some(local.source.clone()),
            (None, Some(remote)) => Some(remote.source.clone()),
            (None, None) => None,
        };
        let mut snapshots: Vec<RestoreCandidate> = local.into_iter().chain(remote).collect();
        snapshots.sort_by_key(|candidate| std::cmp::Reverse(candidate.taken_at));
        candidates.candidates = snapshots;
        for (name, time) in self.replaced_states()? {
            candidates.candidates.push(RestoreCandidate {
                kind: SourceKind::Replaced,
                source: RestoreSource::Replaced { name },
                taken_at: Some(time),
            });
        }
        Ok(candidates)
    }

    /// Every file of the snapshot commit `commit`, checked: plain files only, at paths a
    /// snapshot can have, text, and none too large.
    async fn read_snapshot_commit(&self, commit: &str) -> Result<BTreeMap<String, String>> {
        let repository = &self.location.repository;
        let entries = self.git.ls_tree_r(repository, commit).await?;
        if entries.len() > MAX_SNAPSHOT_FILES {
            bail!(
                "the snapshot holds {} files, more than a snapshot can",
                entries.len()
            );
        }
        for entry in &entries {
            if let Some(problem) = snapshot_path_problem(&entry.path) {
                bail!(
                    "the snapshot's file \"{}\" {problem}; nothing was restored",
                    quoted(&entry.path)
                );
            }
            if entry.kind != "blob" || !(entry.mode == "100644" || entry.mode == "100755") {
                let what = if entry.mode == "120000" {
                    "a symbolic link"
                } else {
                    "not a plain file"
                };
                bail!(
                    "the snapshot's {} is {what}; a snapshot holds only plain files, nothing was restored",
                    quoted(&entry.path)
                );
            }
        }
        let objects: Vec<String> = entries.iter().map(|entry| entry.object.clone()).collect();
        let blobs = self.git.cat_file_batch(repository, &objects).await?;
        let mut files = BTreeMap::new();
        for (entry, blob) in entries.into_iter().zip(blobs) {
            let Some(bytes) = blob else {
                bail!(
                    "the snapshot's {} is missing from the repository",
                    entry.path
                );
            };
            if bytes.len() > MAX_EXPORTED_FILE_BYTES {
                bail!(
                    "the snapshot's {} is larger than {MAX_EXPORTED_FILE_BYTES} bytes; nothing was restored",
                    entry.path
                );
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| anyhow!("the snapshot's {} is not text", entry.path))?;
            files.insert(entry.path, text);
        }
        Ok(files)
    }

    /// The source resolved to a snapshot or replaced state, read whole.
    async fn resolve_source(
        &self,
        source: &RestoreSource,
        exporters: &[Arc<dyn StateExporter>],
    ) -> Result<Resolved> {
        let source = match source {
            RestoreSource::Newest => self.restore_candidates().await?.newest.context(
                "there is no snapshot to restore: neither this machine nor the remote has one",
            )?,
            other => other.clone(),
        };
        match &source {
            RestoreSource::Newest => bail!("no snapshot was found"),
            RestoreSource::Snapshot { commit } => {
                let is_id = (commit.len() == 40 || commit.len() == 64)
                    && commit
                        .chars()
                        .all(|character| character.is_ascii_hexdigit());
                if !is_id {
                    bail!("\"{}\" is not a snapshot's commit id", quoted(commit));
                }
                let repository = &self.location.repository;
                let reference = state_branch::state_ref(&self.backup_options().state_branch)?;
                let kind = if self.git.is_ancestor(repository, commit, &reference).await {
                    SourceKind::Local
                } else if self
                    .git
                    .is_ancestor(repository, commit, REMOTE_STATE_REF)
                    .await
                {
                    SourceKind::Remote
                } else {
                    bail!(
                        "{} is not a snapshot of this project, here or on the remote; list the snapshots again",
                        short(commit)
                    );
                };
                let files = self.read_snapshot_commit(commit).await?;
                Ok(Resolved {
                    taken_at: self.snapshot_time_of(commit).await,
                    source: source.clone(),
                    kind,
                    files,
                })
            }
            RestoreSource::Replaced { name } => {
                let Some(taken_at) = replaced_time(name) else {
                    bail!("\"{}\" is not the name of a replaced state", quoted(name));
                };
                let path = self.replaced_dir().join(name);
                let is_file = std::fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_file());
                if !is_file {
                    bail!("there is no replaced state {name}");
                }
                let reader = StateStore::open_reader_at(&path)?;
                // Kept on this machine: read whole, nothing withheld.
                let plain = Secrets::new(&[], &[])?;
                let snapshot = reader.read(|read| snapshot::export(read, exporters, &plain))?;
                if !snapshot.problems.is_empty() {
                    bail!(
                        "the replaced state {name} could not be read whole: {}",
                        snapshot.problems.join("; ")
                    );
                }
                Ok(Resolved {
                    source: source.clone(),
                    kind: SourceKind::Replaced,
                    taken_at: Some(taken_at),
                    files: snapshot.files,
                })
            }
        }
    }

    /// The registered exporters with valid, distinct namespaces, by namespace.
    fn named_exporters(&self) -> Vec<(String, Arc<dyn StateExporter>)> {
        let mut seen = BTreeSet::new();
        self.exporters()
            .into_iter()
            .filter_map(|exporter| {
                let name =
                    std::panic::catch_unwind(AssertUnwindSafe(|| exporter.namespace().to_string()))
                        .ok()?;
                (namespace_problem(&name).is_none() && seen.insert(name.to_ascii_lowercase()))
                    .then_some((name, exporter))
            })
            .collect()
    }

    async fn plan_restore(&self, source: &RestoreSource) -> Result<Plan> {
        let writer = self.writer_or_error()?;
        let exporters = self.named_exporters();
        let plain_exporters: Vec<Arc<dyn StateExporter>> = exporters
            .iter()
            .map(|(_, exporter)| exporter.clone())
            .collect();
        let resolved = self.resolve_source(source, &plain_exporters).await?;
        let options = self.backup_options();
        // Only a snapshot withholds; a replaced state holds every task under its own name.
        let secrets = match resolved.kind {
            SourceKind::Replaced => None,
            SourceKind::Local | SourceKind::Remote => Some(
                Secrets::new(&options.secret_patterns, &options.secret_keys)
                    .context("restoring needs valid secret patterns")?,
            ),
        };
        let plain = Secrets::new(&[], &[])?;
        let (local, local_tasks) = writer.read(|read| {
            let local = snapshot::export(read, &plain_exporters, &plain)?;
            let mut tasks = BTreeMap::new();
            for id in read.task_ids()? {
                if let Some(record) = read.task(&id)? {
                    tasks.insert(id, record);
                }
            }
            Ok((local.files, tasks))
        })?;
        let mut plan = read_plan(
            resolved,
            &local,
            &local_tasks,
            &exporters,
            secrets.as_ref(),
            options.replaced_keep_days.max(1),
        )?;
        let (worktrees, records) =
            missing_on_this_machine(&plan.tasks, &plan.runs, &self.location.files);
        plan.preview.missing_worktrees = worktrees;
        plan.preview.missing_run_records = records;
        plan.preview.activity = self.activity();
        Ok(plan)
    }

    /// What restoring from `source` would do, read without changing anything (but fetching
    /// the remote's state branch when `source` is the newest).
    pub async fn restore_preview(&self, source: &RestoreSource) -> Result<RestorePreview> {
        Ok(self.plan_restore(source).await?.preview)
    }

    /// Restores the state from `source` at `now`. Refuses while work runs; fails, changing
    /// nothing, while a landing holds the landing lock or is in flight, and on any snapshot
    /// that is not whole. Running it twice from one source leaves the same state.
    pub async fn restore(
        &self,
        source: &RestoreSource,
        now: DateTime<Utc>,
    ) -> Result<RestoreOutcome> {
        let activity = self.activity();
        if !activity.is_empty() {
            return Ok(RestoreOutcome::Refused { activity });
        }
        let plan = self.plan_restore(source).await?;
        self.take_lock_for(RESTORE_HOLDER)?;
        let restored = self.restore_locked(plan, now);
        self.release_lock(RESTORE_HOLDER);
        restored
    }

    fn restore_locked(&self, plan: Plan, now: DateTime<Utc>) -> Result<RestoreOutcome> {
        if self.in_flight()?.is_some() {
            bail!("a landing is in flight; it completes first, then restore again");
        }
        let activity = self.activity();
        if !activity.is_empty() {
            return Ok(RestoreOutcome::Refused { activity });
        }
        let writer = self.writer_or_error()?;
        let exporters = self.named_exporters();
        let (name, path) = self.fresh_replaced_path(now)?;
        let imported = writer.import_without_hooks(ImportReason::Restore, Some(&path), |tx| {
            apply_plan(tx, &plan, &exporters)
        });
        if let Err(error) = imported {
            std::fs::remove_file(&path).ok();
            return Err(error);
        }
        if let Err(error) = self.prune_replaced(now) {
            log::error!("removing old replaced states: {error:#}");
        }
        Ok(RestoreOutcome::Restored {
            preview: Box::new(plan.preview),
            replaced: name,
        })
    }

    /// The marks of the tasks the current state names worktrees or run records for that are
    /// not on this machine: [`WORKTREE_MISSING`] and [`RUN_RECORD_MISSING`].
    pub fn missing_marks(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let state = self
            .state
            .as_ref()
            .context("the project's state store is not open")?;
        let (tasks, runs) = state.read(|read| {
            let mut tasks = BTreeMap::new();
            for id in read.task_ids()? {
                if let Some(record) = read.task(&id)? {
                    tasks.insert(id, record);
                }
            }
            let runs: Vec<(String, toml::Table)> = read
                .kv(RUNS_NAMESPACE)
                .scan("")?
                .into_iter()
                .filter_map(|(id, value)| match value {
                    toml::Value::Table(table) => Some((id, table)),
                    _ => None,
                })
                .collect();
            Ok((tasks, runs))
        })?;
        let (worktrees, records) = missing_on_this_machine(&tasks, &runs, &self.location.files);
        let mut marks: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for missing in worktrees {
            marks
                .entry(missing.task)
                .or_default()
                .push(WORKTREE_MISSING.to_string());
        }
        for missing in records.into_iter().filter(|missing| missing.active) {
            let task_marks = marks.entry(missing.task).or_default();
            if !task_marks.iter().any(|mark| mark == RUN_RECORD_MISSING) {
                task_marks.push(RUN_RECORD_MISSING.to_string());
            }
        }
        Ok(marks)
    }
}
