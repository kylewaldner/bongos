//! The project's `[[landing.checks]]` entries, read from master's `project.toml`, over the
//! registered checks: which built-ins, registered checks and amending checks a landing runs, and
//! the [`DeclaredCheck`] wrapper whose name, phase and applies come from the entry while its stamp
//! and run come from the check the entry names or its `run` table parses into.

use std::collections::BTreeSet;
use std::sync::Arc;

use bongos_project_repo_model::checks::{
    CANNOT_DISABLE, CheckOutcome, CheckPhase, NO_CYCLE_CHECK, NO_STATE_CHECK, REFERENCES_CHECK,
    SCHEMA_CHECK, SECRETS_CHECK, SIZE_AND_KIND_CHECK, STARTED_UNCHANGED_CHECK, Stamp,
};
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE};
use bongos_project_repo_model::{ChangeSummary, Problem};
use bongos_work_model::ProjectFile;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::landing_checks::{AmendingCheck, CheckInput, CheckRegistry, LandingCheck};

/// The built-in checks this crate runs, in their order (check 9, approval, is not here).
pub(crate) const BUILTIN_CHECKS: [&str; 7] = [
    SCHEMA_CHECK,
    NO_STATE_CHECK,
    NO_CYCLE_CHECK,
    REFERENCES_CHECK,
    STARTED_UNCHANGED_CHECK,
    SIZE_AND_KIND_CHECK,
    SECRETS_CHECK,
];

/// The kind a `workflow` entry needs; `bongos_workflow` registers it.
const WORKFLOW_KIND: &str = "workflow";

/// When a declared check applies.
#[derive(Clone)]
pub(crate) enum Applies {
    Always,
    /// When the change adds, removes or redefines a task.
    Tasks,
    /// When a changed path (from the repository root) matches one of the globs.
    Paths(GlobSet),
}

impl Applies {
    fn parse(value: &toml::Value) -> Result<Self, String> {
        match value {
            toml::Value::String(word) if word == "always" => Ok(Applies::Always),
            toml::Value::String(word) if word == "tasks" => Ok(Applies::Tasks),
            toml::Value::Table(table) => {
                let paths = table
                    .get("paths")
                    .and_then(|paths| paths.as_array())
                    .ok_or("applies must be \"always\", \"tasks\" or { paths = [...] }")?;
                let mut builder = GlobSetBuilder::new();
                for path in paths {
                    let path = path.as_str().ok_or("applies paths must be strings")?;
                    builder.add(Glob::new(path).map_err(|error| format!("{path}: {error}"))?);
                }
                builder
                    .build()
                    .map(Applies::Paths)
                    .map_err(|error| error.to_string())
            }
            _ => Err("applies must be \"always\", \"tasks\" or { paths = [...] }".to_string()),
        }
    }

    fn matches(&self, summary: &ChangeSummary) -> bool {
        match self {
            Applies::Always => true,
            Applies::Tasks => !summary.touches_no_task(),
            Applies::Paths(globs) => summary.files.iter().any(|(path, _)| {
                globs.is_match(format!("{DEFINITIONS_DIR}/{path}")) || globs.is_match(path)
            }),
        }
    }
}

/// A check as the project declared it: the entry's name, phase and applies win over the
/// check's own; its stamp and run are the check's.
pub struct DeclaredCheck {
    name: String,
    phase: Option<CheckPhase>,
    applies: Option<Applies>,
    inner: Arc<dyn LandingCheck>,
}

impl LandingCheck for DeclaredCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> CheckPhase {
        self.phase.unwrap_or_else(|| self.inner.phase())
    }

    fn applies(&self, summary: &ChangeSummary) -> bool {
        match &self.applies {
            Some(applies) => applies.matches(summary),
            None => self.inner.applies(summary),
        }
    }

    fn stamp(&self, input: &CheckInput) -> Stamp {
        self.inner.stamp(input)
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        self.inner.run(input)
    }
}

/// A declared `workflow` check while no workflow engine registered the kind.
struct NoWorkflowEngine;

impl LandingCheck for NoWorkflowEngine {
    fn name(&self) -> &str {
        WORKFLOW_KIND
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::BeforeQueue
    }

    fn run(&self, _input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        async { CheckOutcome::no_verdict("the workflow engine is not running") }.boxed()
    }
}

/// The checks a landing runs.
#[derive(Clone, Default)]
pub(crate) struct CheckPlan {
    /// The built-ins not disabled, in [`BUILTIN_CHECKS`] order.
    pub(crate) builtins: Vec<&'static str>,
    pub(crate) checks: Vec<Arc<dyn LandingCheck>>,
    pub(crate) amending: Vec<Arc<dyn AmendingCheck>>,
}

