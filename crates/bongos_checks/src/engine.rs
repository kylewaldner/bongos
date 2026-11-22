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
//!
//! What runs now is also written to the project's state store as live rows (see
//! [`crate::live`]), throttled per task by `task_checks.live_write_ms` on the engine's clock.
//!
//! [`ChecksEngine::drain`] stops everything when the state writer moves to another process, and
//! [`ChecksEngine::undrain`] takes runs again when that move is abandoned.
//!
//! A run with parameters (items, a repeat, or the base tree) records evidence that satisfies
//! nothing. A base run checks out the merge-base of the task branch and its base branch in a
//! second tree a lane lends ([`TaskTrees::second_tree`]), whose lease the engine holds until
//! the check ends, or, when none is lent now, in a temporary tree under
//! `task_checks.scratch_dir` that is removed when the check ends, however it ends. The task's
//! own tree is never touched. Every check gets `BONGOS_CHECK_EVIDENCE`: the records of the task
//! on its tree and on its base tree, and those of the same workflow run.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_checks_model::decl::{self, CheckDecl};
use bongos_checks_model::evidence::{
    CheckState, Current, EvidenceRecord, Params, StartedBy, TreeKind,
};
use bongos_checks_model::section::{Live, TaskSection, TreeState, section};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_checks_model::{Applicability, Row};
use bongos_project_repo::{Git, MAIN_BRANCH, StateWriter};
use bongos_workflow::RunId;
use bongos_workflow_model::NodeKey;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, select_biased};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, WeakEntity,
};
use settings::Settings as _;

use crate::base_tree::{LentTreeError, TempTree, check_out_lent_tree, sweep_scratch};
use crate::changed_files::merge_base;
use crate::command_logic::dump_text;
use crate::evidence_store::{scan_records, task_prefix};
use crate::live::{Flush, LiveRow, LiveRows};
use crate::pin::{Pin, master_definitions};
use crate::pools::{CheckClock, CheckPoolsRegistry, ExecutorClock};
use crate::process::StopHandle;
use crate::run::{CheckRun, LiveState, Now, RunTree};
use crate::settings::TaskChecksSettings;
use crate::task_source::{TREE_KEYS_FOLDER, TaskChecks, TaskSource, WorkTaskSource};
use crate::task_tree::{RunWorktrees, TaskTree, WorkflowRunWorktrees, resolve_task_tree};
use crate::tree_key::{key_inputs_digest, trees_key};
use crate::trees::{EnvBase, SecondTree, SecondTreeLease, TaskTreesRegistry};

/// What a run of checks is asked for.
#[derive(Clone, Debug)]
pub struct RunChecksRequest {
    pub task_id: String,
    /// The checks to run, in the order of the task's set; `None` runs every judged check.
    pub checks: Option<Vec<String>>,
    /// Close the matching unfinished records as superseded and keep no earlier part.
    pub clean: bool,
    pub started_by: StartedBy,
    /// The workflow node (a `use = "checks"` node) the run is for; `None` for a manual run.
    pub node: Option<(RunId, NodeKey)>,
    /// `Some` for a parameterized run (items, a repeat, or the base tree), whose records
    /// satisfy nothing.
    pub params: Option<Params>,
}

impl RunChecksRequest {
    /// A person's request for every check of `task_id`.
    pub fn person(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            checks: None,
            clean: false,
            started_by: StartedBy::Person,
            node: None,
            params: None,
        }
    }

    /// The tree the run's checks run on: the base tree when its parameters say so.
    pub fn tree_kind(&self) -> TreeKind {
        self.params
            .as_ref()
            .and_then(|params| params.tree)
            .unwrap_or(TreeKind::Tip)
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
    #[error("checks of {task} cannot start while the engine moves to the host")]
    Draining { task: String },
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
    /// The engine takes runs again after a drain ([`ChecksEngine::undrain`]).
    Undrained,
}

/// The Checks section of a task and what is live in it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksStatus {
    pub section: TaskSection,
    pub live: BTreeMap<String, Live>,
    /// Whether a run of the task is live.
    pub running: bool,
}

/// The second trees a run holds now; dropping one releases it.
pub(crate) type Leases = Rc<RefCell<Vec<Box<dyn SecondTreeLease>>>>;

struct ActiveRun {
    stop: StopHandle,
    live: BTreeMap<String, Live>,
    writer: Arc<StateWriter>,
    node: Option<(RunId, NodeKey)>,
    leases: Leases,
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
    leases: Leases,
    /// The trees the checks run on instead of the task's own, when they are recorded ones.
    recorded: Option<RecordedTrees>,
}

/// Trees a run checks instead of a task's own: tree objects a tree key named, each written out
/// in a tree a lane lends or under the scratch folder by whoever starts the run (who releases
/// or removes it afterward). A check's
/// tree key is derived from the tree ids, never from the folders, and its key inputs are read
/// from the tree objects.
#[derive(Clone, Debug)]
pub(crate) struct RecordedTrees {
    /// The repository a check that names none runs in.
    pub(crate) own: String,
    pub(crate) trees: Vec<RecordedTree>,
}

