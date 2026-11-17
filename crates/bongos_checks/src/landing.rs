//! Task checks at landing: the `check` kind of a project's `[[landing.checks]]` entries, which
//! runs one check declaration on the tree a change would land, and the approval hook that asks a
//! person for a change that loosens the required checks of a started task.
//!
//! A landing judges a change on a background thread with no app at hand, so the kind's check
//! holds no gpui handle: it copies what it needs out of the landing's input and sends a
//! [`LandingJob`] through a process-wide channel to the [`ChecksEngine`], which receives it on
//! the main thread and runs the check as it runs any other. The check's scripts are read from
//! master's copy (a pin of the master commit), never from the change, so a branch's own copy of
//! a script never decides its own verdict. The tree under check is the landing's read-only copy
//! of the change's tree, which the landing keeps until the check's answer arrives. The record is
//! stored under `landing/<tree>/<check>/`, so a pass counts only for that exact tree.
//!
//! While `task_checks.enabled` is false the kind answers no verdict, "task checks are off", and
//! the hook has no opinion.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use bongos_checks_model::evidence::{EvidenceRecord, Output, StartedBy, TreeKind};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_checks_model::{CheckDecl, Severity, decl, effective_set, loosening, validate};
use bongos_project_repo::{
    ApprovalHook, ApprovalHooks, ApprovalInput, ApprovalOpinion, CheckInput, CheckKinds,
    CheckOutcome, CheckPhase, LandingCheck, ProjectRepo,
};
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::{App, Context, Entity};
use settings::{Settings as _, SettingsStore};

use crate::definitions::named_sets;
use crate::engine::{ChecksEngine, check_digest};
use crate::evidence_store::LANDING_PREFIX;
use crate::pin::{Pin, master_definitions};
use crate::pools::{CheckClock, CheckPoolsRegistry};
use crate::process::{StopHandle, part_output_path};
use crate::run::{CheckRun, Now};
use crate::settings::TaskChecksSettings;
use crate::trees::EnvBase;

/// The key of a `[[landing.checks]]` entry's `run` table that holds a check declaration.
pub const LANDING_KIND: &str = "check";
/// The no verdict of a landing check while `task_checks.enabled` is false.
pub const CHECKS_OFF: &str = "task checks are off";
/// The no verdict of a landing check whose project file has no id.
pub const NO_PROJECT_ID: &str = "project has no id";
/// The no verdict of a landing check the engine stopped or refused while it moves to the host.
pub const MOVING_TO_HOST: &str = "the engine is moving to the host";
/// The name of the approval hook over loosened checks.
pub const CHECKS_LOOSENING: &str = "checks-loosening";

/// One landing check for the engine to run: the declaration, the master commit whose copy of the
/// scripts it runs, the tree under check and where the landing materialized it, the project by
/// id, and where the answer goes.
pub struct LandingJob {
    pub decl: CheckDecl,
    pub master: String,
    pub tree: String,
    pub root: PathBuf,
    pub project_id: String,
    pub reply: oneshot::Sender<CheckOutcome>,
}

/// Where the kind sends its jobs; set once by [`register`].
static LANDING_JOBS: OnceLock<smol::channel::Sender<LandingJob>> = OnceLock::new();

/// `task_checks.enabled` of this process, kept by [`register`] for the landing's threads.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Registers the `check` kind and the loosening hook, and makes `engine` receive the kind's
/// jobs. Runs before any project repository opens, like every landing registration.
pub fn register(engine: &Entity<ChecksEngine>, cx: &mut App) {
    CheckKinds::register(LANDING_KIND, parse_landing_check, cx);
    ApprovalHooks::register(Arc::new(ChecksLoosening::new(&ENABLED)), cx);
    ENABLED.store(TaskChecksSettings::get_global(cx).enabled, Ordering::SeqCst);
    cx.observe_global::<SettingsStore>(|cx| {
        ENABLED.store(TaskChecksSettings::get_global(cx).enabled, Ordering::SeqCst);
    })
    .detach();
    let (sender, receiver) = smol::channel::unbounded();
    if LANDING_JOBS.set(sender).is_ok() {
        engine.update(cx, |engine, cx| engine.receive_landing_jobs(receiver, cx));
    }
}

