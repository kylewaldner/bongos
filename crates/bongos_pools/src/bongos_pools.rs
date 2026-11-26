//! Pools, queues and triggers for bongos, on top of `bongos_pools_model`: the `pools`, `queues`,
//! `triggers` and `hooks` settings; the `pools`, `queues` and `triggers` definition folders and
//! their validators; the lane table keys this feature reads (`pulls`, and the binding's `pools`
//! and `hooks`); and the `pool-references` landing check, which also runs in `workflow: validate`.
//!
//! Nothing is registered while `pools.enabled` is false. The first time it is true everything is
//! registered; the registries keep no way to take a folder, a check or a hook back, so while it
//! is false again they report nothing, and the lane table refuses its keys again.
//!
//! The pool stores ([`ProjectStore`], [`MachineStore`], [`transact_mixed`]) keep the tables the
//! grant rule works on. The [`PoolBroker`] of each project this process is the writer of drives
//! them; it is opened, drained and restarted by the [`PoolsParticipant`] and served to the task
//! checks as [`PoolsForChecks`], both registered once `pools.enabled` is on.

pub mod broker;
pub mod checks;
mod declare;
mod guards;
mod machine_store;
mod mixed;
mod participant;
mod project_store;
mod references;
mod settings;
mod store;

use std::path::{Component, Path};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ::settings::{Settings as _, SettingsStore};
use bongos_checks::CheckPoolsRegistry;
use bongos_engine_host::WriterParticipants;
use bongos_lanes::LaneTableFields;
use bongos_project_repo::{
    CheckInput, CheckOutcome, CheckPhase, DefinitionFolders, LandingCheck, LandingChecks,
};
use bongos_project_repo_model::Problem as FolderProblem;
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE, WORKFLOWS_DIR};
use bongos_workflow::ValidationHooks;
use bongos_workflow_model::{Definition, Diagnostic, Severity};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{App, Global};

pub use broker::{
    BrokerParts, BrokerTiming, GivenUp, HoldId, Lease, MachineStores, PoolBroker, PoolsEvent,
    Refused, ReleaseWhy, Request, StatusSource, StoreKind, WaitStatus51, Waiting,
};
pub use checks::PoolsForChecks;
pub use declare::{
    DeclDefaults, Declaration, DeclareOutcome, KindClash, PoolConfig, ProjectRef, SizeWarning,
};
pub use guards::{GuardReaders, ScriptOutput, ScriptRun, SystemGuardReaders};
pub use machine_store::{
    LOCK_FILE, LOCK_RETRY, LockBusy, LockWait, MACHINE_FILE, MACHINE_SCHEMA, MachineFile,
    MachineStore, PoolMeta, machine_store_dir,
};
pub use mixed::{
    MachineHalfUnwritten, MixedTransacted, transact_mixed, transact_mixed_in_background,
};
pub use participant::PoolsParticipant;
pub use project_store::{
    POOL_KEY_PREFIX, POOLS_NAMESPACE, ProjectPoolsReader, ProjectStore, TABLE_KEY,
    read_project_table,
};
pub use references::{
    LANE_TABLE, LANE_TABLE_KEYS, POOLS_FILE, POOLS_FOLDER, QUEUES_FILE, QUEUES_FOLDER,
    ReferenceInputs, TRIGGERS_FILE, TRIGGERS_FOLDER, WorkflowText, items_may_lack_task,
    read_inputs, reference_problems,
};
pub use settings::{HooksSettings, PoolsSettings, QueuesSettings, TriggersSettings};
pub use store::{PoolStore, ProcessProbe, StoreContext, Transacted, system_now, this_host};

/// The name of the landing check over the references between folders.
pub const POOL_REFERENCES: &str = "pool-references";

/// Whether `pools.enabled` is on now. The folder validators and the validation hook are plain
/// functions with no app, so they read it here.
static ENABLED: AtomicBool = AtomicBool::new(false);

fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// What this process registered.
#[derive(Default)]
struct Registration {
    /// Whether the folders, the landing check, the validation hook, the checks' pools and the
    /// writer participant were registered.
    registered: bool,
    /// The pools served to the checks, one broker per project this process is the writer of.
    checks: Option<Rc<PoolsForChecks>>,
}

impl Global for Registration {}

/// Registers the settings, and everything else once `pools.enabled` is on.
pub fn init(cx: &mut App) {
    PoolsSettings::register(cx);
    QueuesSettings::register(cx);
    TriggersSettings::register(cx);
    HooksSettings::register(cx);
    cx.set_global(Registration::default());
    sync_registration(cx);
    cx.observe_global::<SettingsStore>(sync_registration)
        .detach();
}

/// Whether the folders, the landing check and the validation hook are registered in this app.
pub fn is_registered(cx: &App) -> bool {
    cx.try_global::<Registration>()
        .is_some_and(|registration| registration.registered)
}

