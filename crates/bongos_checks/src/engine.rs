//! The engine that runs a task's checks. One run per task at a time: its checks run one after
//! another in the order of the effective set, except that a check another check names in
//! `or_accepted` runs right after that check. A second request for a task whose run is live is
//! refused with the reason.
//!
//! The engine starts no agent and calls no model; it reads the task and its trees through the
//! seams of [`TaskSource`], [`RunWorktrees`] and [`TaskTrees`], and runs every check as a
//! [`CheckRun`]. Its events tell a view to redraw: [`ChecksEvent::Changed`] after any record or
//! live change, [`ChecksEvent::Waiting`] when a part queues for a pool and
//! [`ChecksEvent::Finished`] when a check has its verdict.
//!
//! Cancel stops the part that runs and runs no further check. The parts that finished stay in
//! the check's finished record, so the next run on the same key resumes after them.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use bongos_checks_model::decl;
use bongos_checks_model::evidence::{CheckState, Current, EvidenceRecord, StartedBy, TreeKind};
use bongos_checks_model::section::{Live, TaskSection, TreeState, section};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_checks_model::{Applicability, Row};
use bongos_project_repo::{MAIN_BRANCH, StateWriter};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, select_biased};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, WeakEntity,
};
use settings::Settings as _;

use crate::command_logic::dump_text;
use crate::evidence_store::{scan_records, task_prefix};
use crate::pin::{Pin, master_definitions};
use crate::pools::{CheckClock, CheckPoolsRegistry, ExecutorClock};
use crate::process::StopHandle;
use crate::run::{CheckRun, LiveState, Now};
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource, WorkTaskSource};
use crate::task_tree::{RunWorktrees, TaskTree, WorkflowRunWorktrees, resolve_task_tree};
use crate::tree_key::{key_inputs_digest, tree_key};
use crate::trees::{EnvBase, TaskTreesRegistry};

/// What a run of checks is asked for.
#[derive(Clone, Debug)]
pub struct RunChecksRequest {
    pub task_id: String,
    /// The checks to run, in the order of the task's set; `None` runs every judged check.
    pub checks: Option<Vec<String>>,
    /// Close the matching unfinished records as superseded and keep no earlier part.
    pub clean: bool,
    pub started_by: StartedBy,
}

impl RunChecksRequest {
    /// A person's request for every check of `task_id`.
    pub fn person(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            checks: None,
            clean: false,
            started_by: StartedBy::Person,
        }
    }
}

/// Why a run was not started. The text is what a command shows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{task} has no working tree; checks need one")]
    NoTree { task: String },
    #[error("{task} has no checks")]
    NoChecks { task: String },
    #[error("{task} has no check named {check}")]
    UnknownCheck { task: String, check: String },
    #[error("{task} already has checks running")]
    Running { task: String },
    #[error("{task} belongs to a project with no state store; checks need one")]
    NoState { task: String },
}

impl Refusal {
    /// The no verdict a refused run stands for: no tree is [`NoVerdictReason::TreeUnavailable`].
    pub fn no_verdict(&self) -> Option<NoVerdictReason> {
        match self {
            Self::NoTree { .. } => Some(NoVerdictReason::TreeUnavailable(self.to_string())),
            _ => None,
        }
    }
}

/// How one check of a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub check: String,
    pub verdict: Verdict,
    /// The record the verdict is in; `None` when the check could not even start.
    pub record_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Refused(Refusal),
    Ran {
        /// The checks that ended, in the order they ran.
        results: Vec<CheckResult>,
        cancelled: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub task_id: String,
    pub outcome: RunOutcome,
}

impl RunReport {
    fn refused(task_id: &str, refusal: Refusal) -> Self {
        Self {
            task_id: task_id.to_string(),
            outcome: RunOutcome::Refused(refusal),
        }
    }

