//! The `WorkflowEngine` global: the definition layers, the live runs and the commands that work
//! on them. It exists only once workflows are on: [`init`](crate::init) creates it when
//! `workflows.enabled` is already true, and a command creates it on first use. While
//! `workflows.enabled` is false nothing creates it, so nothing is read, written or materialized.
//!
//! The run commands (D13) are methods here that features 27, 19 and 31 can call too. Each reads
//! what the app knows first (the work tree, the live-modes registry, the agent health), then locks
//! the [`Lifecycle`], then hands the status changes on: the live-modes registry is told and every
//! observer gets a [`WorkflowEvent::Changed`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bongos_chat::model::Agent;
use bongos_chat::{ChatHealth, HealthState};
use bongos_work::{ActiveRun, LiveModesRegistry, WorkStore};
use bongos_work_model::store::WorkTree;
use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{Definition, Failure, FailureReason, LayerDirs, NodeKey};
use gpui::{App, Global};
use serde_json::Value;
use settings::Settings as _;

use crate::WorkflowsSettings;
use crate::definitions::{
    self, DefinitionError, NewDefinition, Report, SHIPPED_FILES, materialize, profile_dir,
    shipped_dir,
};
use crate::lifecycle::{
    GitWorktrees, HaltWorktrees, Lifecycle, Refusal, RefusalKind, RunId, StartChecks, StartContext,
    StartRequest, WorkflowEvent,
};
use crate::process::ProcessRecord;
use crate::runner_program::Interpreter;
use crate::runtime::{Finish, Runner};

/// The runner the engine's runs use.
pub type EngineRunner = Box<dyn Runner + Send>;

/// The lifecycle the engine shares with its live-modes source.
pub type SharedLifecycle = Arc<Mutex<Lifecycle<EngineRunner>>>;

/// The layers of definitions, resolved fresh by every command, and the live runs. Cheap to clone
/// (the runs are shared), so a command takes a copy to its background task.
#[derive(Clone)]
pub struct WorkflowEngine {
    dirs: LayerDirs,
    runs: SharedLifecycle,
}

impl fmt::Debug for WorkflowEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowEngine")
            .field("dirs", &self.dirs)
            .finish_non_exhaustive()
    }
}

impl Global for WorkflowEngine {}

/// Stands in for the node runners until the engine dispatches each node kind to its runner:
/// every start fails, naming why. Nothing ticks the app's runs before then.
struct UnwiredRunner;

impl Runner for UnwiredRunner {
    fn start(
        &mut self,
        _run: &str,
        _definition: &Definition,
        key: &NodeKey,
        _try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        Err(Failure::new(
            FailureReason::AgentNotInstalledOrLoggedOut,
            Some(key.clone()),
            "the node runners are not connected to the engine in this build",
        ))
    }

    fn poll(&mut self) -> Vec<Finish> {
        Vec::new()
    }

    fn stop(&mut self, _tries: &[(String, NodeKey)]) {}

    fn merge_branch(
        &mut self,
        _run: &str,
        _join: &str,
        _branch: &Branch,
    ) -> Result<String, Failure> {
        Err(Failure::new(
            FailureReason::MergeConflict,
            None,
            "the node runners are not connected to the engine in this build",
        ))
    }
}

/// Observers of the engine's events.
#[derive(Default)]
struct Observers(Vec<Rc<dyn Fn(&WorkflowEvent, &mut App)>>);

impl Global for Observers {}

impl WorkflowEngine {
    /// An engine over `dirs` with no runs; nothing is read or written.
    pub fn new(dirs: LayerDirs) -> Self {
        let lifecycle = Lifecycle::new(
            Box::new(UnwiredRunner) as EngineRunner,
            Box::new(GitWorktrees::system()),
            WorkflowsSettings::default(),
        );
        Self::with_lifecycle(dirs, lifecycle)
    }

    /// An engine over `dirs` driving `lifecycle` (tests give it a scripted runner).
    pub fn with_lifecycle(dirs: LayerDirs, lifecycle: Lifecycle<EngineRunner>) -> Self {
        Self {
            dirs,
            runs: Arc::new(Mutex::new(lifecycle)),
        }
    }