/// One repository's recorded tree.
#[derive(Clone, Debug)]
pub(crate) struct RecordedTree {
    pub(crate) repo: String,
    /// A directory of the repository whose object store holds the tree.
    pub(crate) repository: PathBuf,
    pub(crate) tree: String,
    /// Where the tree is written out.
    pub(crate) path: PathBuf,
    /// The environment a check starts from there: a lent tree's, else bongos's own.
    pub(crate) env: EnvBase,
}

impl RecordedTrees {
    /// The trees of `repos` (the repository a check names, in its order), their key and the
    /// digest of `key_inputs` read from the first tree.
    pub(crate) async fn state(
        &self,
        git: &Git,
        repos: &[String],
        key_inputs: &[String],
    ) -> Result<(Vec<&RecordedTree>, String, String), NoVerdictReason> {
        let mut trees = Vec::with_capacity(repos.len());
        for repo in repos {
            let repo = if repo.is_empty() { &self.own } else { repo };
            let tree = self
                .trees
                .iter()
                .find(|tree| &tree.repo == repo)
                .ok_or_else(|| {
                    NoVerdictReason::TreeUnavailable(format!("the recorded tree has no {repo}"))
                })?;
            trees.push(tree);
        }
        let Some(first) = trees.first() else {
            return Err(NoVerdictReason::TreeUnavailable(
                "the recorded tree is empty".to_string(),
            ));
        };
        let key = recorded_key(
            trees
                .iter()
                .map(|tree| (tree.repo.as_str(), tree.tree.as_str())),
        );
        let inputs =
            crate::tree_key::key_inputs_in_tree(git, &first.repository, &first.tree, key_inputs)
                .await
                .map_err(|error| start_failed("reading the key inputs", error))?;
        Ok((trees, key, inputs))
    }
}

/// The key of recorded trees: one tree's id, or [`crate::tree_key::combine_trees`] over the
/// `(repository, tree id)` pairs.
pub(crate) fn recorded_key<'a>(trees: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let pairs: Vec<(&str, crate::tree_key::TreeKey)> = trees
        .map(|(repo, tree)| (repo, crate::tree_key::TreeKey(tree.to_string())))
        .collect();
    match pairs.as_slice() {
        [(_, key)] => key.as_str().to_string(),
        _ => crate::tree_key::combine_trees(&pairs).as_str().to_string(),
    }
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
pub(crate) fn task_records(task: &TaskChecks) -> Vec<EvidenceRecord> {
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
    pub(crate) source: Rc<dyn TaskSource>,
    runs: Rc<dyn RunWorktrees>,
    pub(crate) clock: Rc<dyn CheckClock>,
    pub(crate) now: Now,
    active: BTreeMap<String, ActiveRun>,
    /// What each task's checks are now, from the last run's keys.
    current: BTreeMap<String, Current>,
    /// The live rows in the state store, throttled.
    pub(crate) live_rows: LiveRows,
    /// Counts drains, so a write scheduled before one is dropped.
    live_epoch: u64,
    /// While true no run starts and no live row is written.
    pub(crate) drained: bool,
    /// The done decider's projects and pins.
    pub(crate) decider: crate::decider::Decider,
    /// Told when the last active run and the last landing check have ended.
    pub(crate) idle: Vec<oneshot::Sender<()>>,
    /// The worktree of each task whose workflow run just completed its todo, by task id, while
    /// the done decision taken for that completion runs ([`crate::completion`]).
    pub(crate) completing: BTreeMap<String, TaskTree>,
    /// The checks landings asked for that run now, by job number ([`crate::landing`]).
    pub(crate) landing: BTreeMap<u64, crate::landing::LandingRun>,
    pub(crate) next_landing: u64,
    /// Receives the jobs of landing checks while the engine lives.
    pub(crate) landing_jobs: Option<Task<()>>,
    /// The workflow checks keys in flight, read from each [`crate::workflow_node::ChecksNode`]
    /// over this engine: whether their checks run, or they wait (for the next repeat, behind
    /// another run of the task, or for the drain to end). A drain names them all.
    pub(crate) node_tries: Vec<NodeTries>,
}

