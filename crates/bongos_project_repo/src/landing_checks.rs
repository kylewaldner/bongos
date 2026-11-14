//! Landing checks other features register (D12): the checks that judge a change, the amending
//! checks that may write into it before it lands, the kinds a project's `[[landing.checks]]`
//! entries are parsed by, the folders under `definitions/` check 1 knows, and the live sources
//! that make a task started. Each registry is a global the app fills from the features' `init`;
//! a landing started with an app reads them, and a project can add its own for one process.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bongos_project_repo_model::checks::{
    AmendOutcome, CheckOutcome, CheckPhase, LiveSource, Stamp,
};
use bongos_project_repo_model::{ChangeMeta, ChangeSummary, Problem, RelPath};
use bongos_work_model::ProjectFile;
use futures::future::BoxFuture;
use gpui::{App, Global};

use crate::create::ProjectRepo;
use crate::git::{CommitId, TreeId};

/// What a landing check reads. `root` is the materialized tree under check, read-only, and stays
/// valid until the check's `run` future completes; `master_root` is the tree of master the change
/// is based on, materialized the same way (a `script` check runs master's copy of its script).
pub struct CheckInput<'a> {
    pub project: &'a ProjectFile,
    pub master: CommitId,
    pub tree: TreeId,
    pub root: &'a Path,
    pub master_root: &'a Path,
    pub summary: &'a ChangeSummary,
    pub meta: &'a ChangeMeta,
    pub live: &'a bongos_project_repo_model::checks::LiveFacts,
}

/// A check a landing runs on the change's tree.
pub trait LandingCheck: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn phase(&self) -> CheckPhase;

    fn applies(&self, _summary: &ChangeSummary) -> bool {
        true
    }

    /// What its result is stored under besides the tree; a different stamp reruns it.
    fn stamp(&self, input: &CheckInput) -> Stamp {
        Stamp::tree(input.tree.0.clone())
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome>;
}

/// A check that may write into the change before it lands (feature 52's rebalancer). Only Rust
/// code registers one; no `[[landing.checks]]` entry can become one. The landing engine commits
/// what it writes, inside the bounds of an amendment, as an amend commit on the change's branch.
pub trait AmendingCheck: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn applies(&self, _summary: &ChangeSummary) -> bool {
        true
    }

    /// Recomputed in the queue: a difference sends the change back to have it run again.
    fn stamp(&self, input: &CheckInput) -> Stamp {
        Stamp::tree(input.tree.0.clone())
    }

    /// Files under `definitions/` it may write besides task files.
    fn stamp_paths(&self) -> Vec<RelPath> {
        Vec::new()
    }

    /// Looks at the change in `input` and may change the files of the checkout at `writable`.
    fn amend(&self, input: CheckInput, writable: &Path) -> BoxFuture<'static, AmendOutcome>;
}

/// Parses the value a `[[landing.checks]]` entry's `run` table holds under the kind's name.
pub type CheckKindParser = fn(&toml::Value) -> Result<Arc<dyn LandingCheck>, String>;

/// Validates one file of a registered definition folder: its path relative to `definitions/`
/// and its text.
pub type FolderValidator = fn(&Path, &str) -> Vec<Problem>;

/// Whether a todo's workflow resolves in the user's or the shipped workflows (the project's own
/// `workflows/` folder is read from the tree).
pub type WorkflowResolver = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Everything registered for landings.
#[derive(Clone, Default)]
pub struct CheckRegistry {
    pub(crate) checks: Vec<Arc<dyn LandingCheck>>,
    pub(crate) amending: Vec<Arc<dyn AmendingCheck>>,
    pub(crate) kinds: BTreeMap<String, CheckKindParser>,
    pub(crate) folders: Vec<(String, FolderValidator)>,
    pub(crate) live: Vec<Arc<dyn LiveSource>>,
    pub(crate) workflows: Option<WorkflowResolver>,
}

impl CheckRegistry {
    /// `self` with what `more` registered added; a kind or resolver of `more` wins.
    pub(crate) fn merged(mut self, more: &CheckRegistry) -> Self {
        self.checks.extend(more.checks.iter().cloned());
        self.amending.extend(more.amending.iter().cloned());
        self.kinds.extend(
            more.kinds
                .iter()
                .map(|(name, parse)| (name.clone(), *parse)),
        );
        self.folders.extend(more.folders.iter().cloned());
        self.live.extend(more.live.iter().cloned());
        if more.workflows.is_some() {
            self.workflows = more.workflows.clone();
        }
        self
    }

