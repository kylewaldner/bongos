//! The export of a project's state into a snapshot: plain text files read from one consistent
//! read of the state store, with every secret withheld. `FORMAT` holds [`FORMAT_LINE`],
//! `tasks/<id>.toml` each task's state record (todo states and history included), `runs.toml`
//! the project's runs, `withheld.toml` where a value was withheld (never the value), and each
//! registered [`StateExporter`] owns one file or folder named for its namespace. Run record
//! files (`files/*/runs/`) are never part of it.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use bongos_project_repo_model::checks::compile_secret_patterns;
use regex::Regex;
use serde::{Deserialize, Serialize};
use util::rel_path::RelPathBuf;

use crate::registries::StateExporter;
use crate::state::StateRead;

pub const FORMAT_FILE: &str = "FORMAT";
/// The first line of `FORMAT`: the kind and version of the snapshot.
pub const FORMAT_LINE: &str = "bongos-state 1";
pub const TASKS_DIR: &str = "tasks";
pub const RUNS_FILE: &str = "runs.toml";
pub const WITHHELD_FILE: &str = "withheld.toml";
/// The top-level names of a snapshot no exporter may use.
pub const RESERVED_NAMES: [&str; 4] = [FORMAT_FILE, TASKS_DIR, RUNS_FILE, WITHHELD_FILE];
/// What a withheld value reads as in a snapshot.
pub const WITHHELD_SECRET: &str = "<withheld: secret>";
/// The `kv` namespace of the project's runs: one key per run id, whose value is a table with at
/// least `task`, `workflow`, `state` and `outcome`. Run record files stay out of the snapshot.
pub const RUNS_NAMESPACE: &str = "runs";
/// The largest file an exporter may give a snapshot.
pub const MAX_EXPORTED_FILE_BYTES: usize = 16 * 1024 * 1024;
/// The longest path component a snapshot keeps.
const MAX_COMPONENT_BYTES: usize = 200;

/// Why a value was withheld.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WithheldReason {
    /// The schema marks the key secret.
    SecretKey,
    /// The text matched a secret pattern.
    SecretPattern,
}

/// One withheld value: the file and the place in it, never the value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withheld {
    pub file: String,
    pub location: String,
    pub reason: WithheldReason,
}

/// The secret patterns and keys a snapshot withholds.
pub(crate) struct Secrets {
    patterns: Vec<Regex>,
    /// Dotted keys the schema marks secret (`linear.token`); a key without a dot matches that
    /// key name anywhere.
    keys: Vec<String>,
}

impl Secrets {
    /// Fails closed: an invalid pattern stops the snapshot, since what it was meant to catch
    /// cannot be told apart. The error names the pattern, which is a setting, not a secret.
    pub(crate) fn new(patterns: &[String], keys: &[String]) -> Result<Self> {
        let patterns = compile_secret_patterns(patterns).map_err(|error| {
            anyhow!("{error}; no snapshot is taken until the secret patterns are fixed")
        })?;
        Ok(Self {
            patterns,
            keys: keys.to_vec(),
        })
    }

    /// `text` with every match of every pattern replaced by [`WITHHELD_SECRET`], or `None` when
    /// nothing matched. Overlapping matches of different patterns are merged first, so no part
    /// of a secret one pattern found survives because another pattern found a shorter piece.
    pub(crate) fn redact(&self, text: &str) -> Option<String> {
        let mut spans: Vec<(usize, usize)> = self
            .patterns
            .iter()
            .flat_map(|pattern| pattern.find_iter(text))
            .filter(|found| found.end() > found.start())
            .map(|found| (found.start(), found.end()))
            .collect();
        if spans.is_empty() {
            return None;
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let mut redacted = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end) in merged {
            redacted.push_str(&text[at..start]);
            redacted.push_str(WITHHELD_SECRET);
            at = end;
        }
        redacted.push_str(&text[at..]);
        Some(redacted)
    }

    /// `text` redacted, for messages: never fails, never returns a secret.
    pub(crate) fn clean(&self, text: &str) -> String {
        self.redact(text).unwrap_or_else(|| text.to_string())
    }

