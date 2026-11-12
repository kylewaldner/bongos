//! Workflow definitions on disk (D2): where the three layers live, materializing the shipped set,
//! checking definitions and creating a new one from the built-in minimal template.
//!
//! Everything here is synchronous and takes its directories as parameters, so tests run over a
//! temporary directory and nothing reaches the real home. Nothing calls into this module while
//! `workflows.enabled` is false.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};

use bongos_work_model::layout::{WORKFLOWS_DIR, project_dir};
use bongos_work_model::slug::slugify;
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

/// The shipped definitions. Empty until step 20 adds the shipped templates.
pub const SHIPPED_FILES: &[ShippedFile] = &[];

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

/// `<work root>/projects/<slug>/workflows`, the project layer of the project `slug`.
pub fn project_layer_dir(work_root: &Path, project_slug: &str) -> PathBuf {
    project_dir(work_root, project_slug).join(WORKFLOWS_DIR)
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
            return report;
        }
    };
    let definition = match parse(&text) {
        Ok(definition) => definition,
        Err(error) => {
            report.load_error = Some(error.to_string());
            return report;
        }
    };
    let mut context = ValidationContext {
        files: list_files(&resolved.dir),
        ..ValidationContext::default()
    };
    adjust(&definition, &mut context);
    report.warnings = definition.warnings();
    report.diagnostics = validate(&definition, &context);
    report
}

/// Resolves the layers fresh and checks the definition named `name`, or every resolved definition
/// when `name` is `None`. Duplicate names and unreadable directories are errors of the report.
pub fn validate_definitions(
    dirs: &LayerDirs,
    name: Option<&str>,
) -> Result<Report, DefinitionError> {
    let resolution = layers::resolve(dirs);
    let mut report = Report::default();
    match name {
        Some(name) => {
            let resolved = resolution
                .get(name)
                .ok_or_else(|| DefinitionError::NotFound(name.to_string()))?;
            report.definitions.push(check_definition(resolved));
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
                .extend(resolution.resolved.iter().map(check_definition));
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
    let profile = profile.ok_or(DefinitionError::NoProfileDir)?;
    let name = match name {
        Some(name) => name.trim().to_string(),
        None => next_free_name(profile),
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
    let written = write_new(&file, &minimal_template(&quoted))
        .and_then(|()| write_new(&dir.join(TEMPLATE_PROMPT_FILE), MINIMAL_PROMPT));
    if let Err(error) = written {
        // The directory is ours, made a moment ago, so removing it leaves nothing behind.
        fs::remove_dir_all(&dir).ok();
        return Err(error);
    }
    Ok(NewDefinition { name, dir, file })
}