/// Reads the `(run, key)` of a checks node's tries in flight.
pub(crate) type NodeTries = Rc<dyn Fn() -> Vec<(RunId, NodeKey)>>;

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
            live_rows: LiveRows::default(),
            live_epoch: 0,
            drained: false,
            decider: crate::decider::Decider::default(),
            idle: Vec::new(),
            completing: BTreeMap::new(),
            landing: BTreeMap::new(),
            next_landing: 0,
            landing_jobs: None,
            node_tries: Vec::new(),
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

    /// The tree of `task`'s own repository: see [`crate::task_tree`] for the order. While the
    /// decision for a run's completion runs, it is that run's worktree.
    pub fn task_tree(&self, task: &TaskChecks, cx: &App) -> Option<TaskTree> {
        if let Some(tree) = self.completing.get(&task.task_id) {
            return Some(tree.clone());
        }
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

    /// Every record of the task, oldest first; none when the task is unknown.
    pub fn records(&self, task_id: &str, cx: &App) -> Vec<EvidenceRecord> {
        self.task(task_id, cx)
            .map(|task| task_records(&task))
            .unwrap_or_default()
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

    /// The task workflow run `run` works on, as the workflow engine knows it.
    pub fn task_of_run(&self, run: &str, cx: &App) -> Option<String> {
        self.runs.task_of(run, cx)
    }

    /// The workflow node (a `use = "checks"` node) the task's live run is for; `None` when no
    /// run is live or it is a manual one.
    pub fn node_of(&self, task_id: &str) -> Option<&(RunId, NodeKey)> {
        self.active.get(task_id)?.node.as_ref()
    }

    /// The live run for workflow node `key` of run `run`: its task, and what each of its checks
    /// is doing now.
    pub fn node_live(&self, run: &str, key: &NodeKey) -> Option<(&str, &BTreeMap<String, Live>)> {
        self.active.iter().find_map(|(task_id, active)| {
            let (node_run, node_key) = active.node.as_ref()?;
            (node_run == run && node_key == key).then_some((task_id.as_str(), &active.live))
        })
    }

    /// After a relaunch: stops the processes an earlier bongos left running for the parts of
    /// workflow node `key` of run `run` (on `task_id`), found through their unfinished records.
    /// The records stay unfinished, so the node's next try resumes them with their finished
    /// parts. Nothing is stopped while this engine runs that node itself. Resolves with one
    /// outcome per part of each such record.
    pub fn kill_node_orphans(
        &self,
        task_id: &str,
        run: &str,
        key: &NodeKey,
        cx: &App,
    ) -> Task<Vec<bongos_workflow::process::OrphanOutcome>> {
        if self.node_live(run, key).is_some() {
            return Task::ready(Vec::new());
        }
        let Some(task) = self.task(task_id, cx) else {
            return Task::ready(Vec::new());
        };
        let node_key = key.to_string();
        let parts: Vec<_> = task_records(&task)
            .into_iter()
            .filter(|record| {
                !record.is_finished()
                    && matches!(&record.started_by, StartedBy::Run { run_id, node_key: started }
                        if run_id == run && *started == node_key)
            })
            .flat_map(|record| record.parts)
            .collect();
        if parts.is_empty() {
            return Task::ready(Vec::new());
        }
        cx.background_spawn(async move { crate::process::kill_orphan_parts(&parts) })
    }

    /// At load: stops the processes an earlier bongos left running for the parts of every
    /// unfinished task record in the store of `writer` (a person's run or a workflow node's), but
    /// those of a task this engine runs now. The records stay unfinished, so the next run on the
    /// same key resumes from their finished parts. Resolves with one outcome per part of each
    /// such record.
    pub fn kill_store_orphans(
        &self,
        writer: &StateWriter,
        cx: &App,
    ) -> Task<Vec<bongos_workflow::process::OrphanOutcome>> {
        let records = match writer.read(|read| scan_records(read, "")) {
            Ok(records) => records,
            Err(error) => {
                log::error!("reading the checks records to stop orphan parts: {error:#}");
                return Task::ready(Vec::new());
            }
        };
        let parts: Vec<_> = records
            .into_iter()
            .filter(|record| {
                !record.is_finished()
                    && !matches!(record.started_by, StartedBy::Landing { .. })
                    && !self.active.contains_key(&record.task_id)
            })
            .flat_map(|record| record.parts)
            .collect();
        if parts.is_empty() {
            return Task::ready(Vec::new());
        }
        cx.background_spawn(async move { crate::process::kill_orphan_parts(&parts) })
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
        match self.start(request, cx) {
            Ok(report) => report,
            Err(refusal) => Task::ready(RunReport::refused(&task_id, refusal)),
        }
    }

    /// Starts the checks of the request, or says why not. The returned task resolves with the
    /// report; the run goes on if it is dropped.
    pub fn start(
        &mut self,
        request: RunChecksRequest,
        cx: &mut Context<Self>,
    ) -> Result<Task<RunReport>, Refusal> {
        let plan = self.plan(request, cx)?;
        Ok(self.launch(plan, cx))
    }

    /// Starts the checks of the request on `recorded` trees instead of `task`'s own (whose
    /// repository is overridden by them): the closing checks of a group on its last child's
    /// tree. Refused like any run when the task has a live run or the engine is drained. The
    /// run is stopped through `stop` and holds `leases`, the second trees its recorded trees
    /// are written out in, so a drain releases them.
    pub(crate) fn start_recorded(
        &mut self,
        request: RunChecksRequest,
        task: TaskChecks,
        recorded: RecordedTrees,
        stop: StopHandle,
        leases: Leases,
        cx: &mut Context<Self>,
    ) -> Result<Task<RunReport>, Refusal> {
        let rows = plan_rows(&task, request.checks.as_deref())?;
        let writer = task.writer().ok_or_else(|| Refusal::NoState {
            task: task.name.clone(),
        })?;
        let Some(first) = recorded.trees.first() else {
            return Err(Refusal::NoTree { task: task.name });
        };
        if self.drained {
            return Err(Refusal::Draining { task: task.name });
        }
        if self.active.contains_key(&request.task_id) {
            return Err(Refusal::Running { task: task.name });
        }
        let tree = TaskTree {
            path: first.path.clone(),
            run: None,
        };
        let plan = Plan {
            request,
            task: TaskChecks {
                repo: Some(recorded.own.clone()),
                base_branch: None,
                ..task
            },
            rows,
            tree,
            writer,
            stop,
            settings: TaskChecksSettings::get_global(cx).clone(),
            pools: CheckPoolsRegistry::get(cx),
            trees: None,
            clock: self.clock.clone(),
            now: self.now.clone(),
            leases,
            recorded: Some(recorded),
        };
        Ok(self.launch(plan, cx))
    }

    fn launch(&mut self, plan: Plan, cx: &mut Context<Self>) -> Task<RunReport> {
        let task_id = plan.request.task_id.clone();
        self.live_rows.adopt(&plan.writer);
        self.active.insert(
            task_id.clone(),
            ActiveRun {
                stop: plan.stop.clone(),
                live: BTreeMap::new(),
                writer: plan.writer.clone(),
                node: plan.request.node.clone(),
                leases: plan.leases.clone(),
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
                engine.live_rows.release(&id);
                engine.tell_idle();
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

    /// Stops everything before the state writer moves to another process: no part or check
    /// starts and new runs are refused until [`Self::undrain`]; every running part is stopped as
    /// at quit (SIGTERM, then SIGKILL within `task_checks.kill_grace_seconds`), the parts that
    /// finished staying in each unfinished record so a later run on the same key resumes from
    /// them; every pool lease is dropped with its part; every second tree a base run holds is
    /// released when its check ends, and at the latest when the drain resolves (so is every second
    /// tree a group's closing run holds, while its trees are made ready or its checks run); and
    /// `checks.live` is cleared at once, with every pending or scheduled live write forgotten.
    ///
    /// Resolves once every run has ended, or once the grace has passed on the engine's clock,
    /// whichever comes first, with the workflow nodes whose runs it stopped (none for manual
    /// runs) and every other workflow checks key in flight then: one waiting for its next
    /// repeat, behind another run of its task, or to start while drained. Each such key is
    /// taken over by whoever drained (its try stops once the run records `ChecksDrained`), so a
    /// waiting checks key never holds the drain of the workflow runs. Every landing check that runs is stopped too and answered no verdict, "the engine
    /// is moving to the host", once its process has ended (so the landing keeps its trees until
    /// then), and a landing job that arrives while drained is answered so at once; the landing
    /// tries again later.
    ///
    /// Guaranteed at resolution: every run was told to stop, no run or part starts while
    /// drained, every second tree lease is released (but one whose tree git is checking out
    /// right then, which goes as soon as that git command ends, with no other after it), and
    /// `checks.live` is empty and stays so while drained (no live row is written). Not
    /// guaranteed when the grace wins: a run that
    /// has not ended yet is still active ([`Self::is_running`]), its process may still be
    /// exiting, its pool lease may still be held, its temporary tree may not be removed yet and
    /// its record may not yet hold the stopped part; those settle when the run ends.
    pub fn drain(&mut self, cx: &mut Context<Self>) -> Task<Vec<(RunId, NodeKey)>> {
        self.drained = true;
        self.live_epoch += 1;
        // At once, so an undrain before this resolves starts from no stale row and no write
        // left scheduled for a dropped timer.
        self.live_rows.clear_all();
        self.decider.forget_holds();
        let mut stopped = Vec::new();
        let mut leases = Vec::new();
        for run in self.active.values() {
            leases.push(run.leases.clone());
            run.stop.stop();
            if let Some(node) = &run.node {
                stopped.push(node.clone());
            }
        }
        // A group run still making its trees ready holds its leases outside any active run.
        for group in self.decider.group_holds.values() {
            group.stop.stop();
            leases.push(group.leases.clone());
        }
        for run in self.landing.values_mut() {
            run.moving = true;
            run.stop.stop();
        }
        let ended = if self.active.is_empty() && self.landing.is_empty() {
            None
        } else {
            let (sender, receiver) = oneshot::channel();
            self.idle.push(sender);
            Some(receiver)
        };
        let grace = Duration::from_secs(u64::from(
            TaskChecksSettings::get_global(cx).kill_grace_seconds,
        ));
        let bound = self.clock.sleep(grace);
        let node_tries = self.node_tries.clone();
        cx.spawn(async move |_, _| {
            if let Some(ended) = ended {
                let mut ended = ended.fuse();
                let mut bound = bound.fuse();
                select_biased! {
                    _ = ended => {},
                    _ = bound => {},
                }
            }
            for leases in leases {
                leases.borrow_mut().clear();
            }
            // Read now, once the runs stopped: a stopped node's try waits again to start, and
            // a key that finished meanwhile is reported as usual, so it is not named.
            for tries in &node_tries {
                for node in tries() {
                    if !stopped.contains(&node) {
                        stopped.push(node);
                    }
                }
            }
            stopped
        })
    }

    /// Removes the temporary base trees a crash left in the scratch folder: every one whose
    /// owner process is gone. Resolves with the trees removed. `git` runs the removals.
    pub fn sweep_scratch(&self, git: Git, cx: &mut Context<Self>) -> Task<Vec<PathBuf>> {
        let scratch = TaskChecksSettings::get_global(cx).scratch_folder();
        cx.background_spawn(async move {
            let index_dir = scratch.join(TREE_KEYS_FOLDER);
            sweep_scratch(&git, &scratch, &index_dir).await
        })
    }

    /// Tells those waiting for the engine to be idle, once no run and no landing check is left.
    pub(crate) fn tell_idle(&mut self) {
        if self.active.is_empty() && self.landing.is_empty() {
            for idle in self.idle.drain(..) {
                idle.send(()).ok();
            }
        }
    }

    /// Takes runs again after [`Self::drain`], when the writer stays in this process, and says so
    /// with [`ChecksEvent::Undrained`]: a workflow checks node the drain stopped starts again
    /// from its kept parts.
    pub fn undrain(&mut self, cx: &mut Context<Self>) {
        self.drained = false;
        // The drain cleared the hold rows: the decider writes them again.
        self.decider.poke_all();
        cx.emit(ChecksEvent::Undrained);
    }

    /// Whether the engine is drained.
    pub fn is_drained(&self) -> bool {
        self.drained
    }

    /// Writes `row` as the live row of `check` of `task_id`, at most once per task per
    /// `task_checks.live_write_ms`; a later row of the same task waits and replaces earlier
    /// ones.
    fn write_live(&mut self, task_id: &str, check: &str, row: LiveRow, cx: &mut Context<Self>) {
        if self.drained {
            return;
        }
        let Some(writer) = self.active.get(task_id).map(|run| run.writer.clone()) else {
            return;
        };
        let interval =
            Duration::from_millis(u64::from(TaskChecksSettings::get_global(cx).live_write_ms));
        let row = self.live_rows.next_row(task_id, check, row);
        let now = self.clock.now();
        let flush = self
            .live_rows
            .update(&writer, task_id, check, row, now, interval);
        self.schedule_live(task_id, flush, cx);
    }

    /// Writes the task's live rows now or later, as `flush` says.
    pub(crate) fn schedule_live(&mut self, task_id: &str, flush: Flush, cx: &mut Context<Self>) {
        match flush {
            Flush::Now => self.flush_live(task_id),
            Flush::After(wait) => {
                let wait = self.clock.sleep(wait);
                let epoch = self.live_epoch;
                let task_id = task_id.to_string();
                cx.spawn(async move |this, cx| {
                    wait.await;
                    this.update(cx, |engine, _| {
                        if engine.live_epoch == epoch {
                            engine.flush_live(&task_id);
                        }
                    })
                    .ok();
                })
                .detach();
            }
            Flush::Scheduled | Flush::Unchanged => {}
        }
    }

    fn flush_live(&mut self, task_id: &str) {
        if self.drained {
            return;
        }
        if let Err(error) = self.live_rows.flush(task_id, self.clock.now()) {
            log::error!("writing the live rows of checks: {error:#}");
        }
        if !self.active.contains_key(task_id) {
            self.live_rows.release(task_id);
        }
    }

    /// Whether the live rows keep anything of the task, for tests.
    #[cfg(test)]
    pub(crate) fn keeps_live_task(&self, task_id: &str) -> bool {
        self.live_rows.has_task(task_id)
    }

    /// The live rows written so far, for tests.
    #[cfg(test)]
    pub(crate) fn live_writes(&self) -> Vec<crate::live::LiveWrite> {
        self.live_rows.writes.clone()
    }

    fn plan(&self, request: RunChecksRequest, cx: &App) -> Result<Plan, Refusal> {
        let Preflight {
            task,
            rows,
            tree,
            writer,
        } = self.preflight(&request.task_id, request.checks.as_deref(), cx)?;
        let tree = match self.node_branch_tree(&request, cx) {
            None => tree,
            Some(Some(branch)) => branch,
            // Never the run's own worktree in its stead.
            Some(None) => return Err(Refusal::NoTree { task: task.name }),
        };
        if self.drained {
            return Err(Refusal::Draining { task: task.name });
        }
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
            leases: Rc::default(),
            recorded: None,
        })
    }

    /// For a workflow node in a fan-out branch: `Some` with the worktree of that branch, or
    /// `Some(None)` when it is not there. `None` for any other request, which runs on the task's
    /// tree.
    fn node_branch_tree(&self, request: &RunChecksRequest, cx: &App) -> Option<Option<TaskTree>> {
        let (run, key) = request.node.as_ref()?;
        let (fan, index) = key.branch.last()?;
        Some(
            self.runs
                .branch_worktree(run, fan, *index, cx)
                .filter(|path| path.is_dir())
                .map(|path| TaskTree {
                    path,
                    run: Some(run.clone()),
                }),
        )
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
            let row = match &live {
                Some(Live::Waiting { pool, place, .. }) => Some(LiveRow::Waiting {
                    pool: pool.clone(),
                    place: *place,
                    since: (self.now)(),
                }),
                Some(Live::Running { part, of, kept }) => Some(LiveRow::Running {
                    part: *part,
                    of: *of,
                    kept: *kept,
                    since: (self.now)(),
                }),
                None => None,
            };
            if let Some(run) = self.active.get_mut(task_id) {
                match live {
                    Some(live) => run.live.insert(check.to_string(), live),
                    None => run.live.remove(check),
                };
            }
            if let Some(row) = row {
                self.write_live(task_id, check, row, cx);
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
        let row = LiveRow::Ended {
            verdict: verdict.clone(),
            since: (self.now)(),
        };
        self.write_live(task_id, check, row, cx);
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
        let mut leftovers = Leftovers::default();
        let prepared = prepare(&plan, row, &pin, sender, &mut leftovers).await;
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
                    Ok(record) => (record.verdict.clone(), Some(record.id), state),
                    Err(error) => (
                        Verdict::NoVerdict(start_failed("the check run", format!("{error:#}"))),
                        None,
                        state,
                    ),
                }
            }
        };
        leftovers.clear().await;
        plan.leases.borrow_mut().clear();
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

/// What a check's run leaves to undo when it ends: its temporary trees and its evidence file.
/// A temporary tree dropped without [`Leftovers::clear`] is still removed, on a thread.
#[derive(Default)]
struct Leftovers {
    temps: Vec<TempTree>,
    evidence: Option<PathBuf>,
}

impl Leftovers {
    async fn clear(&mut self) {
        for temp in self.temps.drain(..) {
            temp.remove().await;
        }
        if let Some(evidence) = self.evidence.take() {
            std::fs::remove_file(evidence).ok();
        }
    }
}

/// One repository's tree a check runs in.
pub(crate) struct RepoTree {
    pub(crate) repo: String,
    pub(crate) path: PathBuf,
    pub(crate) env: EnvBase,
}

/// The tip tree of each repository the check runs in, in its order: the task's own tree for the
/// task's own repository (and for a check that names none), else the tree a lane serves.
fn tip_trees(plan: &Plan, decl: &CheckDecl) -> Result<Vec<RepoTree>, NoVerdictReason> {
    tip_trees_of(&plan.task, &plan.tree.path, plan.trees.as_ref(), decl)
}

/// The repositories `decl` runs in, in its order: those it names, else the task's own.
pub(crate) fn check_repos(task: &TaskChecks, decl: &CheckDecl) -> Vec<String> {
    let own = task.repo.clone().unwrap_or_default();
    decl.repos
        .clone()
        .filter(|repos| !repos.is_empty())
        .unwrap_or_else(|| vec![own])
}

/// [`tip_trees`] for `task`, whose own tree is `own_tree`, with the lanes' trees `trees`.
pub(crate) fn tip_trees_of(
    task: &TaskChecks,
    own_tree: &Path,
    trees: Option<&Rc<dyn crate::trees::TaskTrees>>,
    decl: &CheckDecl,
) -> Result<Vec<RepoTree>, NoVerdictReason> {
    repo_trees(task, own_tree, trees, check_repos(task, decl))
}

/// The tip tree of each of `repos` for `task`: its own tree for its own repository, else the
/// tree a lane serves.
pub(crate) fn repo_trees(
    task: &TaskChecks,
    own_tree: &Path,
    trees: Option<&Rc<dyn crate::trees::TaskTrees>>,
    repos: Vec<String>,
) -> Result<Vec<RepoTree>, NoVerdictReason> {
    let own = task.repo.clone().unwrap_or_default();
    repos
        .into_iter()
        .map(|repo| {
            let path = if repo == own {
                Some(own_tree.to_path_buf())
            } else {
                trees.and_then(|trees| trees.tip(&task.task_id, &repo))
            };
            let path = path.ok_or_else(|| {
                NoVerdictReason::TreeUnavailable(format!("{} has no tree in {repo}", task.name))
            })?;
            let env = EnvBase::for_tip(trees.map(|trees| trees.as_ref()), &task.task_id, &repo);
            Ok(RepoTree { repo, path, env })
        })
        .collect()
}

/// The key of `trees`: one tree's own key, or the digest of the sorted (repository, key) pairs.
pub(crate) async fn key_of(
    git: &Git,
    trees: &[RepoTree],
    index_dir: &Path,
) -> Result<String, NoVerdictReason> {
    let pairs: Vec<(&str, &Path)> = trees
        .iter()
        .map(|tree| (tree.repo.as_str(), tree.path.as_path()))
        .collect();
    trees_key(git, &pairs, index_dir)
        .await
        .map(|key| key.as_str().to_string())
        .map_err(|error| NoVerdictReason::TreeUnavailable(error.to_string()))
}

/// The merge-base of each tip's `HEAD` and the task's base branch, resolved now.
async fn base_commits(
    git: &Git,
    tips: &[RepoTree],
    branch: Option<&str>,
) -> Result<Vec<String>, String> {
    let branch = branch.ok_or_else(|| "the task has no base branch".to_string())?;
    let mut commits = Vec::with_capacity(tips.len());
    for tip in tips {
        let commit = merge_base(git, &tip.path, branch)
            .await
            .map_err(|error| format!("finding the base commit: {error}"))?;
        commits.push(commit);
    }
    Ok(commits)
}

/// The base commits as a record keeps them: the commit for one repository, else
/// `<repository>=<commit>` pairs separated by spaces.
fn base_text(tips: &[RepoTree], commits: &[String]) -> String {
    match commits {
        [commit] => commit.clone(),
        _ => tips
            .iter()
            .zip(commits)
            .map(|(tip, commit)| format!("{}={commit}", tip.repo))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The base tree of each repository at its commit: the second tree a lane lends now, checked
/// out at the commit (its lease held in `plan.leases`), else a temporary tree, kept in
/// `leftovers` for removal. Never waits for a lease.
async fn base_trees(
    plan: &Plan,
    tips: &[RepoTree],
    commits: &[String],
    leftovers: &mut Leftovers,
) -> Result<Vec<RepoTree>, NoVerdictReason> {
    let task = &plan.task;
    let git = task.project.git();
    let mut protected: Vec<&Path> = tips.iter().map(|tip| tip.path.as_path()).collect();
    protected.push(plan.tree.path.as_path());
    protected.push(task.project.location().repository.as_path());
    let mut trees = Vec::with_capacity(tips.len());
    for (tip, commit) in tips.iter().zip(commits) {
        if plan.stop.is_stopped() {
            return Err(NoVerdictReason::Cancelled);
        }
        let answer = plan
            .trees
            .as_ref()
            .map_or(SecondTree::Unavailable, |trees| {
                trees.second_tree(&task.task_id, &tip.repo)
            });
        match answer {
            SecondTree::Lease(lease) => {
                let path = lease.path().to_path_buf();
                let env = EnvBase::for_lease(lease.as_ref());
                // Held here, out of a drain's reach, while git changes the tree: a lease is
                // let go only once no git command runs in it. Dropped on an error.
                check_out_lent_tree(git, &path, commit, &protected, &plan.stop)
                    .await
                    .map_err(|error| match error {
                        LentTreeError::Stopped => NoVerdictReason::Cancelled,
                        LentTreeError::Failed(why) => NoVerdictReason::StartFailed(why),
                    })?;
                plan.leases.borrow_mut().push(lease);
                if plan.stop.is_stopped() {
                    return Err(NoVerdictReason::Cancelled);
                }
                trees.push(RepoTree {
                    repo: tip.repo.clone(),
                    path,
                    env,
                });
            }
            SecondTree::Busy | SecondTree::Unavailable => {
                let temp = TempTree::create(
                    git,
                    &tip.path,
                    &plan.settings.scratch_folder(),
                    commit,
                    &task.index_dir,
                    &protected,
                )
                .await
                .map_err(NoVerdictReason::StartFailed)?;
                trees.push(RepoTree {
                    repo: tip.repo.clone(),
                    path: temp.path().to_path_buf(),
                    env: EnvBase::Own,
                });
                leftovers.temps.push(temp);
            }
        }
    }
    Ok(trees)
}

/// Writes the read-only `BONGOS_CHECK_EVIDENCE` file: every record of the task on `tip_key`, on
/// the base tree at `base`, and of the same workflow run, oldest first.
fn write_evidence(
    plan: &Plan,
    tip_key: &str,
    base: Option<&str>,
) -> Result<PathBuf, NoVerdictReason> {
    use std::os::unix::fs::PermissionsExt as _;

    let task = &plan.task;
    let records = plan
        .writer
        .read(|read| scan_records(read, &task_prefix(&task.task_id)))
        .map_err(|error| start_failed("reading the evidence", format!("{error:#}")))?;
    let run_id = match &plan.request.started_by {
        StartedBy::Run { run_id, .. } => Some(run_id.clone()),
        _ => plan.request.node.as_ref().map(|(run, _)| run.clone()),
    };
    let mut listed: Vec<&EvidenceRecord> = records
        .iter()
        .filter(|record| {
            record.task_id == task.task_id
                && (record.tree_key == tip_key
                    || (record.tree == TreeKind::Base
                        && base.is_some()
                        && record.base.as_deref() == base)
                    || run_id.as_ref().is_some_and(|id| {
                        matches!(&record.started_by, StartedBy::Run { run_id, .. } if run_id == id)
                    }))
        })
        .collect();
    listed.sort_by(|a, b| a.id.cmp(&b.id));
    let json = serde_json::json!({
        "task_id": task.task_id,
        "tree_key": tip_key,
        "base": base,
        "run_id": run_id,
        "records": listed,
    });
    std::fs::create_dir_all(&task.output_dir)
        .map_err(|error| start_failed("creating the output folder", error))?;
    let path = task
        .output_dir
        .join(format!("evidence-{}.json", uuid::Uuid::now_v7()));
    std::fs::write(&path, json.to_string())
        .map_err(|error| start_failed("writing the evidence file", error))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
        .map_err(|error| start_failed("writing the evidence file", error))?;
    Ok(path)
}

/// The digest of `decl` with the files it names, read from `pin`.
pub(crate) fn check_digest(decl: &CheckDecl, pin: &Pin) -> Result<decl::Digest, NoVerdictReason> {
    let mut files = BTreeMap::new();
    for path in decl.named_files() {
        let resolved = pin
            .resolve(path)
            .map_err(|error| start_failed("reading the check's files", error))?;
        let bytes = std::fs::read(&resolved)
            .map_err(|error| start_failed(&format!("reading {path}"), error))?;
        files.insert(path.to_string(), bytes);
    }
    decl::digest(decl, &files).map_err(|error| start_failed("digesting the check", error))
}

/// The run of one check: its digest, its trees and their key, its environment and its hooks,
/// and the check's current state on the tip (none for a base run). A reason when it cannot
/// start. What must be undone when the check ends goes into `leftovers`.
async fn prepare(
    plan: &Plan,
    row: &Row,
    pin: &Arc<Pin>,
    sender: mpsc::UnboundedSender<Message>,
    leftovers: &mut Leftovers,
) -> Result<(CheckRun, Option<CheckState>), NoVerdictReason> {
    let task = &plan.task;
    let decl = row.decl.clone();
    let digest = check_digest(&decl, pin)?;
    let git = task.project.git().clone();
    if let Some(recorded) = &plan.recorded {
        let (trees, key, key_inputs) = recorded
            .state(&git, &check_repos(task, &decl), &decl.key_inputs)
            .await?;
        let trees: Vec<RepoTree> = trees
            .into_iter()
            .map(|tree| RepoTree {
                repo: tree.repo.clone(),
                path: tree.path.clone(),
                env: tree.env.clone(),
            })
            .collect();
        let state = CheckState::new(&key, &key_inputs, &digest);
        leftovers.evidence = Some(write_evidence(plan, &key, None)?);
        return Ok((
            check_run(
                plan, decl, digest, pin, trees, key, key_inputs, None, sender, leftovers,
            )?,
            Some(state),
        ));
    }
    let tips = tip_trees(plan, &decl)?;
    let tip_key = key_of(&git, &tips, &task.index_dir).await?;
    let kind = plan.request.tree_kind();
    let commits = base_commits(&git, &tips, task.base_branch.as_deref()).await;
    let base = commits
        .as_ref()
        .ok()
        .map(|commits| base_text(&tips, commits));
    let (trees, key) = match kind {
        TreeKind::Tip => (tips, tip_key.clone()),
        TreeKind::Base => {
            let commits = commits.map_err(NoVerdictReason::StartFailed)?;
            let trees = base_trees(plan, &tips, &commits, leftovers).await?;
            let key = key_of(&git, &trees, &task.index_dir).await?;
            (trees, key)
        }
    };
    let inputs: Vec<_> = decl
        .key_inputs
        .iter()
        .map(|path| trees[0].path.join(path))
        .collect();
    let key_inputs = key_inputs_digest(&inputs)
        .map_err(|error| start_failed("reading the key inputs", error))?;
    let state = (kind == TreeKind::Tip).then(|| CheckState::new(&key, &key_inputs, &digest));
    leftovers.evidence = Some(write_evidence(plan, &tip_key, base.as_deref())?);
    let base = (kind == TreeKind::Base).then_some(base).flatten();
    Ok((
        check_run(
            plan, decl, digest, pin, trees, key, key_inputs, base, sender, leftovers,
        )?,
        state,
    ))
}

/// The run of one check on `trees` (the first is where it runs), keyed `key`.
#[allow(clippy::too_many_arguments)]
fn check_run(
    plan: &Plan,
    decl: CheckDecl,
    digest: decl::Digest,
    pin: &Arc<Pin>,
    trees: Vec<RepoTree>,
    key: String,
    key_inputs: String,
    base: Option<String>,
    sender: mpsc::UnboundedSender<Message>,
    leftovers: &Leftovers,
) -> Result<CheckRun, NoVerdictReason> {
    let task = &plan.task;
    let kind = plan.request.tree_kind();
    let mut trees = trees.into_iter();
    let Some(first) = trees.next() else {
        return Err(NoVerdictReason::TreeUnavailable(format!(
            "{} has no tree to check",
            task.name
        )));
    };
    let other_trees = trees
        .map(|tree| RunTree {
            path: tree.path,
            env: tree.env,
        })
        .collect();
    let changed = sender.clone();
    let check_run = CheckRun {
        task_id: task.task_id.clone(),
        decl,
        digest: digest.to_string(),
        pin: pin.clone(),
        master: master_definitions(&task.project),
        tree: first.path,
        other_trees,
        tree_kind: kind,
        base,
        tree_key: key,
        key_inputs,
        git: task.project.git().clone(),
        base_branch: task.base_branch.clone(),
        env: first.env,
        output_dir: task.output_dir.clone(),
        evidence: leftovers.evidence.clone(),
        params: plan.request.params.clone(),
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
        node: plan.request.node.clone(),
        pools: plan.pools.clone(),
        clock: plan.clock.clone(),
        on_live: Some(Rc::new(move |state: &LiveState| {
            sender.unbounded_send(Message::Live(state.clone())).ok();
        })),
        #[cfg(test)]
        fail_part_end: None,
    };
    Ok(check_run)
}