    fn is_secret_key(&self, path: &[String]) -> bool {
        let Some(leaf) = path.last() else {
            return false;
        };
        let dotted = path.join(".");
        self.keys.iter().any(|key| {
            if key.contains('.') {
                dotted == *key || dotted.ends_with(&format!(".{key}"))
            } else {
                leaf == key
            }
        })
    }

    /// Withholds the secrets in `value` (keys and values, recursively), noting each in
    /// `withheld` under `file`.
    fn redact_value(
        &self,
        value: &mut toml::Value,
        path: &mut Vec<String>,
        file: &str,
        withheld: &mut Vec<Withheld>,
    ) {
        match value {
            toml::Value::String(text) => {
                if let Some(redacted) = self.redact(text) {
                    *text = redacted;
                    withheld.push(Withheld {
                        file: file.to_string(),
                        location: location(path),
                        reason: WithheldReason::SecretPattern,
                    });
                }
            }
            toml::Value::Array(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    path.push(format!("[{index}]"));
                    self.redact_value(item, path, file, withheld);
                    path.pop();
                }
            }
            toml::Value::Table(table) => {
                let entries = std::mem::take(table);
                for (key, mut item) in entries {
                    let mut name = key.clone();
                    let mut key_withheld = false;
                    if let Some(redacted) = self.redact(&key) {
                        name = redacted;
                        key_withheld = true;
                    }
                    // Two keys withheld alike stay two entries.
                    let mut unique = name.clone();
                    let mut count = 2;
                    while table.contains_key(&unique) {
                        unique = format!("{name} {count}");
                        count += 1;
                    }
                    path.push(unique.clone());
                    if key_withheld {
                        withheld.push(Withheld {
                            file: file.to_string(),
                            location: format!("{} (key)", location(path)),
                            reason: WithheldReason::SecretPattern,
                        });
                    }
                    // The key's real name decides whether the schema marks it secret.
                    let mut real = path.clone();
                    if let Some(last) = real.last_mut() {
                        *last = key;
                    }
                    if self.is_secret_key(&real) && holds_value(&item) {
                        item = toml::Value::String(WITHHELD_SECRET.to_string());
                        withheld.push(Withheld {
                            file: file.to_string(),
                            location: location(path),
                            reason: WithheldReason::SecretKey,
                        });
                    } else {
                        self.redact_value(&mut item, path, file, withheld);
                    }
                    path.pop();
                    table.insert(unique, item);
                }
            }
            toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }

    /// `table` as TOML text with its secrets withheld.
    fn redacted_toml(
        &self,
        table: toml::Table,
        file: &str,
        withheld: &mut Vec<Withheld>,
    ) -> Result<String> {
        let mut value = toml::Value::Table(table);
        self.redact_value(&mut value, &mut Vec::new(), file, withheld);
        let text = toml::to_string(&value).with_context(|| format!("writing {file}"))?;
        // A match across a key and its value is caught on the text.
        Ok(match self.redact(&text) {
            Some(redacted) => {
                withheld.push(Withheld {
                    file: file.to_string(),
                    location: "(text)".to_string(),
                    reason: WithheldReason::SecretPattern,
                });
                redacted
            }
            None => text,
        })
    }
}

fn holds_value(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn location(path: &[String]) -> String {
    if path.is_empty() {
        "(whole file)".to_string()
    } else {
        path.join(".")
    }
}

/// The files of one snapshot, by path, with what was withheld and what went wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<String, String>,
    pub withheld: Vec<Withheld>,
    /// Exporters that failed or gave files that were refused; the rest of the snapshot is whole.
    pub problems: Vec<String>,
    /// The store's generation the snapshot was read at.
    pub generation: u64,
}

/// Whether `component` can be one part of a snapshot path.
pub(crate) fn is_safe_component(component: &str) -> bool {
    !component.is_empty()
        && component.len() <= MAX_COMPONENT_BYTES
        && component != "."
        && component != ".."
        && !component.to_ascii_lowercase().starts_with(".git")
        && !component
            .chars()
            .any(|character| character == '/' || character == '\\' || character.is_control())
}

