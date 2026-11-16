//! The `WorkflowEngine` global: the definition layers, the live runs and the commands that work
//! on them. It exists only once workflows are on: [`init`](crate::init) creates it when
//! `workflows.enabled` is already true, and a command creates it on first use. While
//! `workflows.enabled` is false nothing creates it, so nothing is read, written or materialized.
//!
//! The run commands are methods here that features 27, 19 and 31 can call too. Each reads
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
use bongos_project_repo::{ActivitySources, LiveSources, ProjectRepo, SnapshotReason};
use bongos_work::{ActiveRun, LiveModesRegistry, NeedsYouRegistry};
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::layout::WORKFLOWS_DIR;
use bongos_work_model::now;
use bongos_work_model::store::{Task, WorkTree};
use bongos_workflow_model::definition::{AgentKind, Outcome};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{StartedBy, TodoRef};
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{
    Definition, Failure, FailureReason, Layer, LayerDirs, NodeKey, RunState,
};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Global};
use serde_json::Value;
use settings::Settings as _;

use crate::WorkflowsSettings;
use crate::completion::CompletionHooks;
use crate::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use crate::definitions::{
    self, DefinitionError, NewDefinition, ProjectLayer, Report, SHIPPED_FILES, materialize,
    profile_dir, project_layer_in, shipped_dir,
};
use crate::landing::{LandingRunEnd, LandingRuns, set_landing_runs};
use crate::lifecycle::{
    GitWorktrees, HaltWorktrees, Lifecycle, Refusal, RefusalKind, RunId, RunStatus, StartChecks,
    StartContext, StartRequest, WorkflowEvent,
};
use crate::outcomes::{self, Delivered, Reported, RunOutcome};
use crate::process::ProcessRecord;
use crate::project_sources::{self, RunProjects, WorkHolds, WorkflowActivity, WorkflowLiveSource};
use crate::runner_program::Interpreter;
use crate::runtime::{Finish, Runner, Settlement};
use crate::shipped::Template;
use crate::task_link::{self, Completed, Finale, Next, StoreWrites, TaskWrites, WriteCommand};
use crate::worktrees::{Git, RunWorktree, TaskCheckout, branch_worktree_path};

/// The runner the engine's runs use.
pub type EngineRunner = Box<dyn Runner + Send>;

/// The lifecycle the engine shares with its live-modes source.
pub type SharedLifecycle = Arc<Mutex<Lifecycle<EngineRunner>>>;

/// Merges a run branch into its task branch (the final merge of a definition with no merge
/// node): the shipped merge with `git` in the app; tests script it. The task branch's new head.
pub type Merger = Arc<dyn Fn(&MergeRequest) -> Result<String, MergeRefusal> + Send + Sync>;

/// What `workflow: edit definition` opens.
pub enum EditTarget {
    /// The `workflow.toml` of a definition of the profile or old-format project layer.
    File(PathBuf),
    /// A definition of a project repository: its folder under `definitions/` (`workflows/<dir>`),
    /// opened in a new working copy of the repository.
    WorkingCopy { repo: ProjectRepo, folder: PathBuf },
}

/// The layers one command reads, with the commit its project layer came from and the guard
/// that keeps that layer on disk.
pub struct TaskDirs {
    pub dirs: LayerDirs,
    /// The commit of the project repository the project layer was read from; `None` when the
    /// project layer is not a repository's.
    pub commit: Option<String>,
    /// The project repository of the task, for a task of a new-format project.
    pub repo: Option<ProjectRepo>,
    _layer: Option<ProjectLayer>,
}

/// The layers of definitions, resolved fresh by every command, and the live runs. Cheap to clone
/// (the runs are shared), so a command takes a copy to its background task.
#[derive(Clone)]
pub struct WorkflowEngine {
    dirs: LayerDirs,
    runs: SharedLifecycle,
    /// Where the runs' task changes are written (03's work store in the app).
    writes: Arc<dyn TaskWrites>,
    merger: Merger,
    /// The project repository of each run started in a new-format project.
    projects: RunProjects,
    /// The projects a restore holds new work back in.
    holds: WorkHolds,
    /// The runs a landing check started and waits for.
    landing_waiters: LandingWaiters,
}

type LandingWaiters = Arc<Mutex<BTreeMap<RunId, futures::channel::oneshot::Sender<LandingRunEnd>>>>;