    /// The engine of the running app: the shipped directory of this version (written when
    /// missing, empty set for now), the profile directory under the home directory. The project
    /// layer is not wired yet; see [`WorkflowEngine::with_project`].
    fn open(cx: &App) -> Self {
        let version = release_channel::AppVersion::global(cx);
        let version = format!("{}.{}.{}", version.major, version.minor, version.patch);
        let shipped = shipped_dir(paths::data_dir(), &version);
        if let Err(error) = materialize(&shipped, SHIPPED_FILES) {
            log::warn!("workflows: could not write the shipped definitions: {error}");
        }
        Self::new(LayerDirs {
            shipped: Some(shipped),
            profile: Some(profile_dir(util::paths::home_dir())),
            project: None,
        })
    }

    /// The same engine with the project layer set to `dir`.
    pub fn with_project(mut self, dir: Option<PathBuf>) -> Self {
        self.dirs.project = dir;
        self
    }

    /// The global engine, created on first use (registering the `workflows` live-modes source).
    /// `None` while `workflows.enabled` is false, when nothing may be read or written.
    pub fn ensure(cx: &mut App) -> Option<Self> {
        if !WorkflowsSettings::get_global(cx).enabled {
            return None;
        }
        if let Some(engine) = cx.try_global::<Self>() {
            return Some(engine.clone());
        }
        let engine = Self::open(cx);
        cx.set_global(engine.clone());
        crate::sources::register(&engine, cx);
        Some(engine)
    }

    pub fn dirs(&self) -> &LayerDirs {
        &self.dirs
    }

    /// The live runs. Never held across a call into the live-modes registry, whose `workflows`
    /// source locks it too.
    pub fn lifecycle(&self) -> MutexGuard<'_, Lifecycle<EngineRunner>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Checks the definition `name`, or every definition with no name.
    pub fn validate(&self, name: Option<&str>) -> Result<Report, DefinitionError> {
        definitions::validate_definitions(&self.dirs, name)
    }

    /// Creates a definition from the minimal template in the profile layer.
    pub fn new_definition(&self, name: Option<&str>) -> Result<NewDefinition, DefinitionError> {
        definitions::new_definition(self.dirs.profile.as_deref(), name)
    }

    /// The `workflow.toml` of the definition `name`, or of the first by name with no name (until
    /// the picker of step 17).
    pub fn definition_path(&self, name: Option<&str>) -> Result<PathBuf, DefinitionError> {
        definitions::definition_path(&self.dirs, name)
    }

    /// Calls `f` with every event of the engine, for the life of the app.
    pub fn observe(cx: &mut App, f: impl Fn(&WorkflowEvent, &mut App) + 'static) {
        cx.default_global::<Observers>().0.push(Rc::new(f));
    }

    /// The active run of `task_id` as the `workflows` source reports it.
    pub fn active_run(&self, task_id: &str) -> Option<ActiveRun> {
        self.lifecycle().active_run(task_id)
    }

    /// The id of the active run of `task_id`.
    pub fn active_run_id(&self, task_id: &str) -> Option<RunId> {
        self.lifecycle().active_run_id(task_id)
    }

    /// Starts a run on the task of `request` in the loaded work tree ([`Self::start_in`]).
    pub fn start(&self, request: StartRequest, cx: &mut App) -> Result<RunId, Refusal> {
        let tree = WorkStore::global(cx)
            .and_then(|store| store.read(cx).snapshot())
            .ok_or_else(|| {
                Refusal::new(
                    RefusalKind::UnknownTask,
                    "The task tree is not loaded yet.",
                    "Try again in a moment.",
                )
            })?;
        self.start_in(request, &tree, cx)
    }

    /// Starts a run on the task of `request` in `tree`. A run another source reports on the task
    /// (`can_start_run`) refuses it, with the run's workflow, state and node.
    pub fn start_in(
        &self,
        request: StartRequest,
        tree: &WorkTree,
        cx: &mut App,
    ) -> Result<RunId, Refusal> {
        let start = LiveModesRegistry::can_start_run(&request.task_id, cx);
        let active_run = if start.allowed {
            None
        } else {
            LiveModesRegistry::live_modes(&request.task_id, cx).run
        };
        let checks = AppChecks::read(cx);
        let settings = WorkflowsSettings::get_global(cx).clone();
        let result = {
            let mut lifecycle = self.lifecycle();
            lifecycle.set_settings(settings);
            let context = StartContext {
                tree,
                active_run,
                checks: &checks,
            };
            lifecycle.start(request, &self.dirs, &context)
        };
        self.publish(cx);
        result
    }