    pub fn refusal(&self) -> Option<&Refusal> {
        match &self.outcome {
            RunOutcome::Refused(refusal) => Some(refusal),
            RunOutcome::Ran { .. } => None,
        }
    }

    pub fn results(&self) -> &[CheckResult] {
        match &self.outcome {
            RunOutcome::Ran { results, .. } => results,
            RunOutcome::Refused(_) => &[],
        }
    }

    pub fn cancelled(&self) -> bool {
        matches!(
            self.outcome,
            RunOutcome::Ran {
                cancelled: true,
                ..
            }
        )
    }
}

/// What a view of the checks redraws on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksEvent {
    /// A record or a live row of the task changed.
    Changed { task_id: String },
    /// Part of a check waits in line for `pool`, at `position`, 1 first.
    Waiting {
        task_id: String,
        check: String,
        pool: String,
        position: u32,
    },
    /// A check of the run has its verdict.
    Finished {
        task_id: String,
        check: String,
        verdict: Verdict,
    },
}

/// The Checks section of a task and what is live in it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksStatus {
    pub section: TaskSection,
    pub live: BTreeMap<String, Live>,
    /// Whether a run of the task is live.
    pub running: bool,
}

struct ActiveRun {
    stop: StopHandle,
    live: BTreeMap<String, Live>,
    writer: Arc<StateWriter>,
}

/// Everything a run needs, gathered before it starts.
struct Plan {
    request: RunChecksRequest,
    task: TaskChecks,
    rows: Vec<Row>,
    tree: TaskTree,
    writer: Arc<StateWriter>,
    stop: StopHandle,
    settings: TaskChecksSettings,
    pools: Option<Rc<dyn crate::pools::CheckPools>>,
    trees: Option<Rc<dyn crate::trees::TaskTrees>>,
    clock: Rc<dyn CheckClock>,
    now: Now,
}

/// What a check run tells the engine while it runs.
enum Message {
    Changed,
    Live(LiveState),
}

/// The checks the engine will run and the tree they run in.
pub(crate) struct Preflight {
    pub task: TaskChecks,
    pub rows: Vec<Row>,
    pub tree: TaskTree,
    pub writer: Arc<StateWriter>,
}

/// The rows to run, in order: every judged row, or those named, in the order of the set with a
/// check an `or_accepted` names moved to right after the check that names it. `Err` when the
/// task has no checks or `only` names a check it does not have.
pub(crate) fn plan_rows(task: &TaskChecks, only: Option<&[String]>) -> Result<Vec<Row>, Refusal> {
    let judged = task.set.judged();
    if task.set.applicability != Applicability::Checks || judged.is_empty() {
        return Err(Refusal::NoChecks {
            task: task.name.clone(),
        });
    }
    if let Some(names) = only
        && let Some(missing) = names
            .iter()
            .find(|name| !judged.iter().any(|row| &row.decl.name == *name))
    {
        return Err(Refusal::UnknownCheck {
            task: task.name.clone(),
            check: missing.clone(),
        });
    }
    let mut order: Vec<Row> = judged.to_vec();
    // Each pass moves one check; a loop of names ends after as many passes as there are rows.
    for _ in 0..order.len() {
        let moved = (0..order.len()).find_map(|at| {
            let by = order[at].decl.or_accepted.as_deref()?;
            let accepting = order.iter().position(|row| row.decl.name == by)?;
            (accepting < at).then_some((accepting, at))
        });
        let Some((accepting, at)) = moved else {
            break;
        };
        let row = order.remove(accepting);
        order.insert(at, row);
    }
    if let Some(names) = only {
        order.retain(|row| names.contains(&row.decl.name));
    }
    Ok(order)
}

/// Every record of the task, oldest key first; none when its project has no state store.
fn task_records(task: &TaskChecks) -> Vec<EvidenceRecord> {
    task.writer()
        .and_then(|writer| {
            writer
                .read(|read| scan_records(read, &task_prefix(&task.task_id)))
                .ok()
        })
        .unwrap_or_default()
}

