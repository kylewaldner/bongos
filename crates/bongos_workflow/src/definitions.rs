//! Workflow definitions on disk: where the three layers live, materializing the shipped set,
//! checking definitions and creating a new one from the built-in minimal template.
//!
//! Everything here is synchronous and takes its directories as parameters, so tests run over a
//! temporary directory and nothing reaches the real home. Nothing calls into this module while
//! `workflows.enabled` is false.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};

use crate::shipped::{self, Template};
use bongos_project_repo::{CommitId, PinGuard, ProjectRepo};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_work_model::layout::{WORKFLOWS_DIR, project_dir};
use bongos_work_model::slug::slugify;
use bongos_work_model::store::Task;
use bongos_workflow_model::layers::{self, DEFINITION_FILE};
use bongos_workflow_model::{
    Definition, Diagnostic, Layer, LayerDirs, Resolved, Severity, ValidationContext, Warning,
    parse, validate,
};

/// The work directory under the home directory (`<home>/.bongos`, as `bongos_work::WorkDirs`).
const WORK_DIR: &str = ".bongos";
/// The name `workflow: new` starts from when none is given.
pub const DEFAULT_NAME: &str = "new-workflow";
/// The prompt file of the minimal template.
pub const TEMPLATE_PROMPT_FILE: &str = "work.md";
/// The longest workflow name `workflow: new` accepts.
const MAX_NAME_LEN: usize = 80;

/// A file bongos ships inside a definition, embedded with `include_str!`.
pub struct ShippedFile {
    /// Relative to the shipped directory, with `/` separators: `<definition>/workflow.toml`.
    pub path: &'static str,
    pub contents: &'static str,
}

/// The shipped definitions: every file of [`crate::shipped::TEMPLATES`].
pub const SHIPPED_FILES: &[ShippedFile] = crate::shipped::FILES;

#[derive(Debug, thiserror::Error)]
pub enum DefinitionError {
    #[error("There is no profile directory to create the workflow in")]
    NoProfileDir,
    #[error("'{0}' is not a usable workflow name")]
    InvalidName(String),
    #[error("A workflow named '{0}' already exists in {1}")]
    NameTaken(String, PathBuf),
    #[error("{0} already exists")]
    DirectoryExists(PathBuf),
    #[error("There is no workflow named '{0}'")]
    NotFound(String),
    #[error("There are no workflow definitions")]
    NoDefinitions,
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] io::Error),
}

/// `<data dir>/workflows/shipped/<version>/`: a version change writes a new directory.
pub fn shipped_dir(data_dir: &Path, version: &str) -> PathBuf {
    data_dir.join("workflows").join("shipped").join(version)
}

/// `<home>/.bongos/workflows`, the profile layer when `workflows.definitions_dir` is null.
pub fn profile_dir(home: &Path) -> PathBuf {
    home.join(WORK_DIR).join(WORKFLOWS_DIR)
}

/// The profile layer directory: `definitions_dir` when the setting names one, else
/// [`profile_dir`] under `home`.
pub fn profile_layer_dir(home: &Path, definitions_dir: Option<&Path>) -> PathBuf {
    definitions_dir.map_or_else(|| profile_dir(home), Path::to_path_buf)
}

/// `<work root>/projects/<slug>/workflows`, the project layer of the project `slug`.
pub fn project_layer_dir(work_root: &Path, project_slug: &str) -> PathBuf {
    project_dir(work_root, project_slug).join(WORKFLOWS_DIR)
}

/// The project layer of a new-format project: `definitions/workflows/` of a commit of its
/// repository, materialized read-only under the project's `state/pins/` until this is dropped.
#[derive(Debug)]
pub struct ProjectLayer {
    /// What [`LayerDirs::project`] is set to; it need not exist (a commit with no workflows).
    pub dir: PathBuf,
    /// The commit the layer was read from, which a run records as its `definition_commit`.
    pub commit: CommitId,
    _pin: PinGuard,
}

