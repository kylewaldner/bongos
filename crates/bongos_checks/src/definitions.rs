//! The check declarations in a project's definitions: the validator of the `checks` folder, which
//! reads the named sets in `checks/sets/*.toml`, and the landing check `checks-valid`, which reads
//! the `checks` table of every task definition a change adds or redefines. Both name the file and
//! the line of each problem, so a broken set never reaches master.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use bongos_checks_model::{Finding, NamedSets, Severity, decl, effective_set, validate};
use bongos_project_repo::{
    CheckInput, CheckOutcome, CheckPhase, DefinitionFolders, LandingCheck, LandingChecks,
};
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, TASK_FILE};
use bongos_project_repo_model::{ChangeSummary, DefinitionTree, Problem};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::App;

/// The folder of `definitions/` that task checks own.
pub const CHECKS_FOLDER: &str = "checks";
/// The folder of the named sets, inside [`CHECKS_FOLDER`].
const SETS_FOLDER: &str = "sets";
/// The name of the landing check over the task definitions of a change.
pub const CHECKS_VALID: &str = "checks-valid";

/// Registers the `checks` definition folder and the `checks-valid` landing check.
pub fn register(cx: &mut App) {
    DefinitionFolders::register(CHECKS_FOLDER, validate_folder, cx);
    LandingChecks::register(std::sync::Arc::new(ChecksValid), cx);
}

/// Whether `path` (relative to `definitions/`) is a named set file.
fn is_set_file(path: &Path) -> bool {
    let mut parts = path.components();
    parts
        .next()
        .is_some_and(|part| part.as_os_str() == CHECKS_FOLDER)
        && parts
            .next()
            .is_some_and(|part| part.as_os_str() == SETS_FOLDER)
        && path
            .extension()
            .is_some_and(|extension| extension == "toml")
}

fn problem(finding: &Finding) -> Problem {
    let line = finding
        .line
        .map_or_else(|| "line unknown".to_string(), |line| format!("line {line}"));
    let reason = match &finding.check {
        Some(check) => format!("{line}, check {check}: {}", finding.message),
        None => format!("{line}: {}", finding.message),
    };
    Problem {
        path: finding.file.clone(),
        reason,
    }
}

/// The problems of one file of the `checks` folder: `path` is relative to `definitions/`. Only
/// the named sets are read; scripts and other files are not this folder's to judge, and a script a
/// set names is looked for by `checks-valid` in the tasks that use it.
pub fn validate_folder(path: &Path, text: &str) -> Vec<Problem> {
    if !is_set_file(path) {
        return Vec::new();
    }
    let shown = path.to_string_lossy().replace('\\', "/");
    let table = match decl::parse_file(&shown, text) {
        Ok(table) => table,
        Err(finding) => return vec![problem(&finding)],
    };
    let mut decls = table.check.clone();
    decls.extend(table.closing.iter().cloned());
    table
        .findings
        .iter()
        .cloned()
        .chain(validate(&shown, &decls, &|_| true))
        .filter(|finding| finding.severity == Severity::Error)
        .map(|finding| problem(&finding))
        .collect()
}

/// A landing check that parses the `checks` table of every task definition the change adds or
/// redefines, and refuses the change with the file and line of each error.
pub struct ChecksValid;

impl LandingCheck for ChecksValid {
    fn name(&self) -> &str {
        CHECKS_VALID
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::InQueue
    }

    fn applies(&self, summary: &ChangeSummary) -> bool {
        !summary.tasks_added.is_empty() || !summary.tasks_redefined.is_empty()
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let definitions = input.root.join(DEFINITIONS_DIR);
        let touched: BTreeSet<String> = input
            .summary
            .tasks_added
            .iter()
            .chain(&input.summary.tasks_redefined)
            .cloned()
            .collect();
        async move { check_task_sets(&definitions, &touched) }.boxed()
    }
}

/// Whether `path`, relative to `definitions`, names a file there.
fn exists_in(definitions: &Path, path: &str) -> bool {
    let relative = Path::new(path);
    relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        && definitions.join(relative).is_file()
}

/// The named sets of the tree at `definitions`, read from `checks/sets/*.toml`.
fn named_sets(definitions: &Path) -> NamedSets {
    let directory = definitions.join(CHECKS_FOLDER).join(SETS_FOLDER);
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "toml") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path) {
                files.push((format!("{CHECKS_FOLDER}/{SETS_FOLDER}/{name}"), text));
            }
        }
    }
    files.sort();
    NamedSets::parse(files)
}

/// The errors in the `checks` tables of the tasks `touched` in the definitions at `definitions`.
pub(crate) fn task_set_findings(definitions: &Path, touched: &BTreeSet<String>) -> Vec<String> {
    // Definitions that do not parse are refused by the schema check.
    let Ok(tree) = DefinitionTree::parse_dir(definitions) else {
        return Vec::new();
    };
    let sets = named_sets(definitions);
    let mut found: Vec<String> = Vec::new();
    for node in tree.nodes() {
        let id = node.definition.id.as_str();
        if !touched.contains(id) || !node.definition.extra.contains_key("checks") {
            continue;
        }
        let file = if node.folder.is_empty() {
            TASK_FILE.to_string()
        } else {
            format!("{}/{TASK_FILE}", node.folder)
        };
        let mut task_findings: Vec<Finding> = Vec::new();
        match std::fs::read_to_string(definitions.join(&file)) {
            Ok(text) => match decl::parse_file(&file, &text) {
                Ok(table) => {
                    let mut decls = table.check.clone();
                    decls.extend(table.closing.iter().cloned());
                    task_findings.extend(table.findings);
                    task_findings.extend(validate(&file, &decls, &|path| {
                        exists_in(definitions, path)
                    }));
                }
                Err(finding) => task_findings.push(finding),
            },
            Err(error) => {
                found.push(format!(
                    "{file}, line unknown: the file could not be read: {error}"
                ));
                continue;
            }
        }
        // What only the effective set sees, such as a `use` naming a set that is not there; the
        // rest of its findings repeat what the file's own parse said.
        for finding in effective_set(id, &tree, &sets, &|_| false).findings {
            let known = task_findings
                .iter()
                .any(|seen| seen.file == finding.file && seen.message == finding.message);
            if finding.file == file && !known {
                task_findings.push(finding);
            }
        }
        found.extend(
            task_findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error)
                .map(ToString::to_string),
        );
    }
    found
}

/// The outcome of `checks-valid` for the definitions at `definitions`: a failure naming the file,
/// line and check of every error, or a pass.
pub(crate) fn check_task_sets(definitions: &Path, touched: &BTreeSet<String>) -> CheckOutcome {
    CheckOutcome::from_refusals(task_set_findings(definitions, touched))
}