/// Why the path `path` that the exporter of `namespace` gave is refused, if it is.
pub(crate) fn exported_path_problem(namespace: &str, path: &str) -> Option<String> {
    if path.starts_with('/') || path.starts_with('\\') {
        return Some("is absolute".to_string());
    }
    let components: Vec<&str> = path.split('/').collect();
    if let Some(bad) = components
        .iter()
        .find(|component| !is_safe_component(component))
    {
        return Some(if *bad == ".." {
            "climbs out with `..`".to_string()
        } else if bad.to_ascii_lowercase().starts_with(".git") {
            "names a git file".to_string()
        } else {
            "has an empty or invalid component".to_string()
        });
    }
    if components.first() != Some(&namespace) {
        return Some(format!("is outside the exporter's namespace {namespace}"));
    }
    None
}

/// Why `namespace` cannot be an exporter's, if it cannot.
pub(crate) fn namespace_problem(namespace: &str) -> Option<&'static str> {
    if !is_safe_component(namespace) {
        return Some("is not a valid file name");
    }
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(namespace))
    {
        return Some("is reserved by the snapshot");
    }
    None
}

/// Exports the state seen by `read` with `exporters`, withholding `secrets`. One failing or
/// panicking exporter is reported in [`Snapshot::problems`]; the rest is exported.
pub(crate) fn export(
    read: &StateRead,
    exporters: &[Arc<dyn StateExporter>],
    secrets: &Secrets,
) -> Result<Snapshot> {
    let mut snapshot = Snapshot {
        generation: read.generation()?,
        ..Snapshot::default()
    };
    snapshot
        .files
        .insert(FORMAT_FILE.to_string(), format!("{FORMAT_LINE}\n"));
    export_tasks(read, secrets, &mut snapshot)?;
    export_runs(read, secrets, &mut snapshot)?;
    let mut namespaces = BTreeSet::new();
    for exporter in exporters {
        export_one(
            read,
            exporter.as_ref(),
            secrets,
            &mut namespaces,
            &mut snapshot,
        );
    }
    let withheld = WithheldFile {
        withheld: snapshot.withheld.clone(),
    };
    snapshot.files.insert(
        WITHHELD_FILE.to_string(),
        toml::to_string(&withheld).context("writing withheld.toml")?,
    );
    Ok(snapshot)
}

#[derive(Serialize, Deserialize)]
struct WithheldFile {
    #[serde(default)]
    withheld: Vec<Withheld>,
}

fn export_tasks(read: &StateRead, secrets: &Secrets, snapshot: &mut Snapshot) -> Result<()> {
    let mut withheld_names = 0;
    for id in read.task_ids()? {
        let Some(record) = read.task(&id)? else {
            continue;
        };
        let name = match secrets.redact(&id) {
            None if is_safe_component(&id) && !id.ends_with(".toml") => id.clone(),
            None => {
                withheld_names += 1;
                snapshot.problems.push(format!(
                    "a task id is not a valid file name; its state is in tasks/unnamed-{withheld_names}.toml"
                ));
                format!("unnamed-{withheld_names}")
            }
            Some(_) => {
                withheld_names += 1;
                let name = format!("withheld-{withheld_names}");
                snapshot.withheld.push(Withheld {
                    file: format!("{TASKS_DIR}/{name}.toml"),
                    location: "task id".to_string(),
                    reason: WithheldReason::SecretPattern,
                });
                name
            }
        };
        let file = format!("{TASKS_DIR}/{name}.toml");
        let table = match toml::Value::try_from(&record)
            .with_context(|| format!("writing the state of task {}", secrets.clean(&id)))?
        {
            toml::Value::Table(table) => table,
            _ => toml::Table::new(),
        };
        let text = secrets.redacted_toml(table, &file, &mut snapshot.withheld)?;
        snapshot.files.insert(file, text);
    }
    Ok(())
}

fn export_runs(read: &StateRead, secrets: &Secrets, snapshot: &mut Snapshot) -> Result<()> {
    let mut runs = Vec::new();
    for (id, value) in read.kv(RUNS_NAMESPACE).scan("")? {
        let mut run = match value {
            toml::Value::Table(table) => table,
            other => {
                let mut table = toml::Table::new();
                table.insert("value".to_string(), other);
                table
            }
        };
        run.insert("id".to_string(), toml::Value::String(id));
        runs.push(toml::Value::Table(run));
    }
    let mut table = toml::Table::new();
    table.insert("run".to_string(), toml::Value::Array(runs));
    let text = secrets.redacted_toml(table, RUNS_FILE, &mut snapshot.withheld)?;
    snapshot.files.insert(RUNS_FILE.to_string(), text);
    Ok(())
}