/// The project layer of `repo` at `commit`, or at the commit the project shows (the last
/// verified one) with none. Reads git on the calling thread, as creating a run worktree does.
pub fn project_layer_in(
    repo: &ProjectRepo,
    commit: Option<CommitId>,
) -> anyhow::Result<ProjectLayer> {
    let commit = match commit {
        Some(commit) => commit,
        None => futures::executor::block_on(repo.shown_commit())?,
    };
    let pin = futures::executor::block_on(repo.materialize(&commit.0))?;
    let dir = pin.path().join(DEFINITIONS_DIR).join(WORKFLOWS_DIR);
    Ok(ProjectLayer {
        dir,
        commit,
        _pin: pin,
    })
}

/// The project layer of the new-format project `task` belongs to, at the project's verified
/// commit: what the picker, `workflow: validate`, a start, a restart and a fresh retry read.
/// `None` for an old-format task ([`project_layer_dir`] is its layer), for a project whose
/// repository is not open, and when the commit cannot be read (logged). A run keeps reading the
/// files copied into its attempt from the commit it recorded, so nothing pins an older commit.
pub fn project_layer_for(task: &Task, cx: &gpui::App) -> Option<ProjectLayer> {
    match try_project_layer_for(task, cx) {
        Ok(layer) => layer,
        Err(error) => {
            log::warn!("workflows: could not read the project's workflows: {error:#}");
            None
        }
    }
}

/// [`project_layer_for`] that says why the commit could not be read.
pub fn try_project_layer_for(task: &Task, cx: &gpui::App) -> anyhow::Result<Option<ProjectLayer>> {
    bongos_work::project_repo_of(task, cx)
        .map(|repo| project_layer_in(&repo, None))
        .transpose()
}

/// Writes the files of `files` that are missing under `dir` and returns how many it wrote. A file
/// that exists is never touched, so a shipped directory is written once. With no files it creates
/// nothing, not even `dir`.
pub fn materialize(dir: &Path, files: &[ShippedFile]) -> Result<usize, DefinitionError> {
    let mut written = 0;
    for file in files {
        let relative = Path::new(file.path);
        let safe = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !safe {
            return Err(DefinitionError::Io(
                relative.to_path_buf(),
                io::Error::new(io::ErrorKind::InvalidInput, "not a relative path"),
            ));
        }
        let path = dir.join(relative);
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| DefinitionError::Io(parent.into(), error))?;
        }
        write_new(&path, file.contents)?;
        written += 1;
    }
    Ok(written)
}

/// Writes `contents` to `path`, which must not exist.
fn write_new(path: &Path, contents: &str) -> Result<(), DefinitionError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| DefinitionError::Io(path.into(), error))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| DefinitionError::Io(path.into(), error))
}

/// What checking one definition found.
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionReport {
    pub name: String,
    pub layer: Layer,
    pub dir: PathBuf,
    /// Why the file could not be read or parsed; then there are no diagnostics.
    pub load_error: Option<String>,
    pub warnings: Vec<Warning>,
    pub diagnostics: Vec<Diagnostic>,
}

impl DefinitionReport {
    pub fn error_count(&self) -> usize {
        usize::from(self.load_error.is_some())
            + self
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Error)
                .count()
    }

    pub fn warning_count(&self) -> usize {
        self.warnings.len()
            + self
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Warning)
                .count()
    }

    /// The first error: the load error, else the first error diagnostic's message.
    pub fn first_error(&self) -> Option<String> {
        self.load_error.clone().or_else(|| {
            self.diagnostics
                .iter()
                .find(|diagnostic| diagnostic.severity == Severity::Error)
                .map(|diagnostic| diagnostic.message.clone())
        })
    }
}

/// The result of checking every definition, or one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub definitions: Vec<DefinitionReport>,
    /// Duplicate names and unreadable directories found while resolving, each an error.
    pub resolution_errors: Vec<String>,
}