    /// The parser of `kind`: a registered one, or the `script` kind this crate ships.
    pub(crate) fn kind(&self, kind: &str) -> Option<CheckKindParser> {
        self.kinds.get(kind).copied().or_else(|| {
            (kind == crate::script_check::SCRIPT_KIND)
                .then_some(crate::script_check::parse_script as CheckKindParser)
        })
    }
}

#[derive(Default)]
struct GlobalCheckRegistry(CheckRegistry);

impl Global for GlobalCheckRegistry {}

fn global_mut(cx: &mut App) -> &mut CheckRegistry {
    &mut cx.default_global::<GlobalCheckRegistry>().0
}

/// The checks registered in the app.
pub fn registered_checks(cx: &App) -> CheckRegistry {
    cx.try_global::<GlobalCheckRegistry>()
        .map(|global| global.0.clone())
        .unwrap_or_default()
}

/// The registry of landing checks and amending checks.
pub struct LandingChecks;

impl LandingChecks {
    pub fn register(check: Arc<dyn LandingCheck>, cx: &mut App) {
        global_mut(cx).checks.push(check);
    }

    pub fn register_amending(check: Arc<dyn AmendingCheck>, cx: &mut App) {
        global_mut(cx).amending.push(check);
    }

    /// Resolves a todo's workflow in the user's and the shipped workflows for check 4.
    pub fn register_workflow_resolver(resolver: WorkflowResolver, cx: &mut App) {
        global_mut(cx).workflows = Some(resolver);
    }
}

/// The kinds of check a `[[landing.checks]]` entry's `run` names. `script` is built in.
pub struct CheckKinds;

impl CheckKinds {
    pub fn register(kind_name: &str, parse: CheckKindParser, cx: &mut App) {
        global_mut(cx).kinds.insert(kind_name.to_string(), parse);
    }
}

/// Folders under `definitions/` that other features own, with the validator check 1 runs over
/// each of their files.
pub struct DefinitionFolders;

impl DefinitionFolders {
    pub fn register(name: &str, validator: FolderValidator, cx: &mut App) {
        global_mut(cx).folders.push((name.to_string(), validator));
    }
}

/// The sources of "started" besides the state store (active runs, claims, lane ownership).
pub struct LiveSources;

impl LiveSources {
    pub fn register(source: Arc<dyn LiveSource>, cx: &mut App) {
        global_mut(cx).live.push(source);
    }
}

impl ProjectRepo {
    /// Makes this project's landings use the checks registered in the app (besides those added
    /// to the project itself).
    pub fn use_registered_checks(&self, cx: &App) {
        *self.engine.registered.lock() = registered_checks(cx);
    }

    /// The checks a landing of this project runs now.
    pub(crate) fn check_registry(&self) -> CheckRegistry {
        let registered = self.engine.registered.lock().clone();
        registered.merged(&self.engine.local.lock())
    }

    /// Adds a landing check to this project's landings only.
    pub fn add_landing_check(&self, check: Arc<dyn LandingCheck>) {
        self.engine.local.lock().checks.push(check);
    }

    /// Adds an amending check to this project's landings only.
    pub fn add_amending_check(&self, check: Arc<dyn AmendingCheck>) {
        self.engine.local.lock().amending.push(check);
    }

    /// Adds a check kind to this project's landings only.
    pub fn add_check_kind(&self, kind_name: &str, parse: CheckKindParser) {
        self.engine
            .local
            .lock()
            .kinds
            .insert(kind_name.to_string(), parse);
    }

    /// Adds a definition folder to this project's landings only.
    pub fn add_definition_folder(&self, name: &str, validator: FolderValidator) {
        self.engine
            .local
            .lock()
            .folders
            .push((name.to_string(), validator));
    }

    /// Adds a live source to this project's landings only.
    pub fn add_live_source(&self, source: Arc<dyn LiveSource>) {
        self.engine.local.lock().live.push(source);
    }
}