/// Parses the declaration a `run = { check = { ... } }` table holds: one check, as a task's
/// `[[checks.check]]` entry declares it, whose `name` defaults to what it runs.
pub fn parse_landing_check(value: &toml::Value) -> Result<Arc<dyn LandingCheck>, String> {
    let mut entry = value
        .as_table()
        .cloned()
        .ok_or("check must be a table with run or command")?;
    if !entry.contains_key("name") {
        let runs = entry
            .get("run")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                entry
                    .get("command")
                    .and_then(toml::Value::as_array)
                    .map(|command| {
                        command
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            })
            .unwrap_or_default();
        entry.insert("name".to_string(), toml::Value::String(runs));
    }
    let mut table = toml::Table::new();
    table.insert(
        "check".to_string(),
        toml::Value::Array(vec![toml::Value::Table(entry)]),
    );
    let parsed = decl::parse(PROJECT_FILE, &toml::Value::Table(table));
    // Whether a script exists is master's to say when the check runs.
    let errors: Vec<String> = parsed
        .findings
        .iter()
        .cloned()
        .chain(validate(PROJECT_FILE, &parsed.check, &|_| true))
        .filter(|finding| finding.severity == Severity::Error)
        .map(|finding| finding.message)
        .collect();
    if !errors.is_empty() {
        return Err(format!("check: {}", errors.join("; ")));
    }
    let decl = parsed
        .check
        .into_iter()
        .next()
        .ok_or("check must be a table with run or command")?;
    Ok(Arc::new(TaskCheckLanding { decl }))
}

/// A `[[landing.checks]]` entry `run = { check = { ... } }`: runs the declaration through the
/// checks engine on the change's tree, with master's copy of its scripts.
pub struct TaskCheckLanding {
    decl: CheckDecl,
}

impl TaskCheckLanding {
    pub fn decl(&self) -> &CheckDecl {
        &self.decl
    }
}

impl LandingCheck for TaskCheckLanding {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::BeforeQueue
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let jobs = LANDING_JOBS
            .get()
            .filter(|_| ENABLED.load(Ordering::SeqCst));
        send_job(jobs, &self.decl, &input)
    }
}

/// Sends the job of `decl` on `input` to `jobs` and answers what the engine answers; no verdict
/// when there is no engine (the feature is off) or the project has no id. Everything is copied
/// out of `input` before this returns.
pub(crate) fn send_job(
    jobs: Option<&smol::channel::Sender<LandingJob>>,
    decl: &CheckDecl,
    input: &CheckInput,
) -> BoxFuture<'static, CheckOutcome> {
    let Some(jobs) = jobs else {
        return async { CheckOutcome::no_verdict(CHECKS_OFF) }.boxed();
    };
    let Some(project_id) = input.project.id.clone() else {
        return async { CheckOutcome::no_verdict(NO_PROJECT_ID) }.boxed();
    };
    let (reply, answer) = oneshot::channel();
    let job = LandingJob {
        decl: decl.clone(),
        master: input.master.0.clone(),
        tree: input.tree.0.clone(),
        root: input.root.to_path_buf(),
        project_id,
        reply,
    };
    let sent = jobs.try_send(job).is_ok();
    async move {
        if !sent {
            return CheckOutcome::no_verdict("the checks engine is not running");
        }
        answer
            .await
            .unwrap_or_else(|_| CheckOutcome::no_verdict("the checks engine stopped"))
    }
    .boxed()
}

/// A landing check the engine runs now.
pub(crate) struct LandingRun {
    pub(crate) stop: StopHandle,
    /// Set by a drain: the check is answered [`MOVING_TO_HOST`] when its process has ended.
    pub(crate) moving: bool,
}

/// Everything one landing check runs with.
struct LandingPlan {
    decl: CheckDecl,
    master: String,
    tree: String,
    root: PathBuf,
    project_id: String,
    project: ProjectRepo,
    writer: Arc<bongos_project_repo::StateWriter>,
    output_dir: PathBuf,
    settings: TaskChecksSettings,
    pools: Option<std::rc::Rc<dyn crate::pools::CheckPools>>,
    clock: std::rc::Rc<dyn CheckClock>,
    now: Now,
    stop: StopHandle,
}