impl Report {
    pub fn error_count(&self) -> usize {
        self.resolution_errors.len()
            + self
                .definitions
                .iter()
                .map(DefinitionReport::error_count)
                .sum::<usize>()
    }

    pub fn warning_count(&self) -> usize {
        self.definitions
            .iter()
            .map(DefinitionReport::warning_count)
            .sum()
    }

    /// The text shown in the diagnostics buffer: one block per definition with a line for each
    /// finding (severity, node, line, message), then a total.
    pub fn render(&self) -> String {
        let mut text = String::new();
        if self.definitions.is_empty() && self.resolution_errors.is_empty() {
            text.push_str("No workflow definitions found.\n");
            return text;
        }
        for error in &self.resolution_errors {
            text.push_str(&format!("error: {error}\n"));
        }
        for definition in &self.definitions {
            let status = match (definition.error_count(), definition.warning_count()) {
                (0, 0) => "ok".to_string(),
                (errors, warnings) => format!(
                    "{}, {}",
                    plural(errors, "error"),
                    plural(warnings, "warning")
                ),
            };
            text.push_str(&format!(
                "{} ({}) {}: {status}\n",
                definition.name,
                definition.layer.label(),
                definition.dir.join(DEFINITION_FILE).display(),
            ));
            if let Some(error) = &definition.load_error {
                text.push_str(&format!("  error: {error}\n"));
            }
            for warning in &definition.warnings {
                text.push_str(&format!(
                    "  warning: {}line {}: {}\n",
                    node_prefix(warning.node.as_deref()),
                    warning.line,
                    warning.message
                ));
            }
            for diagnostic in &definition.diagnostics {
                let severity = match diagnostic.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                };
                text.push_str(&format!(
                    "  {severity}: {}line {}: {}\n",
                    node_prefix(diagnostic.node.as_deref()),
                    diagnostic.line,
                    diagnostic.message
                ));
            }
        }
        text.push_str(&format!(
            "\n{} in {}, {}.\n",
            plural(self.error_count(), "error"),
            plural(self.definitions.len(), "definition"),
            plural(self.warning_count(), "warning"),
        ));
        text
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn node_prefix(node: Option<&str>) -> String {
    node.map(|node| format!("[{node}] ")).unwrap_or_default()
}

/// The files under `dir`, relative to it with `/` separators; what `uses` and `prompt` keys may
/// name. Symbolic links to directories are not followed.
fn list_files(dir: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
        let Ok(listing) = fs::read_dir(dir) else {
            return;
        };
        for item in listing.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            match item.file_type() {
                Ok(kind) if kind.is_dir() => walk(&item.path(), &relative, out),
                Ok(_) => {
                    out.insert(relative);
                }
                Err(_) => {}
            }
        }
    }
    let mut files = BTreeSet::new();
    walk(dir, "", &mut files);
    files
}

/// Parses and validates the definition in `dir`, with the default [`ValidationContext`] and the
/// files found in `dir`.
pub fn check_definition(resolved: &Resolved) -> DefinitionReport {
    check_definition_with(resolved, |_, _| {})
}

/// [`check_definition`] with `adjust` completing the [`ValidationContext`] from the parsed
/// definition before it is validated (a start fills in what it checks itself).
pub fn check_definition_with(
    resolved: &Resolved,
    adjust: impl FnOnce(&Definition, &mut ValidationContext),
) -> DefinitionReport {
    check_definition_in(resolved, ValidationContext::default(), adjust)
}

/// [`check_definition_with`] starting from `base` (the settings' ceilings and network switch)
/// instead of the default context; the files found in the definition's directory replace
/// `base.files`.
pub fn check_definition_in(
    resolved: &Resolved,
    base: ValidationContext,
    adjust: impl FnOnce(&Definition, &mut ValidationContext),
) -> DefinitionReport {
    check_parsed_in(resolved, base, adjust).0
}

