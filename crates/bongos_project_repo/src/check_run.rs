//! Running the checks of one change on one tree: the facts are read once, the tree and master
//! are materialized while a registered check may read them, and every result is stored under
//! `(change, check, tree, stamp)`. A stored pass or fail with the same tree and stamp is reused;
//! a result for another tree or stamp is never counted, and a no verdict is always tried again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use bongos_project_repo_model::checks::{
    CheckOutcome, CheckPhase, LiveFacts, StagedTodoMove, Stamp,
};
use bongos_project_repo_model::{Author, ChangeMeta, TaskStateRecord};
use bongos_work_model::ProjectFile;
use serde::{Deserialize, Serialize};

use crate::builtin_checks::{BuiltinInput, ChangeFacts, builtin_stamp, run_builtin, staged_moves};
use crate::create::ProjectRepo;
use crate::declared_checks::{CheckPlan, plan};
use crate::git::{CommitId, TreeId};
use crate::landing::LandingResult;
use crate::landing_checks::{CheckInput, CheckRegistry};
use crate::pins::PinGuard;
use crate::state::StateWriter;

/// A check's outcome on one tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub check: String,
    pub tree: String,
    pub stamp: String,
    pub outcome: CheckOutcome,
}

/// How a result is stored.
#[derive(Serialize, Deserialize)]
struct StoredResult {
    check: String,
    tree: String,
    stamp: String,
    outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<PathBuf>,
}

impl CheckResult {
    fn to_record(&self) -> Result<String> {
        let (text, output) = match &self.outcome {
            CheckOutcome::Pass => (None, None),
            CheckOutcome::Fail { message, output } => (Some(message.clone()), output.clone()),
            CheckOutcome::NoVerdict { reason, output } => (Some(reason.clone()), output.clone()),
        };
        toml::to_string(&StoredResult {
            check: self.check.clone(),
            tree: self.tree.clone(),
            stamp: self.stamp.clone(),
            outcome: self.outcome.word().to_string(),
            text,
            output,
        })
        .context("serializing a check result")
    }

    pub(crate) fn from_record(record: &str) -> Result<Self> {
        let stored: StoredResult = toml::from_str(record).context("reading a check result")?;
        let text = stored.text.unwrap_or_default();
        let outcome = match stored.outcome.as_str() {
            "pass" => CheckOutcome::Pass,
            "fail" => CheckOutcome::Fail {
                message: text,
                output: stored.output,
            },
            _ => CheckOutcome::NoVerdict {
                reason: text,
                output: stored.output,
            },
        };
        Ok(Self {
            check: stored.check,
            tree: stored.tree,
            stamp: stored.stamp,
            outcome,
        })
    }
}

/// What the results say: refused with every failure, or waiting for the first no verdict.
pub(crate) fn verdict(results: &[CheckResult]) -> Option<LandingResult> {
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| match &result.outcome {
            CheckOutcome::Fail { message, .. } => Some(format!("{}: {message}", result.check)),
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        return Some(LandingResult::Refused { reasons: failures });
    }
    results.iter().find_map(|result| match &result.outcome {
        CheckOutcome::NoVerdict { reason, .. } => Some(LandingResult::NoVerdict {
            reason: format!("{}: {reason}", result.check),
        }),
        _ => None,
    })
}

/// The checks of one change on one tree, against master at `facts.master_commit`.
pub(crate) struct CheckRun {
    pub(crate) change_id: String,
    pub(crate) facts: ChangeFacts,
    pub(crate) registry: CheckRegistry,
    pub(crate) plan: CheckPlan,
    pub(crate) live: LiveFacts,
    pub(crate) project: ProjectFile,
    pub(crate) meta: ChangeMeta,
    pub(crate) staged_moves: Vec<StagedTodoMove>,
    pub(crate) timer: crate::landing_queue::Timer,
    /// The materialized change tree and master, while a registered check may read them.
    pins: Option<(PinGuard, PinGuard)>,
}

impl CheckRun {
    pub(crate) fn tree(&self) -> &str {
        &self.facts.new.id
    }