fn sync_registration(cx: &mut App) {
    let enabled = PoolsSettings::get_global(cx).enabled;
    ENABLED.store(enabled, Ordering::SeqCst);
    for key in LANE_TABLE_KEYS {
        if enabled {
            LaneTableFields::register(key);
        } else {
            LaneTableFields::unregister(key);
        }
    }
    if !enabled || is_registered(cx) {
        return;
    }
    cx.default_global::<Registration>().registered = true;
    DefinitionFolders::register(POOLS_FOLDER, validate_pools_folder, cx);
    DefinitionFolders::register(QUEUES_FOLDER, validate_queues_folder, cx);
    DefinitionFolders::register(TRIGGERS_FOLDER, validate_triggers_folder, cx);
    LandingChecks::register(Arc::new(PoolReferences), cx);
    ValidationHooks::register(workflow_reference_diagnostics);
    let checks = PoolsForChecks::new();
    CheckPoolsRegistry::set(checks.clone(), cx);
    WriterParticipants::register(Rc::new(PoolsParticipant::new(checks.clone())), cx);
    cx.default_global::<Registration>().checks = Some(checks);
}

/// The pools served to the checks, once `pools.enabled` registered them.
pub fn pools_for_checks(cx: &App) -> Option<Rc<PoolsForChecks>> {
    cx.try_global::<Registration>()
        .and_then(|registration| registration.checks.clone())
}

/// The problems of the declaration file `file` when `path` (relative to `definitions/`) is it.
fn validate_declarations(
    path: &Path,
    text: &str,
    file: &str,
    validate: fn(&str, &str) -> Vec<bongos_pools_model::Problem>,
) -> Vec<FolderProblem> {
    if !enabled() || path != Path::new(file) {
        return Vec::new();
    }
    validate(file, text)
        .into_iter()
        .map(|problem| FolderProblem {
            path: problem.file,
            reason: format!("line {}: {}", problem.line, problem.message),
        })
        .collect()
}

/// The validator of the `pools` folder: `pools/pools.toml`; guard scripts are not judged here.
pub fn validate_pools_folder(path: &Path, text: &str) -> Vec<FolderProblem> {
    validate_declarations(path, text, POOLS_FILE, bongos_pools_model::validate_pools)
}

/// The validator of the `queues` folder: `queues/queues.toml`.
pub fn validate_queues_folder(path: &Path, text: &str) -> Vec<FolderProblem> {
    validate_declarations(path, text, QUEUES_FILE, bongos_pools_model::validate_queues)
}

/// The validator of the `triggers` folder: `triggers/triggers.toml`.
pub fn validate_triggers_folder(path: &Path, text: &str) -> Vec<FolderProblem> {
    validate_declarations(
        path,
        text,
        TRIGGERS_FILE,
        bongos_pools_model::validate_triggers,
    )
}

/// The `pool-references` landing check (`InQueue`): [`reference_problems`] over the change's
/// definitions, each problem one line naming its file and line.
pub struct PoolReferences;

impl LandingCheck for PoolReferences {
    fn name(&self) -> &str {
        POOL_REFERENCES
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::InQueue
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let definitions = input.root.join(DEFINITIONS_DIR);
        let other_layers = bongos_workflow::landing::resolver_dirs();
        async move {
            if !enabled() {
                return CheckOutcome::Pass;
            }
            let inputs = read_inputs(&definitions, other_layers);
            CheckOutcome::from_refusals(
                reference_problems(&inputs)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            )
        }
        .boxed()
    }
}

/// The `definitions/` folder a project workflow folder `dir` (`<definitions>/workflows/<name>`)
/// sits in, with the workflow's label (`workflows/<name>`). `None` for a user's or shipped
/// workflow, whose folder is not in a project's definitions.
fn project_definitions(dir: &Path) -> Option<(&Path, String)> {
    let Some(Component::Normal(folder)) = dir.components().next_back() else {
        return None;
    };
    let workflows = dir.parent()?;
    if workflows.file_name()? != WORKFLOWS_DIR {
        return None;
    }
    let definitions = workflows.parent()?;
    if !definitions.join(PROJECT_FILE).is_file() {
        return None;
    }
    Some((
        definitions,
        format!("{WORKFLOWS_DIR}/{}", folder.to_string_lossy()),
    ))
}

/// The validation hook of `workflow: validate`: the reference problems found in this workflow's
/// own file (an undeclared pool in a node's `needs`, a task input or a merge node of a workflow
/// that a `pulls` entry or a trigger runs with no task), as errors on their lines.
pub fn workflow_reference_diagnostics(_definition: &Definition, dir: &Path) -> Vec<Diagnostic> {
    if !enabled() {
        return Vec::new();
    }
    let Some((definitions, label)) = project_definitions(dir) else {
        return Vec::new();
    };
    let inputs = read_inputs(definitions, bongos_workflow::landing::resolver_dirs());
    reference_problems(&inputs)
        .into_iter()
        .filter(|problem| problem.file == label)
        .map(|problem| Diagnostic {
            node: None,
            line: problem.line,
            severity: Severity::Error,
            message: problem.message,
        })
        .collect()
}

#[cfg(test)]
mod broker_tests;
#[cfg(test)]
mod checks_tests;
#[cfg(test)]
mod machine_store_tests;
#[cfg(test)]
mod mixed_tests;
#[cfg(test)]
mod pools_tests;
#[cfg(test)]
mod project_store_tests;
#[cfg(test)]
mod references_tests;
#[cfg(test)]
mod store_test_support;