/// [`check_definition_in`] that also hands back the parsed definition, `None` when it could not
/// be read or parsed.
pub fn check_parsed_in(
    resolved: &Resolved,
    base: ValidationContext,
    adjust: impl FnOnce(&Definition, &mut ValidationContext),
) -> (DefinitionReport, Option<Definition>) {
    let mut report = DefinitionReport {
        name: resolved.name.clone(),
        layer: resolved.layer,
        dir: resolved.dir.clone(),
        load_error: None,
        warnings: Vec::new(),
        diagnostics: Vec::new(),
    };
    let file = resolved.dir.join(DEFINITION_FILE);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => {
            report.load_error = Some(format!("cannot read {}: {error}", file.display()));
            return (report, None);
        }
    };
    let mut definition = match parse(&text) {
        Ok(definition) => definition,
        Err(error) => {
            report.load_error = Some(error.to_string());
            return (report, None);
        }
    };
    // A node with `output_schema` takes its fields and its one compiled schema from the file,
    // read here at load, before validation looks at the node's routes.
    let mut schema_errors = Vec::new();
    let mut from_file = Vec::new();
    for node in &mut definition.nodes {
        if node.output_schema.is_some() && node.output.is_empty() {
            match bongos_workflow_model::document(node, &resolved.dir) {
                Ok(schema) => {
                    from_file.push(node.name.clone());
                    node.output = schema.fields().to_vec();
                    node.schema = std::sync::Arc::new(schema);
                }
                Err(diagnostic) => schema_errors.push(diagnostic),
            }
        }
    }
    let mut context = ValidationContext {
        files: list_files(&resolved.dir),
        ..base
    };
    adjust(&definition, &mut context);
    report.warnings = definition.warnings();
    report.diagnostics = validate(&definition, &context);
    // Fields the loader took from a schema file are not a second declaration beside it.
    report.diagnostics.retain(|diagnostic| {
        !(diagnostic
            .message
            .starts_with("set either `output` or `output_schema`")
            && diagnostic
                .node
                .as_ref()
                .is_some_and(|node| from_file.contains(node)))
    });
    if !schema_errors.is_empty() {
        report.diagnostics.extend(schema_errors);
        report.diagnostics.sort_by_key(|diagnostic| diagnostic.line);
    }
    (report, Some(definition))
}

/// A check another crate adds to `workflow: validate`: given a parsed definition and its
/// directory, the problems it finds.
pub type ValidationHook = fn(&Definition, &Path) -> Vec<Diagnostic>;

static HOOKS: std::sync::Mutex<Vec<ValidationHook>> = std::sync::Mutex::new(Vec::new());

/// The checks other crates add to the workflow validation entry point ([`validate_definitions`],
/// which `workflow: validate` runs): each registered hook runs on every definition that parsed,
/// and its diagnostics join the definition's own, in line order.
pub struct ValidationHooks;

impl ValidationHooks {
    /// Adds `hook`; registering the same hook twice runs it once.
    pub fn register(hook: ValidationHook) {
        let mut hooks = HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !hooks.iter().any(|known| std::ptr::fn_addr_eq(*known, hook)) {
            hooks.push(hook);
        }
    }

    /// The diagnostics every registered hook reports on `definition` in `dir`.
    pub fn run(definition: &Definition, dir: &Path) -> Vec<Diagnostic> {
        let hooks = HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        hooks
            .iter()
            .flat_map(|hook| hook(definition, dir))
            .collect()
    }
}

/// Resolves the layers fresh and checks the definition named `name`, or every resolved definition
/// when `name` is `None`. Duplicate names and unreadable directories are errors of the report.
pub fn validate_definitions(
    dirs: &LayerDirs,
    name: Option<&str>,
) -> Result<Report, DefinitionError> {
    validate_definitions_in(dirs, name, &ValidationContext::default())
}

