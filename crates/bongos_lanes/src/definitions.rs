//! The lane declarations in a project's definitions: the validator of the `lanes` folder, which
//! reads `lanes/lanes.toml` and refuses a landing whose table does not parse or validate, and the
//! workflow validation hook that refuses a prompt naming a lane instance (`build-2`) where it must
//! name the kind. Both name the line of each problem.

use std::path::Path;

use bongos_lanes_model::validate_lane_table;
use bongos_lanes_model::{
    LaneTable, ValidationContext, parse, parse_accepting, prompt_lane_problems,
};
use bongos_project_repo::DefinitionFolders;
use bongos_project_repo_model::Problem;
use bongos_workflow::ValidationHooks;
use bongos_workflow_model::{Definition, Diagnostic, Severity};
use gpui::App;

/// The folder of `definitions/` that lanes own.
pub const LANES_FOLDER: &str = "lanes";
/// The lane table, inside [`LANES_FOLDER`].
pub const LANES_FILE: &str = "lanes.toml";

/// Registers the `lanes` definition folder and the prompt check of workflow validation.
pub fn register(cx: &mut App) {
    DefinitionFolders::register(LANES_FOLDER, validate_folder, cx);
    ValidationHooks::register(prompt_lane_diagnostics);
}

/// Whether `path` (relative to `definitions/`) is the lane table.
fn is_lane_table(path: &Path) -> bool {
    let mut parts = path.components();
    parts
        .next()
        .is_some_and(|part| part.as_os_str() == LANES_FOLDER)
        && parts
            .next()
            .is_some_and(|part| part.as_os_str() == LANES_FILE)
        && parts.next().is_none()
}

/// The names a table refers to that only master can resolve (workflows, task ids). The folder
/// validator sees one file alone, so it takes them as known; what it judges is the table itself.
fn names_taken_as_known(table: &LaneTable) -> ValidationContext {
    let mut context = ValidationContext::default();
    for rule in &table.rules {
        for located in [&rule.workflow, &rule.refresh].into_iter().flatten() {
            context.workflow_names.insert(located.value.clone());
        }
    }
    for pin in &table.pins {
        if let Some(task) = &pin.task {
            context.task_ids.insert(task.value.clone());
        }
    }
    for exemption in &table.exemptions {
        for task in &exemption.tasks {
            context.task_ids.insert(task.value.clone());
        }
    }
    context
}

static LANE_TABLE_FIELDS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The lane table keys other features define (`kind.pulls`, `kind.binding.pools`,
/// `kind.binding.hooks`). The table refuses such a key ("pulls are not available yet") until the
/// feature that reads it registers it; its value is kept raw on the kind or binding either way.
pub struct LaneTableFields;

impl LaneTableFields {
    /// Accepts `key` in the lane table; registering it twice keeps it once.
    pub fn register(key: &str) {
        let mut keys = LANE_TABLE_FIELDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !keys.iter().any(|known| known == key) {
            keys.push(key.to_string());
        }
    }

    /// Refuses `key` again, as when the feature that reads it is turned off.
    pub fn unregister(key: &str) {
        LANE_TABLE_FIELDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|known| known != key);
    }

    /// The keys registered now.
    pub fn registered() -> Vec<String> {
        LANE_TABLE_FIELDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The problems of one file of the `lanes` folder: `path` is relative to `definitions/`. Only
/// `lanes/lanes.toml` is read; each problem names its line. The keys registered with
/// [`LaneTableFields`] are accepted.
pub fn validate_folder(path: &Path, text: &str) -> Vec<Problem> {
    let registered = LaneTableFields::registered();
    let accepted: Vec<&str> = registered.iter().map(String::as_str).collect();
    validate_folder_accepting(path, text, &accepted)
}

/// [`validate_folder`] with the lane table keys `accepted` (key paths such as `kind.pulls`).
pub fn validate_folder_accepting(path: &Path, text: &str, accepted: &[&str]) -> Vec<Problem> {
    if !is_lane_table(path) {
        return Vec::new();
    }
    let shown = path.to_string_lossy().replace('\\', "/");
    let problem = |line: usize, message: &str| Problem {
        path: shown.clone(),
        reason: format!("line {line}: {message}"),
    };
    match parse_accepting(text, accepted) {
        Err(error) => vec![problem(error.line, &error.message)],
        Ok(table) => validate_lane_table(&table, &names_taken_as_known(&table))
            .iter()
            .map(|diagnostic| problem(diagnostic.line, &diagnostic.message))
            .collect(),
    }
}

/// The lane table of the definitions a workflow folder `dir` sits in
/// (`<definitions>/workflows/<name>`): `<definitions>/lanes/lanes.toml`. `None` when there is none
/// or it does not parse (the folder validator reports that).
fn lane_table_near(dir: &Path) -> Option<LaneTable> {
    let definitions = dir.parent()?.parent()?;
    let text = std::fs::read_to_string(definitions.join(LANES_FOLDER).join(LANES_FILE)).ok()?;
    parse(&text).ok()
}

/// Whether a dispatch rule of `table` names the workflow `name` (to run or to refresh).
fn named_by_a_rule(table: &LaneTable, name: &str) -> bool {
    table.rules.iter().any(|rule| {
        [&rule.workflow, &rule.refresh]
            .into_iter()
            .flatten()
            .any(|named| named.value == name)
    })
}

/// The validation hook, run by `workflow: validate` and by the workflow landing check: every
/// prompt or script of a workflow that a dispatch rule names, that names an instance of a
/// declared lane kind (`build-2`), is an error on the node's `prompt` or `script` line, whose
/// message names the file's own line. A project with no lane table, and a workflow no rule
/// names, are not checked.
pub fn prompt_lane_diagnostics(definition: &Definition, dir: &Path) -> Vec<Diagnostic> {
    let Some(table) = lane_table_near(dir) else {
        return Vec::new();
    };
    if !named_by_a_rule(&table, &definition.name) {
        return Vec::new();
    }
    let kinds = table.kind_names();
    let mut out = Vec::new();
    for node in &definition.nodes {
        for file in [&node.prompt, &node.script].into_iter().flatten() {
            // A file that cannot be read is reported by the definition's own validation.
            let Ok(text) = std::fs::read_to_string(dir.join(&file.value)) else {
                continue;
            };
            out.extend(
                prompt_lane_problems(&file.value, &text, &kinds)
                    .into_iter()
                    .map(|problem| Diagnostic {
                        node: Some(node.name.clone()),
                        line: file.line,
                        severity: Severity::Error,
                        message: problem.message,
                    }),
            );
        }
    }
    out
}