struct GlobalChecksEngine(Entity<ChecksEngine>);

impl Global for GlobalChecksEngine {}

/// The global engine of task checks.
pub struct ChecksEngine {
    source: Rc<dyn TaskSource>,
    runs: Rc<dyn RunWorktrees>,
    clock: Rc<dyn CheckClock>,
    now: Now,
    active: BTreeMap<String, ActiveRun>,
    /// What each task's checks are now, from the last run's keys.
    current: BTreeMap<String, Current>,
}

impl EventEmitter<ChecksEvent> for ChecksEngine {}

/// The current time as RFC 3339 text.
fn wall_clock_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl ChecksEngine {
    pub fn new(
        source: Rc<dyn TaskSource>,
        runs: Rc<dyn RunWorktrees>,
        clock: Rc<dyn CheckClock>,
        now: Now,
    ) -> Self {
        Self {
            source,
            runs,
            clock,
            now,
            active: BTreeMap::new(),
            current: BTreeMap::new(),
        }
    }

    /// The engine of the app: over the Work tree, the workflow engine and the executor's clock.
    pub(crate) fn for_app(cx: &App) -> Self {
        Self::new(
            Rc::new(WorkTaskSource),
            Rc::new(WorkflowRunWorktrees),
            Rc::new(ExecutorClock(cx.background_executor().clone())),
            Arc::new(wall_clock_now),
        )
    }

    /// Makes `engine` the global one.
    pub fn install(engine: Self, cx: &mut App) -> Entity<Self> {
        let entity = cx.new(|_| engine);
        cx.set_global(GlobalChecksEngine(entity.clone()));
        entity
    }

    /// The global engine, once [`crate::init`] has run.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalChecksEngine>()
            .map(|global| global.0.clone())
    }

    /// The tree of `task`'s own repository: see [`crate::task_tree`] for the order.
    pub fn task_tree(&self, task: &TaskChecks, cx: &App) -> Option<TaskTree> {
        let trees = TaskTreesRegistry::get(cx);
        resolve_task_tree(
            &task.task_id,
            task.repo.as_deref(),
            task.worktree.as_deref(),
            self.runs.as_ref(),
            trees.as_deref(),
            cx,
        )
    }

    /// The task as the engine sees it, if it is known and has a project.
    pub fn task(&self, task_id: &str, cx: &App) -> Option<TaskChecks> {
        self.source.task(task_id, cx)
    }

    /// Whether the task has checks to run.
    pub fn has_checks(&self, task_id: &str, cx: &App) -> bool {
        self.task(task_id, cx)
            .is_some_and(|task| plan_rows(&task, None).is_ok())
    }

    /// Why a run of `only` (or every check) of `task_id` would be refused, if it would.
    pub(crate) fn preflight(
        &self,
        task_id: &str,
        only: Option<&[String]>,
        cx: &App,
    ) -> Result<Preflight, Refusal> {
        let Some(task) = self.task(task_id, cx) else {
            return Err(Refusal::NoChecks {
                task: task_id.to_string(),
            });
        };
        let rows = plan_rows(&task, only)?;
        let tree = self.task_tree(&task, cx).ok_or_else(|| Refusal::NoTree {
            task: task.name.clone(),
        })?;
        let writer = task.writer().ok_or_else(|| Refusal::NoState {
            task: task.name.clone(),
        })?;
        Ok(Preflight {
            task,
            rows,
            tree,
            writer,
        })
    }

    /// The Checks section of the task and what is live in it; `None` when the task is unknown.
    pub fn status(&self, task_id: &str, cx: &App) -> Option<ChecksStatus> {
        let task = self.task(task_id, cx)?;
        let records = task_records(&task);
        let tree = match self.task_tree(&task, cx) {
            Some(_) => TreeState::Available,
            None => TreeState::Unavailable,
        };
        let live = self
            .active
            .get(task_id)
            .map(|run| run.live.clone())
            .unwrap_or_default();
        let current = self.current.get(task_id).cloned().unwrap_or_default();
        Some(ChecksStatus {
            section: section(&task.set, &records, &current, &tree, &live),
            live,
            running: self.active.contains_key(task_id),
        })
    }

    /// The text `task: dump checks` writes; `None` when the task is unknown.
    pub fn dump(&self, task_id: &str, cx: &App) -> Option<String> {
        let task = self.task(task_id, cx)?;
        let status = self.status(task_id, cx)?;
        let current = self.current.get(task_id).cloned().unwrap_or_default();
        Some(dump_text(
            &task,
            self.task_tree(&task, cx).as_ref(),
            &status.section,
            &task_records(&task),
            &current,
        ))
    }

    /// Whether a run of the task is live.
    pub fn is_running(&self, task_id: &str) -> bool {
        self.active.contains_key(task_id)
    }

    /// Stops the run of the task: the part that runs is stopped and no further check starts.
    /// False when no run is live.
    pub fn cancel(&mut self, task_id: &str) -> bool {
        match self.active.get(task_id) {
            Some(run) => {
                run.stop.stop();
                true
            }
            None => false,
        }
    }

    /// Runs the checks of the request. The returned task resolves with the report; the run goes
    /// on if it is dropped.
    pub fn run(&mut self, request: RunChecksRequest, cx: &mut Context<Self>) -> Task<RunReport> {
        let task_id = request.task_id.clone();
        let plan = match self.plan(request, cx) {
            Ok(plan) => plan,
            Err(refusal) => return Task::ready(RunReport::refused(&task_id, refusal)),
        };
        self.active.insert(
            task_id.clone(),
            ActiveRun {
                stop: plan.stop.clone(),
                live: BTreeMap::new(),
                writer: plan.writer.clone(),
            },
        );
        cx.emit(ChecksEvent::Changed {
            task_id: task_id.clone(),
        });
        let (reply, report) = oneshot::channel();
        let id = task_id.clone();
        cx.spawn(async move |this, cx| {
            let outcome = execute(plan, &this, cx).await;
            this.update(cx, |engine, cx| {
                engine.active.remove(&id);
                cx.emit(ChecksEvent::Changed {
                    task_id: id.clone(),
                });
            })
            .ok();
            reply
                .send(RunReport {
                    task_id: id,
                    outcome,
                })
                .ok();
        })
        .detach();
        cx.background_spawn(async move {
            report.await.unwrap_or(RunReport {
                task_id,
                outcome: RunOutcome::Ran {
                    results: Vec::new(),
                    cancelled: true,
                },
            })
        })
    }

    fn plan(&self, request: RunChecksRequest, cx: &App) -> Result<Plan, Refusal> {
        let Preflight {
            task,
            rows,
            tree,
            writer,
        } = self.preflight(&request.task_id, request.checks.as_deref(), cx)?;
        if self.active.contains_key(&request.task_id) {
            return Err(Refusal::Running { task: task.name });
        }
        Ok(Plan {
            request,
            task,
            rows,
            tree,
            writer,
            stop: StopHandle::default(),
            settings: TaskChecksSettings::get_global(cx).clone(),
            pools: CheckPoolsRegistry::get(cx),
            trees: TaskTreesRegistry::get(cx),
            clock: self.clock.clone(),
            now: self.now.clone(),
        })
    }

    /// Applies what a check run said.
    fn apply(&mut self, task_id: &str, check: &str, message: Message, cx: &mut Context<Self>) {
        if let Message::Live(state) = &message {
            let live = match state {
                LiveState::Waiting { status, .. } => Some(Live::Waiting {
                    pool: status.pool.clone(),
                    place: status.position,
                    secs: self
                        .clock
                        .now()
                        .saturating_duration_since(status.since)
                        .as_secs(),
                }),
                LiveState::Running { part, parts } => Some(Live::Running {
                    part: *part,
                    of: *parts,
                    kept: self.kept_parts(task_id, check),
                }),
                LiveState::Rerun { .. } => None,
            };
            if let Some(run) = self.active.get_mut(task_id) {
                match live {
                    Some(live) => run.live.insert(check.to_string(), live),
                    None => run.live.remove(check),
                };
            }
            if let LiveState::Waiting { status, .. } = state {
                cx.emit(ChecksEvent::Waiting {
                    task_id: task_id.to_string(),
                    check: check.to_string(),
                    pool: status.pool.clone(),
                    position: status.position,
                });
            }
        }
        cx.emit(ChecksEvent::Changed {
            task_id: task_id.to_string(),
        });
    }

    /// The parts of the check's unfinished record that have ended: the ones a run keeps.
    fn kept_parts(&self, task_id: &str, check: &str) -> u32 {
        let Some(run) = self.active.get(task_id) else {
            return 0;
        };
        let records = run
            .writer
            .read(|read| scan_records(read, &task_prefix(task_id)))
            .unwrap_or_default();
        let open: Option<EvidenceRecord> = records
            .into_iter()
            .filter(|record| record.check == check && !record.is_finished())
            .max_by(|a, b| a.id.cmp(&b.id));
        open.map_or(0, |record| {
            let kept = record
                .parts
                .iter()
                .filter(|part| part.ended.is_some())
                .count();
            u32::try_from(kept).unwrap_or(u32::MAX)
        })
    }

    fn finished(
        &mut self,
        task_id: &str,
        check: &str,
        state: Option<CheckState>,
        verdict: &Verdict,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.active.get_mut(task_id) {
            run.live.remove(check);
        }
        if let Some(state) = state {
            self.current
                .entry(task_id.to_string())
                .or_default()
                .checks
                .insert(check.to_string(), state);
        }
        cx.emit(ChecksEvent::Finished {
            task_id: task_id.to_string(),
            check: check.to_string(),
            verdict: verdict.clone(),
        });
    }
}