impl CheckPlan {
    pub(crate) fn checks_in(&self, phase: CheckPhase) -> Vec<Arc<dyn LandingCheck>> {
        self.checks
            .iter()
            .filter(|check| check.phase() == phase)
            .cloned()
            .collect()
    }
}

/// One `[[landing.checks]]` entry, or why it cannot be used.
struct Entry<'a> {
    name: &'a str,
    table: &'a toml::Table,
    enabled: bool,
}

fn entries(project: Option<&ProjectFile>) -> Vec<Entry<'_>> {
    let Some(entries) = project
        .and_then(|project| project.extra.get("landing"))
        .and_then(|landing| landing.get("checks"))
        .and_then(|checks| checks.as_array())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let table = entry.as_table()?;
            Some(Entry {
                name: table.get("name")?.as_str()?,
                table,
                enabled: table.get("enabled").and_then(|value| value.as_bool()) != Some(false),
            })
        })
        .collect()
}

/// The check an entry declares, or why it cannot be built.
fn declared(entry: &Entry, registry: &CheckRegistry) -> Result<Arc<dyn LandingCheck>, String> {
    let at = format!("landing check {}", entry.name);
    let phase = match entry.table.get("phase") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(CheckPhase::parse)
                .ok_or_else(|| format!("{at}: phase must be \"before-queue\" or \"in-queue\""))?,
        ),
    };
    let applies = entry
        .table
        .get("applies")
        .map(Applies::parse)
        .transpose()
        .map_err(|error| format!("{at}: {error}"))?;
    let inner = match entry.table.get("run") {
        Some(run) => {
            let table = run
                .as_table()
                .filter(|table| table.len() == 1)
                .ok_or_else(|| format!("{at}: run must name one kind, such as {{ script = ... }}"))?;
            let (kind, value) = table.iter().next().expect("one key");
            match registry.kind(kind) {
                Some(parse) => parse(value).map_err(|error| format!("{at}: {error}"))?,
                None if kind == WORKFLOW_KIND => Arc::new(NoWorkflowEngine),
                None => return Err(format!("{at}: unknown check kind {kind}")),
            }
        }
        None => registry
            .checks
            .iter()
            .find(|check| check.name() == entry.name)
            .cloned()
            .ok_or_else(|| format!("{at}: names no check bongos knows and has no run"))?,
    };
    Ok(Arc::new(DeclaredCheck {
        name: entry.name.to_string(),
        phase,
        applies,
        inner,
    }))
}

/// What `project`'s entries cannot become, for check 1: a `run` no kind parses, an unknown
/// kind, a bad phase or applies.
pub(crate) fn entry_problems(project: Option<&ProjectFile>, registry: &CheckRegistry) -> Vec<Problem> {
    let builtins: BTreeSet<&str> = BUILTIN_CHECKS.into_iter().collect();
    let amending: BTreeSet<&str> = registry.amending.iter().map(|check| check.name()).collect();
    entries(project)
        .iter()
        .filter(|entry| !builtins.contains(entry.name) && !amending.contains(entry.name))
        .filter_map(|entry| declared(entry, registry).err())
        .map(|reason| Problem {
            path: PROJECT_FILE.to_string(),
            reason,
        })
        .collect()
}

/// The checks a landing runs under master's declarations `project`: every built-in and
/// registered check unless an entry disables it (the always-on ones cannot be), every declared
/// check that parses, and the registered amending checks not disabled.
pub(crate) fn plan(project: Option<&ProjectFile>, registry: &CheckRegistry) -> CheckPlan {
    let entries = entries(project);
    let disabled: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| !entry.enabled && !CANNOT_DISABLE.contains(&entry.name))
        .map(|entry| entry.name)
        .collect();
    let builtins = BUILTIN_CHECKS
        .into_iter()
        .filter(|name| !disabled.contains(name))
        .collect();
    let amending = registry
        .amending
        .iter()
        .filter(|check| !disabled.contains(check.name()))
        .cloned()
        .collect();
    let amending_names: BTreeSet<&str> =
        registry.amending.iter().map(|check| check.name()).collect();
    let mut declared_names = BTreeSet::new();
    let mut checks: Vec<Arc<dyn LandingCheck>> = Vec::new();
    for entry in &entries {
        if BUILTIN_CHECKS.contains(&entry.name) || amending_names.contains(entry.name) {
            continue;
        }
        declared_names.insert(entry.name);
        if !entry.enabled {
            continue;
        }
        match declared(entry, registry) {
            Ok(check) => checks.push(check),
            // Check 1 refuses a change that adds such an entry; one already in master is
            // reported and skipped.
            Err(reason) => log::error!("{reason}"),
        }
    }
    for check in &registry.checks {
        if !declared_names.contains(check.name()) {
            checks.push(check.clone());
        }
    }
    CheckPlan {
        builtins,
        checks,
        amending,
    }
}
