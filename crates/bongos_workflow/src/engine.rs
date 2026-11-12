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
use bongos_work::{ActiveRun, LiveModesRegistry, NeedsYouRegistry, WorkStore};
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::now;
use bongos_work_model::store::{Task, WorkTree};
use bongos_workflow_model::definition::{AgentKind, Outcome};
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{Definition, Failure, FailureReason, LayerDirs, NodeKey, RunState};
use gpui::{App, AppContext as _, AsyncApp, Global};
use serde_json::Value;
use settings::Settings as _;

use crate::WorkflowsSettings;
use crate::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use crate::definitions::{
    self, DefinitionError, NewDefinition, Report, SHIPPED_FILES, materialize, profile_dir,
    shipped_dir,
};
use crate::lifecycle::{
    GitWorktrees, HaltWorktrees, Lifecycle, Refusal, RefusalKind, RunId, RunStatus, StartChecks,
    StartContext, StartRequest, WorkflowEvent,
};
use crate::outcomes::{self, Delivered, Reported, RunOutcome};
use crate::process::ProcessRecord;
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

/// The layers of definitions, resolved fresh by every command, and the live runs. Cheap to clone
/// (the runs are shared), so a command takes a copy to its background task.
#[derive(Clone)]
pub struct WorkflowEngine {
    dirs: LayerDirs,
    runs: SharedLifecycle,
    /// Where the runs' task changes are written (03's work store in the app).
    writes: Arc<dyn TaskWrites>,
    merger: Merger,
}

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
        crate::gates::register(&engine, cx);
        crate::run_view::observe_focus(&engine, cx);
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

    /// Records an outcome feature 21 reports for `run` from `key` (D16): the same records as an
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

    /// Delivers one outcome (D16).
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