    pub fn pause(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.pause(run))
    }

    pub fn halt(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.halt(run))
    }

    pub fn resume(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.resume(run))
    }

    pub fn cancel(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.cancel(run))
    }

    pub fn retry(
        &self,
        run: &RunId,
        node: Option<NodeKey>,
        fresh_definition: bool,
        cx: &mut App,
    ) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, dirs| {
            lifecycle.retry(run, node, fresh_definition, dirs)
        })
    }

    pub fn restart(&self, run: &RunId, cx: &mut App) -> Result<RunId, Refusal> {
        self.act(cx, |lifecycle, dirs| lifecycle.restart(run, dirs))
    }

    pub fn approve(
        &self,
        run: &RunId,
        key: &NodeKey,
        edit: Option<(String, Value)>,
        cx: &mut App,
    ) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.approve(run, key, edit))
    }

    pub fn revise(
        &self,
        run: &RunId,
        key: &NodeKey,
        comment: String,
        cx: &mut App,
    ) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.revise(run, key, comment))
    }

    /// The worktrees a halted run is edited in.
    pub fn halt_worktrees(&self, run: &RunId) -> Option<HaltWorktrees> {
        self.lifecycle().halt_worktrees(run)
    }

    /// The command that resumes the last agent session of `run`.
    pub fn resume_command(&self, run: &RunId) -> Result<String, Refusal> {
        self.lifecycle().resume_command(run, None)
    }

    /// The open gates of `run`.
    pub fn open_gates(&self, run: &RunId) -> Vec<NodeKey> {
        self.lifecycle().open_gates(run)
    }

    /// The active runs, in start order.
    pub fn active_runs(&self) -> Vec<RunId> {
        self.lifecycle().active_runs()
    }

    fn act<T>(
        &self,
        cx: &mut App,
        command: impl FnOnce(&mut Lifecycle<EngineRunner>, &LayerDirs) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let settings = WorkflowsSettings::get_global(cx).clone();
        let result = {
            let mut lifecycle = self.lifecycle();
            lifecycle.set_settings(settings);
            command(&mut lifecycle, &self.dirs)
        };
        self.publish(cx);
        result
    }

    /// Hands the lifecycle's events on: the live-modes registry recomputes, then the observers.
    pub fn publish(&self, cx: &mut App) {
        let events = self.lifecycle().take_events();
        if events.is_empty() {
            return;
        }
        LiveModesRegistry::changed(cx);
        let observers: Vec<_> = cx
            .try_global::<Observers>()
            .map(|observers| observers.0.clone())
            .unwrap_or_default();
        for event in &events {
            for observer in &observers {
                observer(event, cx);
            }
        }
    }
}

/// The start checks of the app: 04's agent health, the program interpreter lookup. MCP discovery
/// is not read yet, so the MCP check is skipped until the runners take their servers from it.
struct AppChecks {
    agents: BTreeMap<AgentKind, Result<(), String>>,
    python: Option<String>,
    env: BTreeMap<String, String>,
}

impl AppChecks {
    fn read(cx: &App) -> Self {
        let mut agents = BTreeMap::new();
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        if let Some(health) = ChatHealth::global(cx) {
            let health = health.read(cx);
            env = health.env().clone();
            for (kind, agent) in [
                (AgentKind::Claude, Agent::Claude),
                (AgentKind::Codex, Agent::Codex),
            ] {
                let health = health.health(agent);
                let verdict = match health.state {
                    HealthState::NotFound | HealthState::LoggedOut => Err(health.fix.clone()),
                    HealthState::Ready { .. } | HealthState::Unchecked => Ok(()),
                };
                agents.insert(kind, verdict);
            }
        }
        Self {
            agents,
            python: WorkflowsSettings::get_global(cx).programs.python.clone(),
            env,
        }
    }
}

impl StartChecks for AppChecks {
    fn agent(&self, kind: AgentKind) -> Result<(), String> {
        self.agents.get(&kind).cloned().unwrap_or(Ok(()))
    }

    fn interpreter(&self) -> Result<(), String> {
        Interpreter::find(self.python.as_deref(), &self.env)
            .map(|_| ())
            .map_err(|failure| failure.detail)
    }

    fn mcp_servers(&self, _kind: AgentKind) -> Option<BTreeSet<String>> {
        None
    }
}

/// Creates the global engine when `workflows.enabled` is true. Call after [`crate::init`].
pub(crate) fn init(cx: &mut App) {
    WorkflowEngine::ensure(cx);
}