impl ChecksEngine {
    /// Runs every landing job that arrives on `receiver`, for as long as the engine lives.
    pub(crate) fn receive_landing_jobs(
        &mut self,
        receiver: smol::channel::Receiver<LandingJob>,
        cx: &mut Context<Self>,
    ) {
        self.landing_jobs = Some(cx.spawn(async move |this, cx| {
            while let Ok(job) = receiver.recv().await {
                // A job the engine never takes drops its reply, which the check reads as the
                // engine having stopped.
                if this
                    .update(cx, |engine, cx| engine.start_landing(job, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Starts the check of `job`, or answers it at once when it cannot run here.
    pub(crate) fn start_landing(&mut self, job: LandingJob, cx: &mut Context<Self>) {
        let settings = TaskChecksSettings::get_global(cx).clone();
        let refusal = if self.drained {
            Some(MOVING_TO_HOST.to_string())
        } else if !settings.enabled {
            Some(CHECKS_OFF.to_string())
        } else {
            None
        };
        if let Some(reason) = refusal {
            job.reply.send(CheckOutcome::no_verdict(reason)).ok();
            return;
        }
        let Some(project) = self.source.project(&job.project_id, cx) else {
            let reason = format!("the project {} is not open here", job.project_id);
            job.reply.send(CheckOutcome::no_verdict(reason)).ok();
            return;
        };
        let Some(writer) = project.state().and_then(|state| state.writer().cloned()) else {
            let reason = "this process does not write the project's state";
            job.reply.send(CheckOutcome::no_verdict(reason)).ok();
            return;
        };
        let id = self.next_landing;
        self.next_landing += 1;
        let stop = StopHandle::default();
        self.landing.insert(
            id,
            LandingRun {
                stop: stop.clone(),
                moving: false,
            },
        );
        let LandingJob {
            decl,
            master,
            tree,
            root,
            project_id,
            reply,
        } = job;
        let plan = LandingPlan {
            decl,
            master,
            tree,
            root,
            project_id,
            output_dir: settings.output_folder(&project.location().files, LANDING_PREFIX),
            project,
            writer,
            settings,
            pools: CheckPoolsRegistry::get(cx),
            clock: self.clock.clone(),
            now: self.now.clone(),
            stop,
        };
        cx.spawn(async move |this, cx| {
            let outcome = run_landing(plan).await;
            let outcome = this
                .update(cx, |engine, _| {
                    let moving = engine.landing.remove(&id).is_some_and(|run| run.moving);
                    engine.tell_idle();
                    if moving {
                        CheckOutcome::NoVerdict {
                            reason: MOVING_TO_HOST.to_string(),
                            output: output_of(&outcome),
                        }
                    } else {
                        outcome.clone()
                    }
                })
                .unwrap_or(outcome);
            reply.send(outcome).ok();
        })
        .detach();
    }
}

fn output_of(outcome: &CheckOutcome) -> Option<PathBuf> {
    match outcome {
        CheckOutcome::Pass => None,
        CheckOutcome::Fail { output, .. } | CheckOutcome::NoVerdict { output, .. } => {
            output.clone()
        }
    }
}

/// Runs the check of `plan` with master's copy of its scripts on the landing's tree.
async fn run_landing(plan: LandingPlan) -> CheckOutcome {
    let pin = match Pin::materialize(&plan.project, &plan.master).await {
        Ok(pin) => Arc::new(pin),
        Err(error) => {
            return CheckOutcome::no_verdict(format!(
                "could not start: pinning master's definitions: {error:#}"
            ));
        }
    };
    let digest = match check_digest(&plan.decl, &pin) {
        Ok(digest) => digest,
        Err(reason) => return CheckOutcome::no_verdict(reason.text()),
    };
    let inputs: Vec<PathBuf> = plan
        .decl
        .key_inputs
        .iter()
        .map(|path| plan.root.join(path))
        .collect();
    let key_inputs = match crate::tree_key::key_inputs_digest(&inputs) {
        Ok(key_inputs) => key_inputs,
        Err(error) => {
            return CheckOutcome::no_verdict(format!(
                "could not start: reading the key inputs: {error}"
            ));
        }
    };
    let output_dir = plan.output_dir.clone();
    let check_run = CheckRun {
        task_id: LANDING_PREFIX.to_string(),
        digest: digest.to_string(),
        pin,
        master: master_definitions(&plan.project),
        tree: plan.root,
        other_trees: Vec::new(),
        tree_kind: TreeKind::Tip,
        base: None,
        tree_key: plan.tree.clone(),
        key_inputs,
        git: plan.project.git().clone(),
        base_branch: None,
        env: EnvBase::Own,
        output_dir: plan.output_dir,
        evidence: None,
        params: None,
        started_by: StartedBy::Landing { tree: plan.tree },
        clean: false,
        settings: plan.settings,
        writer: plan.writer,
        stop: plan.stop,
        now: plan.now,
        on_changed: None,
        project_id: plan.project_id,
        node: None,
        pools: plan.pools,
        clock: plan.clock,
        on_live: None,
        decl: plan.decl,
        #[cfg(test)]
        fail_part_end: None,
    };
    match check_run.run().await {
        Ok(record) => outcome_of(&record, &output_dir),
        Err(error) => CheckOutcome::no_verdict(format!("the check run: {error:#}")),
    }
}

/// The landing's outcome of `record`: a fail with its summary and failing items, or a no verdict
/// with its reason, each with the log of the part that decided it.
pub(crate) fn outcome_of(record: &EvidenceRecord, output_dir: &Path) -> CheckOutcome {
    let log = record
        .parts
        .iter()
        .find(|part| part.verdict == record.verdict)
        .map(|part| part_output_path(output_dir, &record.id, part.n))
        .filter(|path| path.is_file())
        .or_else(|| match &record.output {
            Output::Path(path) => Some(PathBuf::from(path)),
            Output::Deleted => None,
        });
    match &record.verdict {
        Verdict::Pass => CheckOutcome::Pass,
        Verdict::Fail => {
            let mut message = record
                .summary
                .clone()
                .filter(|summary| !summary.trim().is_empty())
                .unwrap_or_else(|| "failed".to_string());
            if !record.failing.is_empty() {
                message.push_str(&format!("; failing: {}", record.failing.join(", ")));
            }
            CheckOutcome::Fail {
                message,
                output: log,
            }
        }
        Verdict::NoVerdict(reason) => CheckOutcome::NoVerdict {
            reason: no_verdict_text(record, reason),
            output: log,
        },
    }
}

fn no_verdict_text(record: &EvidenceRecord, reason: &NoVerdictReason) -> String {
    record.reason.clone().unwrap_or_else(|| reason.text())
}

/// The approval hook over loosened checks: a change that loosens a required check of a started
/// task (removes, drops or advises it, widens its pass codes, and the like) waits for a person.
/// A task that has not started is not judged.
pub struct ChecksLoosening {
    enabled: &'static AtomicBool,
}

impl ChecksLoosening {
    /// A hook that judges while `enabled` is true and has no opinion otherwise.
    pub fn new(enabled: &'static AtomicBool) -> Self {
        Self { enabled }
    }
}

impl ApprovalHook for ChecksLoosening {
    fn name(&self) -> &str {
        CHECKS_LOOSENING
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        if !self.enabled.load(Ordering::SeqCst) {
            return ApprovalOpinion::NoOpinion;
        }
        let master_sets = named_sets(&input.master_root.join(DEFINITIONS_DIR));
        let change_sets = named_sets(&input.change_root.join(DEFINITIONS_DIR));
        let mut found = Vec::new();
        for node in input.master.nodes() {
            let id = node.definition.id.as_str();
            if !input.live.started(id) {
                continue;
            }
            let old = effective_set(id, input.master, &master_sets, &|_| false);
            let new = effective_set(id, input.change, &change_sets, &|_| false);
            for finding in loosening(&old, &new) {
                let shown = finding.to_string();
                let prefix = format!("check {}: ", finding.check);
                let details = shown.strip_prefix(&prefix).unwrap_or(&shown);
                found.push(format!(
                    "loosens check {} of started task {}: {details}",
                    finding.check, node.definition.goal
                ));
            }
        }
        if found.is_empty() {
            ApprovalOpinion::NoOpinion
        } else {
            ApprovalOpinion::NeedsPerson(found.join("; "))
        }
    }
}