    /// The input of a registered check; [`ProjectRepo::pin`] must have run.
    pub(crate) fn input(&self) -> CheckInput<'_> {
        let (root, master_root) = self
            .pins
            .as_ref()
            .expect("the trees are materialized before a registered check runs");
        CheckInput {
            project: &self.project,
            master: CommitId(self.facts.master_commit.clone()),
            tree: TreeId(self.facts.new.id.clone()),
            root: root.path(),
            master_root: master_root.path(),
            summary: &self.facts.summary,
            meta: &self.meta,
            live: &self.live,
            timer: self.timer.clone(),
        }
    }

    fn builtin_input<'a>(&'a self, options: &'a crate::LandOptions) -> BuiltinInput<'a> {
        BuiltinInput {
            facts: &self.facts,
            registry: &self.registry,
            live: &self.live,
            change_id: &self.change_id,
            meta: &self.meta,
            staged_moves: &self.staged_moves,
            size_limits: options.size_limits,
            secret_patterns: &options.secret_patterns,
        }
    }
}

impl ProjectRepo {
    /// Reads what the checks of `change_id` at `treeish` against master at `master` need.
    pub(crate) async fn prepare_checks(
        &self,
        change_id: &str,
        master: &str,
        treeish: &str,
        meta: &ChangeMeta,
        author: &Author,
    ) -> Result<CheckRun> {
        let registry = self.check_registry();
        let folders: Vec<String> = registry
            .folders
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let facts = ChangeFacts::read(
            &self.git,
            &self.location.repository,
            master,
            treeish,
            &folders,
            author,
        )
        .await?;
        let master_project = facts
            .master
            .definitions
            .as_ref()
            .ok()
            .and_then(|tree| tree.project.clone());
        let plan = plan(master_project.as_ref(), &registry);
        let live = self.live_facts(&registry, &facts.task_ids())?;
        let staged = self
            .read_state(|read| read.staged(change_id))?
            .unwrap_or_default();
        let project = facts
            .new
            .definitions
            .as_ref()
            .ok()
            .and_then(|tree| tree.project.clone())
            .or(master_project)
            .unwrap_or_else(|| ProjectFile::new(""));
        Ok(CheckRun {
            change_id: change_id.to_string(),
            facts,
            registry,
            plan,
            live,
            project,
            meta: meta.clone(),
            staged_moves: staged_moves(&staged),
            timer: self.engine.timer(),
            pins: None,
        })
    }

    /// The live facts over `ids`: their state rows and the registered sources.
    pub(crate) fn live_facts(&self, registry: &CheckRegistry, ids: &[String]) -> Result<LiveFacts> {
        let states: BTreeMap<String, TaskStateRecord> = self.read_state(|read| {
            let mut states = BTreeMap::new();
            for id in ids {
                if let Some(record) = read.task(id)? {
                    states.insert(id.clone(), record);
                }
            }
            Ok(states)
        })?;
        let mut live = LiveFacts::from_states(states);
        for source in &registry.live {
            live.register(source.clone());
        }
        Ok(live)
    }

    /// Materializes the change's tree and master for the registered checks of `run`.
    pub(crate) async fn pin(&self, run: &mut CheckRun) -> Result<()> {
        if run.pins.is_none() {
            let root = self.materialize(&run.facts.new.id).await?;
            let master = self.materialize(&run.facts.master_commit).await?;
            run.pins = Some((root, master));
        }
        Ok(())
    }

    fn store_result(
        &self,
        writer: &StateWriter,
        change_id: &str,
        result: &CheckResult,
    ) -> Result<()> {
        let record = result.to_record()?;
        writer.transact(|tx| {
            tx.set_check_result(
                change_id,
                &result.check,
                &result.tree,
                &result.stamp,
                &record,
            )
        })
    }