fn start_failed(what: &str, error: impl std::fmt::Display) -> NoVerdictReason {
    NoVerdictReason::StartFailed(format!("{what}: {error}"))
}

/// Runs the rows of `plan` one after another.
async fn execute(plan: Plan, this: &WeakEntity<ChecksEngine>, cx: &mut AsyncApp) -> RunOutcome {
    let task_id = plan.request.task_id.clone();
    let mut results: Vec<CheckResult> = Vec::new();
    let pin = match Pin::materialize(&plan.task.project, MAIN_BRANCH).await {
        Ok(pin) => Arc::new(pin),
        Err(error) => {
            let reason = start_failed("pinning the definitions", format!("{error:#}"));
            for row in &plan.rows {
                let verdict = Verdict::NoVerdict(reason.clone());
                results.push(CheckResult {
                    check: row.decl.name.clone(),
                    verdict: verdict.clone(),
                    record_id: None,
                });
                this.update(cx, |engine, cx| {
                    engine.finished(&task_id, &row.decl.name, None, &verdict, cx)
                })
                .ok();
            }
            return RunOutcome::Ran {
                results,
                cancelled: false,
            };
        }
    };
    let mut cancelled = false;
    for row in &plan.rows {
        if plan.stop.is_stopped() {
            cancelled = true;
            break;
        }
        let name = row.decl.name.clone();
        let (sender, mut messages) = mpsc::unbounded();
        let prepared = prepare(&plan, row, &pin, sender).await;
        let (verdict, record_id, state) = match prepared {
            Err(reason) => (Verdict::NoVerdict(reason), None, None),
            Ok((check_run, state)) => {
                let mut running = Box::pin(check_run.run()).fuse();
                let result = loop {
                    select_biased! {
                        result = running => break result,
                        message = messages.next() => {
                            if let Some(message) = message {
                                this.update(cx, |engine, cx| {
                                    engine.apply(&task_id, &name, message, cx)
                                })
                                .ok();
                            }
                        }
                    }
                };
                while let Ok(message) = messages.try_recv() {
                    this.update(cx, |engine, cx| engine.apply(&task_id, &name, message, cx))
                        .ok();
                }
                match result {
                    Ok(record) => (record.verdict.clone(), Some(record.id), Some(state)),
                    Err(error) => (
                        Verdict::NoVerdict(start_failed("the check run", format!("{error:#}"))),
                        None,
                        Some(state),
                    ),
                }
            }
        };
        this.update(cx, |engine, cx| {
            engine.finished(&task_id, &name, state, &verdict, cx)
        })
        .ok();
        let ended_cancelled = verdict == Verdict::NoVerdict(NoVerdictReason::Cancelled);
        results.push(CheckResult {
            check: name,
            verdict,
            record_id,
        });
        if ended_cancelled {
            cancelled = true;
            break;
        }
    }
    RunOutcome::Ran { results, cancelled }
}