fn export_one(
    read: &StateRead,
    exporter: &dyn StateExporter,
    secrets: &Secrets,
    namespaces: &mut BTreeSet<String>,
    snapshot: &mut Snapshot,
) {
    let namespace =
        match std::panic::catch_unwind(AssertUnwindSafe(|| exporter.namespace().to_string())) {
            Ok(namespace) => namespace,
            Err(_) => {
                snapshot
                    .problems
                    .push("an exporter panicked naming its namespace".to_string());
                return;
            }
        };
    let shown = secrets.clean(&namespace);
    if let Some(problem) = namespace_problem(&namespace) {
        snapshot
            .problems
            .push(format!("exporter {shown}: the namespace {problem}"));
        return;
    }
    if !namespaces.insert(namespace.to_ascii_lowercase()) {
        snapshot.problems.push(format!(
            "exporter {shown}: another exporter has this namespace"
        ));
        return;
    }
    if secrets.redact(&namespace).is_some() {
        snapshot.problems.push(format!(
            "exporter {shown}: the namespace matches a secret pattern"
        ));
        return;
    }
    let files: Vec<(RelPathBuf, String)> =
        match std::panic::catch_unwind(AssertUnwindSafe(|| exporter.export(read))) {
            Ok(Ok(files)) => files,
            Ok(Err(error)) => {
                snapshot.problems.push(format!(
                    "exporter {shown} failed: {}",
                    secrets.clean(&format!("{error:#}"))
                ));
                return;
            }
            Err(_) => {
                snapshot.problems.push(format!("exporter {shown} panicked"));
                return;
            }
        };
    let mut withheld_names = 0;
    for (path, text) in files {
        let path = path.as_unix_str().to_string();
        let shown_path = secrets.clean(&path);
        if let Some(problem) = exported_path_problem(&namespace, &path) {
            snapshot.problems.push(format!(
                "exporter {shown}: the file {shown_path} {problem}; left out"
            ));
            continue;
        }
        if text.len() > MAX_EXPORTED_FILE_BYTES {
            snapshot.problems.push(format!(
                "exporter {shown}: the file {shown_path} is larger than {MAX_EXPORTED_FILE_BYTES} bytes; left out"
            ));
            continue;
        }
        let components: Vec<String> = path
            .split('/')
            .map(|component| match secrets.redact(component) {
                Some(_) => {
                    withheld_names += 1;
                    format!("withheld-{withheld_names}")
                }
                None => component.to_string(),
            })
            .collect();
        let file = components.join("/");
        if file != path {
            snapshot.withheld.push(Withheld {
                file: file.clone(),
                location: "file name".to_string(),
                reason: WithheldReason::SecretPattern,
            });
        }
        if snapshot.files.contains_key(&file) {
            snapshot.problems.push(format!(
                "exporter {shown}: the file {} was given twice; the first is kept",
                secrets.clean(&file)
            ));
            continue;
        }
        let text = redact_file(secrets, &file, text, &mut snapshot.withheld);
        snapshot.files.insert(file, text);
    }
}

/// An exported file with its secrets withheld: key by key when it is a TOML table, otherwise
/// as text.
fn redact_file(
    secrets: &Secrets,
    file: &str,
    text: String,
    withheld: &mut Vec<Withheld>,
) -> String {
    if file.ends_with(".toml")
        && let Ok(table) = text.parse::<toml::Table>()
        && let Ok(redacted) = secrets.redacted_toml(table, file, withheld)
    {
        return redacted;
    }
    match secrets.redact(&text) {
        Some(redacted) => {
            withheld.push(Withheld {
                file: file.to_string(),
                location: "(text)".to_string(),
                reason: WithheldReason::SecretPattern,
            });
            redacted
        }
        None => text,
    }
}