fn git_merger() -> Merger {
    Arc::new(|request| {
        futures::executor::block_on(builtin_merge::merge(&Git::new("git"), request)).map(|merged| {
            match merged {
                Merged::FastForward { head } | Merged::MergeCommit { head } => head,
            }
        })
    })
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
    fn is_connected(&self) -> bool {
        false
    }

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
    /// The engine holds a succeeded end for its task work (outcomes, the final merge, the todo:
    /// [`crate::task_link`]), so the lifecycle's runtime is set to hold it.
    pub fn with_lifecycle(dirs: LayerDirs, mut lifecycle: Lifecycle<EngineRunner>) -> Self {
        lifecycle.runtime_mut().hold_success(true);
        Self {
            dirs,
            runs: Arc::new(Mutex::new(lifecycle)),
            writes: Arc::new(StoreWrites),
            merger: git_merger(),
            projects: Arc::default(),
            holds: WorkHolds::default(),
            landing_waiters: Arc::default(),
        }
    }

    /// The same engine writing task changes through `writes`.
    pub fn with_task_writes(mut self, writes: Arc<dyn TaskWrites>) -> Self {
        self.writes = writes;
        self
    }

    /// The same engine merging with `merger`.
    pub fn with_merger(mut self, merger: Merger) -> Self {
        self.merger = merger;
        self
    }

    /// The engine of the running app: the shipped directory of this version (written when
    /// missing, empty set for now), the profile directory under the home directory. The project
    /// layer of a new-format task is read per command ([`WorkflowEngine::dirs_for`]); the one set
    /// with [`WorkflowEngine::with_project`] is that of an old-format project.
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
        engine.register_project_sources(cx);
        engine.install_landing_runs(cx);
        crate::gates::register(&engine, cx);
        crate::run_view::observe_focus(&engine, cx);
        Some(engine)
    }

    pub fn dirs(&self) -> &LayerDirs {
        &self.dirs
    }

    /// The layers a command on `task` reads: the engine's own, with the project layer of a
    /// new-format task read from its project's verified commit. The layer stays materialized
    /// until the result is dropped. An old-format task, or none, gets the engine's layers as
    /// they are.
    pub fn dirs_for(&self, task: Option<&Task>, cx: &App) -> Result<TaskDirs, Refusal> {
        let mut dirs = self.dirs.clone();
        let repo = task.and_then(|task| bongos_work::project_repo_of(task, cx));
        let layer = match repo
            .as_ref()
            .map(|repo| project_layer_in(repo, None))
            .transpose()
        {
            Ok(layer) => layer,
            Err(error) => {
                return Err(Refusal::new(
                    RefusalKind::Io,
                    format!("Could not read the project's workflows: {error:#}."),
                    "Check the project repository, then try again.",
                ));
            }
        };
        if let Some(layer) = &layer {
            dirs.project = Some(layer.dir.clone());
        }
        Ok(TaskDirs {
            commit: layer.as_ref().map(|layer| layer.commit.0.clone()),
            dirs,
            repo,
            _layer: layer,
        })
    }

    /// [`Self::dirs_for`] the task `task_id` of the loaded work tree.
    fn dirs_for_id(&self, task_id: &str, cx: &App) -> Result<TaskDirs, Refusal> {
        let tree = self.writes.tree(cx);
        self.dirs_for(tree.as_ref().and_then(|tree| tree.task_by_id(task_id)), cx)
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

    /// [`Self::validate`] over `dirs`, such as the layers [`Self::dirs_for`] built.
    pub fn validate_in(
        &self,
        dirs: &LayerDirs,
        name: Option<&str>,
    ) -> Result<Report, DefinitionError> {
        definitions::validate_definitions(dirs, name)
    }

    /// Creates a definition in the profile layer from the minimal template, or with `template`
    /// from a shipped definition.
    pub fn new_definition(
        &self,
        name: Option<&str>,
        template: Option<&Template>,
    ) -> Result<NewDefinition, DefinitionError> {
        definitions::new_definition_from(self.dirs.profile.as_deref(), name, template)
    }

    /// The `workflow.toml` of the definition `name`, or of the first by name with no name (until
    /// the run picker exists).
    pub fn definition_path(&self, name: Option<&str>) -> Result<PathBuf, DefinitionError> {
        definitions::definition_path(&self.dirs, name)
    }

    /// What `workflow: edit definition` opens for the definition `name` (the first by name with
    /// none): its `workflow.toml`, or for a definition of a project repository's layer the
    /// repository and the definition's folder inside `definitions/`, to open in a working copy
    /// (master is never edited in place).
    pub fn edit_target(
        &self,
        name: Option<&str>,
        task: Option<&Task>,
        cx: &App,
    ) -> Result<EditTarget, String> {
        let task_dirs = self
            .dirs_for(task, cx)
            .map_err(|refusal| refusal.to_string())?;
        let resolved = definitions::find_definition(&task_dirs.dirs, name)
            .map_err(|error| error.to_string())?;
        let folder = resolved.dir.file_name().map(PathBuf::from);
        match (resolved.layer, task_dirs.repo, folder) {
            (Layer::Project, Some(repo), Some(folder)) if task_dirs.commit.is_some() => {
                Ok(EditTarget::WorkingCopy {
                    repo,
                    folder: PathBuf::from(WORKFLOWS_DIR).join(folder),
                })
            }
            _ => Ok(EditTarget::File(resolved.dir.join(DEFINITION_FILE))),
        }
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

    /// The id and the worktree path of the active run of `task_id`.
    pub fn active_run_worktree(&self, task_id: &str) -> Option<(RunId, PathBuf)> {
        let lifecycle = self.lifecycle();
        let run = lifecycle.active_run_id(task_id)?;
        let path = lifecycle.worktree(&run)?.path.clone();
        Some((run, path))
    }

    /// Starts a run on the task of `request` in the loaded work tree ([`Self::start_in`]).
    pub fn start(&self, request: StartRequest, cx: &mut App) -> Result<RunId, Refusal> {
        let tree = self.writes.tree(cx).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownTask,
                "The task tree is not loaded yet.",
                "Try again in a moment.",
            )
        })?;
        self.start_in(request, &tree, cx)
    }

    /// [`Self::start`] for a landing check's run: `reply` is registered as the run's waiter as
    /// soon as the run exists, before any event of it is handed on, so a run that ends at once
    /// still reaches the check. `reply` is left in place when the start is refused.
    fn start_waited(
        &self,
        request: StartRequest,
        reply: &mut Option<futures::channel::oneshot::Sender<LandingRunEnd>>,
        cx: &mut App,
    ) -> Result<RunId, Refusal> {
        let tree = self.writes.tree(cx).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownTask,
                "The task tree is not loaded yet.",
                "Try again in a moment.",
            )
        })?;
        self.start_with_waiter(request, &tree, reply, cx)
    }

    /// Starts a run on the task of `request` in `tree`. A run another source reports on the task
    /// (`can_start_run`) refuses it, with the run's workflow, state and node.
    pub fn start_in(
        &self,
        request: StartRequest,
        tree: &WorkTree,
        cx: &mut App,
    ) -> Result<RunId, Refusal> {
        self.start_with_waiter(request, tree, &mut None, cx)
    }

    fn start_with_waiter(
        &self,
        request: StartRequest,
        tree: &WorkTree,
        reply: &mut Option<futures::channel::oneshot::Sender<LandingRunEnd>>,
        cx: &mut App,
    ) -> Result<RunId, Refusal> {
        if !self.lifecycle().runtime().runner().is_connected() {
            return Err(Refusal::new(
                RefusalKind::RunnerNotConnected,
                "Workflow runs are not connected to an agent runner yet.",
                "Nothing was started.",
            ));
        }
        let start = LiveModesRegistry::can_start_run(&request.task_id, cx);
        let active_run = if start.allowed {
            None
        } else {
            LiveModesRegistry::live_modes(&request.task_id, cx).run
        };
        let checks = AppChecks::read(cx);
        let settings = WorkflowsSettings::get_global(cx).clone();
        // The project layer is read from the verified commit and stays materialized until the
        // run's definition has been copied into its first attempt.
        let task_dirs = self.dirs_for(tree.task_by_id(&request.task_id), cx)?;
        if let Some(repo) = &task_dirs.repo {
            self.refuse_held(repo)?;
        }
        let mut result = {
            let mut lifecycle = self.lifecycle();
            lifecycle.set_settings(settings);
            let context = StartContext {
                tree,
                active_run,
                checks: &checks,
                definition_commit: task_dirs.commit.clone(),
            };
            lifecycle.start(request, &task_dirs.dirs, &context)
        };
        if let Ok(run) = &result {
            // Both registrations come before any event of the run is handed on.
            if let Some(reply) = reply.take() {
                self.landing_waiters
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(run.clone(), reply);
            }
            if let Some(repo) = task_dirs.repo.clone() {
                self.projects
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(run.clone(), repo.clone());
                // A restore may have taken its hold while the run was starting, after the
                // check above: the run counts as work now, so it is cancelled and refused.
                if let Err(refusal) = self.refuse_held(&repo) {
                    let run = run.clone();
                    let waiter = self
                        .landing_waiters
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&run);
                    *reply = waiter.or(reply.take());
                    self.lifecycle().cancel(&run).ok();
                    result = Err(refusal);
                }
            }
        }
        drop(task_dirs);
        if let Ok(run) = &result {
            self.link_start(run, cx);
        }
        self.publish(cx);
        result
    }

    /// Marks the run's todo started and records `run started: <workflow>` on its task.
    fn link_start(&self, run: &str, cx: &mut App) {
        let Some((task_id, todo, workflow)) = self.run_task(run) else {
            return;
        };
        let name = workflow.clone();
        self.write_detached(
            &workflow,
            Box::new(move |tree, ctx| {
                task_link::start_change(tree, ctx, &task_id, todo.index, &name)
            }),
            cx,
        );
    }

    /// The task id, todo and workflow of `run`.
    fn run_task(&self, run: &str) -> Option<(String, TodoRef, String)> {
        let lifecycle = self.lifecycle();
        let record = lifecycle.record(run)?;
        Some((
            record.task_id.clone(),
            record.todo.clone(),
            record.workflow.clone(),
        ))
    }

    /// Writes `command` on the run's task by `workflow:<name>`, logging a refusal.
    fn write_detached(&self, workflow: &str, command: WriteCommand, cx: &mut App) {
        let ctx = WorkContext::new(task_link::by(workflow), now());
        let write = self.writes.write(ctx, command, cx);
        cx.spawn(async move |_| {
            if let Err(error) = write.await {
                log::warn!("workflows: could not record the run on its task: {error}");
            }
        })
        .detach();
    }

    /// Records an outcome feature 21 reports for `run` from `key`: the same records as an
    /// outcome field, delivered now. Resolves to what it left on the task.
    pub fn report_outcome(
        &self,
        run: &RunId,
        key: &NodeKey,
        outcome: RunOutcome,
        cx: &mut App,
    ) -> gpui::Task<Result<Delivered, String>> {
        let reported = Reported {
            key: key.clone(),
            outcome,
        };
        let Some(context) = self.settle_context(run, cx) else {
            return gpui::Task::ready(Err(format!("No run {run} is loaded.")));
        };
        let engine = self.clone();
        cx.spawn(async move |cx| engine.deliver(&context, &reported, cx).await)
    }

    pub fn pause(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.pause(run))
    }

    pub fn halt(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.act(cx, |lifecycle, _| lifecycle.halt(run))
    }

    pub fn resume(&self, run: &RunId, cx: &mut App) -> Result<(), Refusal> {
        self.refuse_held_run(run)?;
        self.act(cx, |lifecycle, _| lifecycle.resume(run))
    }

    /// Cancels `run`; its task records `run cancelled` and the todo stays open.
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
        self.refuse_held_run(run)?;
        // A retry that reads the definition again, and a restart, read the project's verified
        // commit now; any other retry reads nothing and keeps its attempt's commit.
        let task_dirs = if fresh_definition {
            self.run_dirs(run, cx)?
        } else {
            None
        };
        let (dirs, commit) = match &task_dirs {
            Some(task_dirs) => (&task_dirs.dirs, task_dirs.commit.clone()),
            None => (&self.dirs, None),
        };
        self.act(cx, |lifecycle, _| {
            lifecycle.retry(run, node, fresh_definition, dirs, commit)
        })
    }

    pub fn restart(&self, run: &RunId, cx: &mut App) -> Result<RunId, Refusal> {
        self.refuse_held_run(run)?;
        let task_dirs = self.run_dirs(run, cx)?;
        let (dirs, commit) = match &task_dirs {
            Some(task_dirs) => (&task_dirs.dirs, task_dirs.commit.clone()),
            None => (&self.dirs, None),
        };
        self.act(cx, |lifecycle, _| lifecycle.restart(run, dirs, commit))
    }

    /// Refuses work in a project a restore holds new work back in.
    fn refuse_held(&self, repo: &ProjectRepo) -> Result<(), Refusal> {
        if self.holds.is_held(&repo.location().id) {
            return Err(Refusal::new(
                RefusalKind::Held,
                "This project is being restored, so no workflow work starts in it.",
                "Try again when the restore is over.",
            ));
        }
        Ok(())
    }

    /// [`Self::refuse_held`] for the project of `run`, when it has one.
    fn refuse_held_run(&self, run: &str) -> Result<(), Refusal> {
        let repo = self
            .projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .cloned();
        match repo {
            Some(repo) => self.refuse_held(&repo),
            None => Ok(()),
        }
    }

    /// The layers `run`'s task reads now, `None` when the run is not loaded (the lifecycle then
    /// refuses the command by name).
    fn run_dirs(&self, run: &str, cx: &App) -> Result<Option<TaskDirs>, Refusal> {
        let Some((task_id, _, _)) = self.run_task(run) else {
            return Ok(None);
        };
        self.dirs_for_id(&task_id, cx).map(Some)
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
    /// A run that failed or was cancelled records it on its task; a run held at a succeeded end
    /// is settled ([`WorkflowEngine::settle_run`]).
    pub fn publish(&self, cx: &mut App) {
        let (events, settling) = {
            let mut lifecycle = self.lifecycle();
            (lifecycle.take_events(), lifecycle.take_settling())
        };
        for run in settling {
            self.settle_run(run, cx);
        }
        if events.is_empty() {
            return;
        }
        for event in &events {
            let WorkflowEvent::Changed {
                run,
                task_id,
                state,
            } = event;
            self.export_run(run, *state, cx);
            self.end_landing_wait(run, *state);
            let failure = match state {
                RunStatus::Failed => self
                    .lifecycle()
                    .state(run)
                    .and_then(|state| state.ended)
                    .map(|(_, failure)| failure),
                RunStatus::Cancelled => None,
                _ => continue,
            };
            let Some((_, _, workflow)) = self.run_task(run) else {
                continue;
            };
            let task_id = task_id.clone();
            let command: WriteCommand = match failure {
                Some(failure) => Box::new(move |tree, ctx| {
                    task_link::failed_change(tree, ctx, &task_id, failure.as_ref())
                }),
                None => Box::new(move |tree, ctx| task_link::cancelled_change(tree, ctx, &task_id)),
            };
            self.write_detached(&workflow, command, cx);
        }
        LiveModesRegistry::changed(cx);
        NeedsYouRegistry::changed(cx);
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

impl WorkflowEngine {
    /// Records the state of `run` in the project's state (the `runs.toml` of its snapshots), and
    /// takes a snapshot when the run ended (a job end).
    fn export_run(&self, run: &str, state: RunStatus, cx: &mut App) {
        let Some(repo) = self
            .projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .cloned()
        else {
            return;
        };
        let Some((task_id, _, workflow)) = self.run_task(run) else {
            return;
        };
        let row = project_sources::run_row(&task_id, &workflow, state);
        if let Err(error) = project_sources::export_run(&repo, run, row) {
            log::warn!("workflows: {error:#}");
        }
        if matches!(
            state,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        ) {
            cx.background_spawn(async move {
                if let Err(error) = repo
                    .snapshot_now(SnapshotReason::JobEnd, chrono::Utc::now())
                    .await
                {
                    log::warn!("workflows: snapshot at the end of run: {error:#}");
                }
            })
            .detach();
        }
    }

    /// Tells the landing check waiting on `run` how it ended.
    fn end_landing_wait(&self, run: &str, state: RunStatus) {
        let end = match state {
            RunStatus::Succeeded => LandingRunEnd::Succeeded,
            RunStatus::Cancelled => LandingRunEnd::Cancelled,
            RunStatus::Failed => {
                let detail = self
                    .lifecycle()
                    .state(run)
                    .and_then(|state| state.ended)
                    .and_then(|(_, failure)| failure)
                    .map(|failure| failure.detail)
                    .unwrap_or_default();
                LandingRunEnd::Failed(detail)
            }
            _ => return,
        };
        let waiter = self
            .landing_waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(run);
        if let Some(waiter) = waiter {
            waiter.send(end).ok();
        }
    }

    /// Makes the engine start the runs of `workflow` landing checks ([`crate::landing`]): each
    /// request is started on the main thread with `StartedBy::Landing`, and the check hears how
    /// the run ends.
    pub(crate) fn install_landing_runs(&self, cx: &mut App) {
        let (requests, mut incoming) = futures::channel::mpsc::unbounded::<LandingRequest>();
        let engine = self.clone();
        cx.spawn(async move |cx| {
            while let Some(request) = incoming.next().await {
                cx.update(|cx| engine.start_landing_run(request, cx));
            }
        })
        .detach();
        set_landing_runs(Some(Arc::new(EngineLandingRuns { requests })));
        crate::landing::set_resolver_dirs(Some(self.dirs.clone()));
    }

    pub(crate) fn start_landing_run(&self, request: LandingRequest, cx: &mut App) {
        let start = StartRequest {
            workflow: Some(request.workflow),
            started_by: StartedBy::Landing,
            ..StartRequest::new(request.task_id)
        };
        let mut reply = Some(request.reply);
        if let Err(refusal) = self.start_waited(start, &mut reply, cx) {
            if let Some(reply) = reply {
                reply
                    .send(LandingRunEnd::NotStarted(refusal.to_string()))
                    .ok();
            }
        }
    }

    /// Registers what runs tell a project repository: the live source of active runs and the
    /// activity source a restore waits on.
    pub(crate) fn register_project_sources(&self, cx: &mut App) {
        LiveSources::register(Arc::new(self.live_source()), cx);
        ActivitySources::global().register(Arc::new(self.activity_source()));
    }

    pub(crate) fn live_source(&self) -> WorkflowLiveSource {
        WorkflowLiveSource {
            runs: self.runs.clone(),
        }
    }

    pub(crate) fn activity_source(&self) -> WorkflowActivity {
        WorkflowActivity {
            runs: self.runs.clone(),
            projects: self.projects.clone(),
            holds: self.holds.clone(),
        }
    }
}

/// A run a landing check asks for.
pub(crate) struct LandingRequest {
    pub(crate) workflow: String,
    pub(crate) task_id: String,
    pub(crate) reply: futures::channel::oneshot::Sender<LandingRunEnd>,
}

/// Hands the runs of landing checks to the engine's main-thread loop.
struct EngineLandingRuns {
    requests: futures::channel::mpsc::UnboundedSender<LandingRequest>,
}

impl LandingRuns for EngineLandingRuns {
    fn run(
        &self,
        workflow: &str,
        task_id: &str,
    ) -> futures::future::BoxFuture<'static, LandingRunEnd> {
        let (reply, end) = futures::channel::oneshot::channel();
        let request = LandingRequest {
            workflow: workflow.to_string(),
            task_id: task_id.to_string(),
            reply,
        };
        if self.requests.unbounded_send(request).is_err() {
            return Box::pin(std::future::ready(LandingRunEnd::NotStarted(
                "the workflow engine stopped".to_string(),
            )));
        }
        Box::pin(async move {
            end.await.unwrap_or_else(|_| {
                LandingRunEnd::NotStarted("the workflow engine stopped".to_string())
            })
        })
    }
}

/// What settling a run reads, taken before any await.
#[derive(Clone)]
struct SettleContext {
    run: RunId,
    workflow: String,
    task_id: String,
    todo: TodoRef,
    task: Option<Task>,
    definition: Definition,
    state: RunState,
    checkout: TaskCheckout,
    worktree: RunWorktree,
    attempt_dir: PathBuf,
    merge_is_ship: bool,
}

/// The file in an attempt directory recording the outcomes the engine delivered, so a run settled
/// again after a wait does not deliver them twice.
pub const DELIVERED_FILE: &str = "outcomes.json";

impl WorkflowEngine {
    fn settle_context(&self, run: &str, cx: &App) -> Option<SettleContext> {
        let (workflow, task_id, todo, definition, state, checkout, worktree, attempt_dir) = {
            let lifecycle = self.lifecycle();
            let record = lifecycle.record(run)?;
            (
                record.workflow.clone(),
                record.task_id.clone(),
                record.todo.clone(),
                lifecycle.runtime().definition(run)?.clone(),
                lifecycle.state(run)?,
                lifecycle.task_checkout(run)?.clone(),
                lifecycle.worktree(run)?.clone(),
                lifecycle.current_attempt_dir(run)?,
            )
        };
        let task = self
            .writes
            .tree(cx)
            .and_then(|tree| tree.task_by_id(&task_id).cloned());
        let merge_is_ship = task
            .as_ref()
            .is_none_or(|task| self.writes.merge_is_ship(task, cx));
        Some(SettleContext {
            run: run.to_string(),
            workflow,
            task_id,
            todo,
            task,
            definition,
            state,
            checkout,
            worktree,
            attempt_dir,
            merge_is_ship,
        })
    }

    /// Settles `run`, held at a succeeded end, in the background: final merge, deploy
    /// confirmation, outcomes and todo ([`task_link::next`]), then ends it or waits on Kyle.
    fn settle_run(&self, run: RunId, cx: &mut App) {
        let engine = self.clone();
        cx.spawn(async move |cx| {
            let settlement = engine.settlement(&run, cx).await;
            let result = cx.update(|cx| {
                let result = engine.lifecycle().settle(&run, settlement);
                engine.publish(cx);
                result
            });
            if let Err(error) = result {
                log::warn!("workflows: could not settle run {run}: {error}");
            }
        })
        .detach();
    }

    async fn settlement(&self, run: &str, cx: &mut AsyncApp) -> Settlement {
        loop {
            let Some(context) = cx.update(|cx| self.settle_context(run, cx)) else {
                return Settlement::Fail(Failure::new(
                    FailureReason::WorktreeUnavailable,
                    None,
                    "the run's task is not loaded",
                ));
            };
            let finale = Finale::read(
                &context.definition,
                context.task.as_ref(),
                &context.todo,
                context.merge_is_ship,
            );
            let merged = context
                .state
                .merged
                .iter()
                .any(|merged| merged.join == task_link::MERGE_GATE);
            let delivered = context.attempt_dir.join(DELIVERED_FILE).exists();
            match task_link::next(&finale, &context.state, merged, delivered) {
                Next::Wait { key, question } => return Settlement::Wait { key, question },
                Next::Merge => {
                    let Some(request) = merge_request(&context) else {
                        return Settlement::Fail(Failure::new(
                            FailureReason::WorktreeUnavailable,
                            None,
                            "the task has no worktree and branch to merge the run into",
                        ));
                    };
                    let merger = self.merger.clone();
                    let merged = cx.background_spawn(async move { merger(&request) }).await;
                    match merged {
                        Ok(head) => {
                            if let Err(refusal) =
                                self.lifecycle()
                                    .record_merge(run, task_link::MERGE_GATE, head)
                            {
                                return Settlement::Fail(Failure::new(
                                    FailureReason::WorktreeUnavailable,
                                    None,
                                    refusal.to_string(),
                                ));
                            }
                        }
                        Err(refusal) => {
                            let reason = match refusal {
                                MergeRefusal::Conflict(_) => FailureReason::MergeConflict,
                                _ => FailureReason::WorktreeUnavailable,
                            };
                            return Settlement::Fail(Failure::new(
                                reason,
                                None,
                                refusal.to_string(),
                            ));
                        }
                    }
                }
                Next::Deliver => {
                    if let Some(failure) = self.deliver_all(&context, cx).await {
                        return Settlement::Fail(failure);
                    }
                }
                Next::CompleteTodo => {
                    let (task_id, todo) = (context.task_id.clone(), context.todo.clone());
                    let write = cx.update(|cx| {
                        self.writes.write(
                            WorkContext::new(task_link::by(&context.workflow), now()),
                            Box::new(move |tree, ctx| {
                                task_link::complete_change(tree, ctx, &task_id, &todo)
                            }),
                            cx,
                        )
                    });
                    let result = write.await;
                    if result.is_ok() {
                        // The hooks look at the run's worktree, so the run ends only after them.
                        let hooks = cx.update(|cx| {
                            CompletionHooks::after_complete(
                                &context.task_id,
                                &context.run,
                                &context.worktree.path,
                                cx,
                            )
                        });
                        hooks.await;
                    }
                    return match task_link::completed(&context.state, &result) {
                        Completed::Done => Settlement::Succeed,
                        Completed::Wait { key, question } => Settlement::Wait { key, question },
                    };
                }
            }
        }
    }

    /// Delivers every outcome of the run, then records them delivered. The failure when one the
    /// success ends promised could not be delivered; an outcome nobody promised that fails is
    /// logged and skipped.
    async fn deliver_all(&self, context: &SettleContext, cx: &mut AsyncApp) -> Option<Failure> {
        let limits = cx.update(|_| *self.lifecycle().runtime().limits());
        let promised = outcomes::promised(&context.definition, &context.state, &limits);
        let mut delivered: Vec<Outcome> = Vec::new();
        let mut first_failure: Option<(Reported, String)> = None;
        let mut record = Vec::new();
        for reported in outcomes::collect(&context.definition, &context.state) {
            match self.deliver(context, &reported, cx).await {
                Ok(done) => {
                    delivered.push(reported.outcome.kind());
                    record.push(format!("{done:?}"));
                }
                Err(detail) => {
                    log::warn!(
                        "workflows: run {} could not deliver an outcome of {}: {detail}",
                        context.run,
                        reported.key
                    );
                    if first_failure.is_none() {
                        first_failure = Some((reported, detail));
                    }
                }
            }
        }
        if let Some(missing) = promised.iter().find(|outcome| !delivered.contains(outcome)) {
            let (key, detail) = match first_failure {
                Some((reported, detail)) if reported.outcome.kind() == *missing => {
                    (Some(reported.key), detail)
                }
                _ => (None, "no node delivered it".to_string()),
            };
            return Some(Failure::new(
                FailureReason::OutcomePromisedNotDelivered,
                key,
                detail,
            ));
        }
        let marker = context.attempt_dir.join(DELIVERED_FILE);
        let text = serde_json::to_string_pretty(&record).unwrap_or_default();
        if let Err(error) = std::fs::write(&marker, text) {
            log::warn!("workflows: could not write {}: {error}", marker.display());
        }
        None
    }

    /// Delivers one outcome.
    async fn deliver(
        &self,
        context: &SettleContext,
        reported: &Reported,
        cx: &mut AsyncApp,
    ) -> Result<Delivered, String> {
        let by = WorkContext::new(task_link::by(&context.workflow), now());
        let task_id = context.task_id.clone();
        match &reported.outcome {
            RunOutcome::Pr {
                url,
                title,
                branch,
                draft,
            } => {
                let result = outcomes::pr_result(url, title.as_deref(), branch.as_deref(), *draft);
                let write = cx.update(|cx| {
                    self.writes.write(
                        by,
                        Box::new(move |tree, ctx| {
                            outcomes::pr_change(tree, ctx, &task_id, result.clone())
                        }),
                        cx,
                    )
                });
                write.await.map_err(|error| error.to_string())?;
                Ok(Delivered::Pr { url: url.clone() })
            }
            RunOutcome::Document { path } => {
                let task = context
                    .task
                    .as_ref()
                    .ok_or("the run's task is not loaded")?;
                let root = source_root(&context.worktree, &reported.key, path);
                let source = outcomes::source_file(&root, path)?;
                let name = outcomes::copy_document(&source, &task.dir)?;
                let description = outcomes::document_description(&context.run, &reported.key.node);
                let manifest = outcomes::shared_manifest(&task.dir, &name, &description)?;
                let write = cx.update(|cx| {
                    self.writes.write(
                        by,
                        Box::new(move |tree, ctx| {
                            outcomes::manifest_change(tree, ctx, &task_id, manifest.clone())
                        }),
                        cx,
                    )
                });
                write.await.map_err(|error| error.to_string())?;
                Ok(Delivered::Document { name })
            }
            RunOutcome::Tasks { path } => {
                let task = context
                    .task
                    .as_ref()
                    .ok_or("the run's task is not loaded")?;
                let root = source_root(&context.worktree, &reported.key, path);
                let source = outcomes::source_file(&root, path)?;
                let written = outcomes::copy_proposal(task, &source, &context.workflow)?;
                Ok(Delivered::Proposal { path: written })
            }
        }
    }
}

/// The worktree an outcome path of `key` is relative to: its fan-out branch worktree while that
/// still holds the file, else the run worktree (where the join merged it).
fn source_root(worktree: &RunWorktree, key: &NodeKey, path: &str) -> PathBuf {
    if let Some((fan, index)) = key.branch.last() {
        let branch = branch_worktree_path(&worktree.run_dir, fan, *index);
        if branch.join(path).exists() {
            return branch;
        }
    }
    worktree.path.clone()
}

/// The final merge of the run into its task branch.
fn merge_request(context: &SettleContext) -> Option<MergeRequest> {
    Some(MergeRequest {
        task_worktree: context.checkout.worktree.clone()?,
        task_branch: context.checkout.branch.clone()?,
        run_branch: context.worktree.branch.clone(),
        run_id: context.run.clone(),
        workflow: context.workflow.clone(),
        todo: context.todo.name.clone(),
        style: MergeStyle::default(),
    })
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