/// [`validate_definitions`] under `context`, such as the settings' [`ValidationContext`].
pub fn validate_definitions_in(
    dirs: &LayerDirs,
    name: Option<&str>,
    context: &ValidationContext,
) -> Result<Report, DefinitionError> {
    let check = |resolved: &Resolved| {
        let (mut report, definition) = check_parsed_in(resolved, context.clone(), |_, _| {});
        if let Some(definition) = definition {
            report
                .diagnostics
                .extend(ValidationHooks::run(&definition, &resolved.dir));
            report.diagnostics.sort_by_key(|diagnostic| diagnostic.line);
        }
        report
    };
    let resolution = layers::resolve(dirs);
    let mut report = Report::default();
    match name {
        Some(name) => {
            let resolved = resolution
                .get(name)
                .ok_or_else(|| DefinitionError::NotFound(name.to_string()))?;
            report.definitions.push(check(resolved));
        }
        None => {
            report.resolution_errors.extend(
                resolution
                    .duplicates
                    .iter()
                    .map(|duplicate| format!("{}: {}", duplicate.name, duplicate.message())),
            );
            report
                .resolution_errors
                .extend(resolution.problems.iter().map(|problem| {
                    format!(
                        "{} ({}): {}",
                        problem.dir.display(),
                        problem.layer.label(),
                        problem.message
                    )
                }));
            report
                .definitions
                .extend(resolution.resolved.iter().map(check));
        }
    }
    Ok(report)
}

/// Finds the definition to open: the one called `name`, or with no name the first by name.
pub fn find_definition(dirs: &LayerDirs, name: Option<&str>) -> Result<Resolved, DefinitionError> {
    let resolution = layers::resolve(dirs);
    match name {
        Some(name) => resolution
            .get(name)
            .cloned()
            .ok_or_else(|| DefinitionError::NotFound(name.to_string())),
        None => resolution
            .resolved
            .into_iter()
            .next()
            .ok_or(DefinitionError::NoDefinitions),
    }
}

/// The path of `workflow.toml` of the definition `name` (or the first by name).
pub fn definition_path(dirs: &LayerDirs, name: Option<&str>) -> Result<PathBuf, DefinitionError> {
    find_definition(dirs, name).map(|resolved| resolved.dir.join(DEFINITION_FILE))
}

/// The minimal template: one agent node that reads the task's goal. `name` is a TOML string
/// literal, already quoted.
fn minimal_template(quoted_name: &str) -> String {
    format!(
        "schema = 1\n\
         name = {quoted_name}\n\
         description = \"Describe what this workflow does\"\n\
         start = \"work\"\n\
         \n\
         [inputs]\n\
         goal = {{ type = \"text\", from = \"task.goal\" }}\n\
         \n\
         [nodes.work]\n\
         kind = \"agent\"\n\
         purpose = \"Do the work\"\n\
         prompt = \"{TEMPLATE_PROMPT_FILE}\"\n\
         output = {{ summary = {{ type = \"text\" }} }}\n\
         routes = [{{ to = \"done\" }}]\n\
         \n\
         [ends.done]\n\
         kind = \"success\"\n"
    )
}

/// The prompt of the minimal template.
const MINIMAL_PROMPT: &str = "{{inputs.goal}}\n";

/// `text` with its top-level `name = ...` line replaced by `name = <quoted_name>`.
fn with_name(text: &str, quoted_name: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut top_level = true;
    let mut renamed = false;
    for line in text.split_inclusive('\n') {
        top_level &= !line.starts_with('[');
        if top_level && !renamed && line.starts_with("name = ") {
            output.push_str(&format!("name = {quoted_name}\n"));
            renamed = true;
        } else {
            output.push_str(line);
        }
    }
    output
}

/// A definition [`new_definition`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDefinition {
    pub name: String,
    pub dir: PathBuf,
    /// The `workflow.toml` to open.
    pub file: PathBuf,
}