    /// The built-ins on the run's tree, each result stored.
    pub(crate) fn run_builtins(
        &self,
        writer: &StateWriter,
        run: &CheckRun,
    ) -> Result<Vec<CheckResult>> {
        let options = self.land_options();
        let input = run.builtin_input(&options);
        let mut results = Vec::new();
        for name in &run.plan.builtins {
            let result = CheckResult {
                check: name.to_string(),
                tree: run.tree().to_string(),
                stamp: builtin_stamp(name, &input).0,
                outcome: run_builtin(name, &input),
            };
            self.store_result(writer, &run.change_id, &result)?;
            results.push(result);
        }
        Ok(results)
    }

    /// The registered and declared checks of `phase` that apply, at once; a stored pass or fail
    /// for the same tree and stamp is reused. Returns the results and the stamp of each.
    pub(crate) async fn run_registered(
        &self,
        writer: &StateWriter,
        run: &mut CheckRun,
        phase: CheckPhase,
    ) -> Result<(Vec<CheckResult>, BTreeMap<String, Stamp>)> {
        let checks: Vec<_> = run
            .plan
            .checks_in(phase)
            .into_iter()
            .filter(|check| check.applies(&run.facts.summary))
            .collect();
        if checks.is_empty() {
            return Ok((Vec::new(), BTreeMap::new()));
        }
        self.pin(run).await?;
        let tree = run.tree().to_string();
        let mut stamps = BTreeMap::new();
        let mut results: Vec<Option<CheckResult>> = Vec::new();
        let mut pending = Vec::new();
        for (index, check) in checks.iter().enumerate() {
            let stamp = check.stamp(&run.input());
            stamps.insert(check.name().to_string(), stamp.clone());
            let stored = self
                .read_state(|read| {
                    read.check_result(&run.change_id, check.name(), &tree, stamp.as_str())
                })?
                .map(|record| CheckResult::from_record(&record))
                .transpose()?
                .filter(|result| !matches!(result.outcome, CheckOutcome::NoVerdict { .. }));
            if stored.is_none() {
                pending.push((index, stamp, check.run(run.input())));
            }
            results.push(stored);
        }
        // The pins in `run` stay until every future here has completed.
        let (indexes, futures): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .map(|(index, stamp, future)| ((index, stamp), future))
            .unzip();
        let outcomes = futures::future::join_all(futures).await;
        for ((index, stamp), outcome) in indexes.into_iter().zip(outcomes) {
            let result = CheckResult {
                check: checks[index].name().to_string(),
                tree: tree.clone(),
                stamp: stamp.0,
                outcome,
            };
            self.store_result(writer, &run.change_id, &result)?;
            results[index] = Some(result);
        }
        Ok((results.into_iter().flatten().collect(), stamps))
    }

    /// The stamps the checks of `run` would store now for the checks named in `seen`; the names
    /// whose stamp differs from the one they saw (or that no longer apply).
    pub(crate) async fn moved_stamps(
        &self,
        run: &mut CheckRun,
        seen: &BTreeMap<String, Stamp>,
    ) -> Result<BTreeSet<String>> {
        if seen.is_empty() {
            return Ok(BTreeSet::new());
        }
        self.pin(run).await?;
        let mut now = BTreeMap::new();
        for check in &run.plan.checks {
            if check.applies(&run.facts.summary) {
                now.insert(check.name().to_string(), check.stamp(&run.input()));
            }
        }
        for check in &run.plan.amending {
            if check.applies(&run.facts.summary) {
                now.insert(check.name().to_string(), check.stamp(&run.input()));
            }
        }
        Ok(seen
            .iter()
            .filter(|(name, stamp)| now.get(*name) != Some(*stamp))
            .map(|(name, _)| name.clone())
            .collect())
    }

    /// The stored check results of `change_id`, every tree.
    pub fn check_results(&self, change_id: &str) -> Result<Vec<CheckResult>> {
        let rows = self.read_state(|read| read.check_results(change_id))?;
        rows.iter()
            .map(|(_, record)| CheckResult::from_record(record))
            .collect()
    }

    /// The stored check results of `change_id` on `tree`; results for other trees are not
    /// counted.
    pub fn check_results_on(&self, change_id: &str, tree: &str) -> Result<Vec<CheckResult>> {
        Ok(self
            .check_results(change_id)?
            .into_iter()
            .filter(|result| result.tree == tree)
            .collect())
    }
}