/// The run of one check: its digest, the key of the tree, its environment and its hooks. A
/// reason when it cannot start.
async fn prepare(
    plan: &Plan,
    row: &Row,
    pin: &Arc<Pin>,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<(CheckRun, CheckState), NoVerdictReason> {
    let task = &plan.task;
    let decl = row.decl.clone();
    let mut files = BTreeMap::new();
    for path in decl.named_files() {
        let resolved = pin
            .resolve(path)
            .map_err(|error| start_failed("reading the check's files", error))?;
        let bytes = std::fs::read(&resolved)
            .map_err(|error| start_failed(&format!("reading {path}"), error))?;
        files.insert(path.to_string(), bytes);
    }
    let digest =
        decl::digest(&decl, &files).map_err(|error| start_failed("digesting the check", error))?;
    let git = task.project.git().clone();
    let key = tree_key(&git, &plan.tree.path, &task.index_dir)
        .await
        .map_err(|error| NoVerdictReason::TreeUnavailable(error.to_string()))?;
    let inputs: Vec<_> = decl
        .key_inputs
        .iter()
        .map(|path| plan.tree.path.join(path))
        .collect();
    let key_inputs = key_inputs_digest(&inputs)
        .map_err(|error| start_failed("reading the key inputs", error))?;
    let state = CheckState::new(key.as_str(), &key_inputs, &digest);
    let changed = sender.clone();
    let check_run = CheckRun {
        task_id: task.task_id.clone(),
        decl,
        digest: digest.to_string(),
        pin: pin.clone(),
        master: master_definitions(&task.project),
        tree: plan.tree.path.clone(),
        tree_kind: TreeKind::Tip,
        tree_key: key.as_str().to_string(),
        key_inputs,
        git,
        base_branch: task.base_branch.clone(),
        env: EnvBase::for_tip(
            plan.trees.as_deref(),
            &task.task_id,
            task.repo.as_deref().unwrap_or_default(),
        ),
        output_dir: task.output_dir.clone(),
        evidence: None,
        params: None,
        started_by: plan.request.started_by.clone(),
        clean: plan.request.clean,
        settings: plan.settings.clone(),
        writer: plan.writer.clone(),
        stop: plan.stop.clone(),
        now: plan.now.clone(),
        on_changed: Some(Arc::new(move |_: &str| {
            changed.unbounded_send(Message::Changed).ok();
        })),
        project_id: task.project.location().id.clone(),
        // A run's worktree is only a tree here; no workflow node asked for the check.
        node: None,
        pools: plan.pools.clone(),
        clock: plan.clock.clone(),
        on_live: Some(Rc::new(move |state: &LiveState| {
            sender.unbounded_send(Message::Live(state.clone())).ok();
        })),
        #[cfg(test)]
        fail_part_end: None,
    };
    Ok((check_run, state))
}