fn name_is_usable(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_LEN
        && !name.chars().any(char::is_control)
        && name
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
}

/// The first of `new-workflow`, `new-workflow-2`, ... that names no definition and no directory
/// in `profile`.
pub fn next_free_name(profile: &Path) -> String {
    let taken = layers::resolve(&LayerDirs {
        profile: Some(profile.to_path_buf()),
        ..LayerDirs::default()
    });
    let free = |name: &str| {
        taken.get(name).is_none()
            && !taken
                .duplicates
                .iter()
                .any(|duplicate| duplicate.name == name)
            && !profile.join(slugify(name)).exists()
    };
    if free(DEFAULT_NAME) {
        return DEFAULT_NAME.to_string();
    }
    (2..)
        .map(|number| format!("{DEFAULT_NAME}-{number}"))
        .find(|name| free(name))
        .expect("an unbounded range has a free name")
}

/// Creates a definition from the minimal template in a fresh directory of the profile layer, named
/// from the workflow name (`name`, or the first free `new-workflow`). It refuses an unusable name,
/// a name a profile definition already has and an existing directory, and never overwrites.
pub fn new_definition(
    profile: Option<&Path>,
    name: Option<&str>,
) -> Result<NewDefinition, DefinitionError> {
    new_definition_from(profile, name, None)
}

/// [`new_definition`] starting from a shipped definition instead of the minimal template: every
/// file of `template` is copied into the new directory and the workflow takes `name`, or with no
/// name the template's own name. A copy with the template's name shadows the shipped definition
/// (the profile layer wins), which is how a shipped workflow is customized.
pub fn new_definition_from(
    profile: Option<&Path>,
    name: Option<&str>,
    template: Option<&Template>,
) -> Result<NewDefinition, DefinitionError> {
    let profile = profile.ok_or(DefinitionError::NoProfileDir)?;
    let name = match (name, template) {
        (Some(name), _) => name.trim().to_string(),
        (None, Some(template)) => template.name.to_string(),
        (None, None) => next_free_name(profile),
    };
    if !name_is_usable(&name) {
        return Err(DefinitionError::InvalidName(name));
    }
    let existing = layers::resolve(&LayerDirs {
        profile: Some(profile.to_path_buf()),
        ..LayerDirs::default()
    });
    if let Some(resolved) = existing.get(&name) {
        return Err(DefinitionError::NameTaken(name, resolved.dir.clone()));
    }
    if let Some(duplicate) = existing
        .duplicates
        .iter()
        .find(|duplicate| duplicate.name == name)
    {
        return Err(DefinitionError::NameTaken(name, duplicate.dirs[0].clone()));
    }

    let dir = profile.join(slugify(&name));
    fs::create_dir_all(profile).map_err(|error| DefinitionError::Io(profile.into(), error))?;
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(DefinitionError::DirectoryExists(dir));
        }
        Err(error) => return Err(DefinitionError::Io(dir, error)),
    }
    let file = dir.join(DEFINITION_FILE);
    let quoted = toml::Value::String(name.clone()).to_string();
    let files: Vec<(&str, String)> = match template {
        None => vec![
            (DEFINITION_FILE, minimal_template(&quoted)),
            (TEMPLATE_PROMPT_FILE, MINIMAL_PROMPT.to_string()),
        ],
        Some(template) => shipped::files_of(template)
            .map(|(relative, contents)| {
                if relative == DEFINITION_FILE {
                    (relative, with_name(contents, &quoted))
                } else {
                    (relative, contents.to_string())
                }
            })
            .collect(),
    };
    let written = files
        .iter()
        .try_for_each(|(relative, contents)| write_new(&dir.join(relative), contents));
    if let Err(error) = written {
        // The directory is ours, made a moment ago, so removing it leaves nothing behind.
        fs::remove_dir_all(&dir).ok();
        return Err(error);
    }
    Ok(NewDefinition { name, dir, file })
}
