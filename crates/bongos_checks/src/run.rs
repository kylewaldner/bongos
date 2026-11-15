//! A check run: its scope, its part count, its parts one after another, and its record.
//!
//! [`CheckRun::run`] asks the check's scope script what is in scope (or takes `all`, or the
//! check's lack of a scope), counts the parts (a fixed count or the `parts.plan` script), then
//! looks for a checkpoint: a record of the same task, check, digest, tree key, key inputs and
//! variant (scope, part count, params). Its parts that passed or failed are kept; the others run
//! again. Any difference of key starts a new record, so an edit, a replan, another scope, a
//! changed check or another tree never reuses old parts. A clean run closes the matching
//! unfinished records as superseded and starts a new one.
//!
//! Each part's start (its pid, start time and token) and its end are written into the
//! unfinished record in a transaction of their own, so a crash leaves a record the next run
//! resumes; the record's end is written in one transaction when the parts are done.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bongos_checks_model::decl::{self, CheckDecl};
use bongos_checks_model::evidence::{
    EvidenceRecord, Output, Params, PartRecord, Scope, StartedBy, TreeKind, Variant,
};
use bongos_checks_model::verdict::{
    MAX_PARTS, NoVerdictReason, ScopeAnswer, Verdict, combine, parse_plan_answer,
    parse_scope_answer, union_failing,
};
use bongos_project_repo::{Git, StateTx, StateWriter};
use serde_json::json;

use crate::changed_files::{ChangedFiles, changed_files};
use crate::evidence_store::{evidence_prefix, scan_records_in, store_record, task_prefix};
use crate::pin::Pin;
use crate::process::{
    self, PartOutcome, PartScope, PartSpec, PartStart, ProcessRole, StopHandle, kill_orphan_parts,
    run_part,
};
use crate::settings::TaskChecksSettings;
use crate::trees::EnvBase;

/// The current time as RFC 3339 text, injected so nothing here reads the clock itself.
pub type Now = Arc<dyn Fn() -> String + Send + Sync>;

/// Told the task id each time a run wrote a record's end.
pub type OnChanged = Arc<dyn Fn(&str) + Send + Sync>;

/// The scope answer when the task has no base branch, so no changed files can be listed.
pub const NO_BASE_BRANCH: &str = "no base branch";
/// The reason of a record a clean run closed.
pub const SUPERSEDED: &str = "superseded by a clean run";

/// The items file of a run with `items` in scope, in the record's output folder.
const ITEMS_FILE: &str = "items.txt";

/// Everything one run of one check needs.
pub struct CheckRun {
    pub task_id: String,
    pub decl: CheckDecl,
    /// The check's digest (its declaration and the files it names).
    pub digest: String,
    /// Held for the whole run; every script resolves in it.
    pub pin: Arc<Pin>,
    /// `definitions/` of the master checkout.
    pub master: PathBuf,
    pub tree: PathBuf,
    pub tree_kind: TreeKind,
    pub tree_key: String,
    /// The combined digest of the check's key input files.
    pub key_inputs: String,
    /// Lists the changed files for a scope script.
    pub git: Git,
    /// The task's base branch; `None` hands a scoped check `all` with "no base branch".
    pub base_branch: Option<String>,
    pub env: EnvBase,
    /// The task's output folder; each record writes into `<this>/<record id>/`.
    pub output_dir: PathBuf,
    /// `BONGOS_CHECK_EVIDENCE`, when the caller wrote one.
    pub evidence: Option<PathBuf>,
    /// `Some` for a parameterized run.
    pub params: Option<Params>,
    pub started_by: StartedBy,
    /// Start over: close matching unfinished records as superseded and keep no part.
    pub clean: bool,
    pub settings: TaskChecksSettings,
    pub writer: Arc<StateWriter>,
    pub stop: StopHandle,
    pub now: Now,
    pub on_changed: Option<OnChanged>,
}

/// What the scope settled on.
struct Scoped {
    scope: Scope,
    reason: Option<String>,
    /// The items, when the scope script named some.
    items: Option<Vec<String>>,
}

impl CheckRun {
    /// Runs the check to its end (or its stop) and returns the record as written. An error is
    /// an error of the state store or the output folder; everything about the check itself
    /// ends in a record.
    pub async fn run(self) -> Result<EvidenceRecord> {
        let staging_id = new_id();
        let staging = self.output_dir.join(&staging_id);
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating the output folder {}", staging.display()))?;
        let started = (self.now)();

        let scoped = match self.scope(&staging).await {
            Ok(scoped) => scoped,
            Err(reason) => {
                return self.refused(&staging_id, &staging, started, Scope::All, None, reason);
            }
        };
        if let Scope::None { reason } = &scoped.scope {
            return self.nothing_in_scope(&staging_id, &staging, started, reason.clone());
        }
        let parts = match self.part_count(&staging, &scoped).await {
            Ok(parts) => parts,
            Err(reason) => {
                return self.refused(
                    &staging_id,
                    &staging,
                    started,
                    scoped.scope,
                    scoped.reason,
                    reason,
                );
            }
        };
        let variant = Variant {
            scope: scoped.scope.clone(),
            parts,
            params: self.params.clone(),
        };

        let found = self.writer.transact(|tx| {
            let matching = self.matching(tx, &variant)?;
            if self.clean {
                self.supersede(tx, &matching)?;
                return Ok(None);
            }
            Ok(matching.into_iter().max_by(|a, b| a.id.cmp(&b.id)))
        })?;
        let stops_early = self.decl.stops_at_first_failure();
        let mut record = match found {
            Some(found) if !found.is_finished() => {
                kill_orphan_parts(&found.parts);
                found
            }
            Some(found) => {
                if parts_to_run(&found, parts, stops_early).is_empty() {
                    remove_folder(&staging);
                    return Ok(found);
                }
                let mut record = self.new_record(&staging_id, &staging, started, variant);
                record.scope_reason = scoped.reason.clone();
                record.parts = kept_parts(&found);
                record.command = found.command;
                record
            }
            None => {
                let mut record = self.new_record(&staging_id, &staging, started, variant);
                record.scope_reason = scoped.reason.clone();
                record
            }
        };
        let folder = self.output_dir.join(&record.id);
        if record.id != staging_id {
            adopt(&staging, &folder)?;
        }
        self.writer.transact(|tx| store_record(tx, &record))?;

        let part_scope = match (&scoped.scope, &scoped.items) {
            (Scope::Unscoped, _) => PartScope::Unscoped,
            (_, Some(_)) => PartScope::Items(folder.join(ITEMS_FILE)),
            _ => PartScope::All,
        };
        record = self.run_parts(record, &part_scope).await?;
        self.finish(record)
    }

    /// Runs every part not kept, in order, writing each into the record as it starts and ends.
    async fn run_parts(
        &self,
        record: EvidenceRecord,
        part_scope: &PartScope,
    ) -> Result<EvidenceRecord> {
        let total = record.variant.parts;
        let stops_early = self.decl.stops_at_first_failure();
        let shared = Arc::new(Mutex::new(record));
        let to_run = parts_to_run(&lock(&shared), total, stops_early);
        for n in to_run {
            if self.stop.is_stopped() {
                break;
            }
            if stops_early && has_fail(&lock(&shared)) {
                break;
            }
            let started = (self.now)();
            let record_id = lock(&shared).id.clone();
            let mut spec = self.spec(
                &process::part_output_path(&self.output_dir, &record_id, n),
                ProcessRole::Part,
                part_scope.clone(),
                (n, total),
            );
            spec.on_start = Some(self.on_start(shared.clone(), n, started.clone()));
            let outcome = run_part(spec).await;
            let part = finished_part(n, started, (self.now)(), &outcome);
            let cancelled = part.verdict == Verdict::NoVerdict(NoVerdictReason::Cancelled);
            let snapshot = {
                let mut record = lock(&shared);
                put_part(&mut record, part);
                if record.command.is_empty() {
                    record.command = outcome.command.clone();
                }
                record.cut = record.cut.saturating_add(outcome.output.cut_bytes);
                record.clone()
            };
            self.writer.transact(|tx| store_record(tx, &snapshot))?;
            if cancelled {
                break;
            }
        }
        let record = lock(&shared).clone();
        Ok(record)
    }

    /// Writes the part's start into the record before its output is read.
    fn on_start(
        &self,
        shared: Arc<Mutex<EvidenceRecord>>,
        n: u32,
        started: String,
    ) -> Box<dyn FnOnce(&PartStart) + Send> {
        let writer = self.writer.clone();
        Box::new(move |start: &PartStart| {
            let snapshot = {
                let mut record = lock(&shared);
                put_part(
                    &mut record,
                    PartRecord {
                        n,
                        verdict: Verdict::NoVerdict(NoVerdictReason::Interrupted),
                        reason: None,
                        exit: None,
                        started,
                        ended: None,
                        pid: Some(start.pid),
                        pid_start_time: Some(start.pid_start_time),
                        token: Some(start.token.clone()),
                        duration_ms: None,
                        summary: None,
                        failing: Vec::new(),
                    },
                );
                record.clone()
            };
            if let Err(error) = writer.transact(|tx| store_record(tx, &snapshot)) {
                log::error!("checks: recording the start of part {n}: {error:#}");
            }
        })
    }

    /// Combines the parts and writes the record's end in one transaction.
    fn finish(&self, mut record: EvidenceRecord) -> Result<EvidenceRecord> {
        // A part that never finished and was not run again does not count.
        record.parts.retain(|part| part.ended.is_some());
        let total = record.variant.parts;
        let verdicts: Vec<(u32, Verdict)> = record
            .parts
            .iter()
            .map(|part| (part.n, part.verdict.clone()))
            .collect();
        let passed = record
            .parts
            .iter()
            .filter(|part| part.verdict.is_pass())
            .count();
        let mut verdict = combine(&verdicts);
        if verdict.is_pass() && passed < total as usize {
            let reason = if self.stop.is_stopped() {
                NoVerdictReason::Cancelled
            } else {
                NoVerdictReason::Interrupted
            };
            verdict = Verdict::NoVerdict(reason);
        }
        record.exit = deciding_part(&record.parts, &verdict).and_then(|part| part.exit);
        record.reason = match &verdict {
            Verdict::NoVerdict(reason) => Some(reason.text()),
            _ => None,
        };
        record.verdict = verdict;
        record.failing = union_failing(record.parts.iter().map(|part| part.failing.as_slice()));
        record.summary = if total == 1 {
            record.parts.first().and_then(|part| part.summary.clone())
        } else {
            Some(format!("{passed} of {total} parts passed"))
        };
        record.duration_ms = Some(
            record
                .parts
                .iter()
                .filter_map(|part| part.duration_ms)
                .sum(),
        );
        record.ended = Some((self.now)());
        self.writer.transact(|tx| store_record(tx, &record))?;
        self.changed();
        Ok(record)
    }

    fn changed(&self) {
        if let Some(on_changed) = &self.on_changed {
            on_changed(&self.task_id);
        }
    }

    /// The scope of this run: the check's lack of one, `all`, or the scope script's answer.
    async fn scope(&self, staging: &Path) -> Result<Scoped, NoVerdictReason> {
        let script = match &self.decl.scope {
            None => {
                return Ok(Scoped {
                    scope: Scope::Unscoped,
                    reason: None,
                    items: None,
                });
            }
            Some(decl::Scope::Word(word)) if word == "all" => {
                return Ok(Scoped {
                    scope: Scope::All,
                    reason: None,
                    items: None,
                });
            }
            Some(decl::Scope::Script { script }) => script.clone(),
            Some(_) => {
                return Err(NoVerdictReason::InvalidScope(
                    "scope must be \"all\" or a table with a script".to_string(),
                ));
            }
        };
        let Some(base) = self.base_branch.as_deref() else {
            return Ok(all_without_base_branch());
        };
        let changed = match changed_files(&self.git, &self.tree, Some(base)).await {
            Ok(ChangedFiles::Changed(set)) => set,
            Ok(ChangedFiles::NoBaseBranch) => return Ok(all_without_base_branch()),
            Err(error) => {
                return Err(NoVerdictReason::InvalidScope(format!(
                    "listing the changed files: {error}"
                )));
            }
        };
        let changed_path = staging.join("changed-files.txt");
        let result_path = staging.join("scope-answer.json");
        std::fs::write(&changed_path, changed.as_lines()).map_err(|error| {
            NoVerdictReason::StartFailed(format!("writing the changed files: {error}"))
        })?;
        let spec = self.script_spec(
            &script,
            &staging.join("scope.log"),
            ProcessRole::Scope {
                changed_files: changed_path,
                result: result_path.clone(),
            },
            PartScope::Unscoped,
        );
        let outcome = run_part(spec).await;
        if let Some(reason) = ended_early(&outcome) {
            return Err(reason);
        }
        let exit = outcome.exit.ok_or(NoVerdictReason::Interrupted)?;
        let limit = self.result_limit();
        let file = read_capped(&result_path, limit);
        match parse_scope_answer(exit, file.as_deref(), limit)? {
            ScopeAnswer::Items { items, reason } => {
                let count = u32::try_from(items.len())
                    .map_err(|_| NoVerdictReason::InvalidScope("too many items".to_string()))?;
                let lines: String = items.iter().map(|item| format!("{item}\n")).collect();
                std::fs::write(staging.join(ITEMS_FILE), lines).map_err(|error| {
                    NoVerdictReason::StartFailed(format!("writing the items file: {error}"))
                })?;
                Ok(Scoped {
                    scope: Scope::Items {
                        digest: items_digest(&items),
                        count,
                    },
                    reason: Some(reason),
                    items: Some(items),
                })
            }
            ScopeAnswer::All { reason } => Ok(Scoped {
                scope: Scope::All,
                reason: Some(reason),
                items: None,
            }),
            ScopeAnswer::None { reason } => Ok(Scoped {
                scope: Scope::None {
                    reason: reason.clone(),
                },
                reason: Some(reason),
                items: None,
            }),
        }
    }

    /// The number of parts: 1, the declared count, or the plan script's answer.
    async fn part_count(&self, staging: &Path, scoped: &Scoped) -> Result<u32, NoVerdictReason> {
        let plan = match &self.decl.parts {
            None => return Ok(1),
            Some(decl::Parts::Count(count)) => {
                return u32::try_from(*count)
                    .ok()
                    .filter(|count| (1..=MAX_PARTS).contains(count))
                    .ok_or_else(|| {
                        NoVerdictReason::InvalidPlan(format!("parts must be from 1 to {MAX_PARTS}"))
                    });
            }
            Some(decl::Parts::Plan { plan }) => plan.clone(),
            Some(decl::Parts::Invalid(_)) => {
                return Err(NoVerdictReason::InvalidPlan(
                    "parts must be a count or a table with a plan".to_string(),
                ));
            }
        };
        let durations = staging.join("durations.json");
        let earlier = self
            .writer
            .transact(|tx| self.earlier_durations(tx))
            .map_err(|error| {
                NoVerdictReason::InvalidPlan(format!("reading earlier durations: {error:#}"))
            })?;
        std::fs::write(&durations, earlier.to_string()).map_err(|error| {
            NoVerdictReason::StartFailed(format!("writing the durations file: {error}"))
        })?;
        let part_scope = match (&scoped.scope, &scoped.items) {
            (Scope::Unscoped, _) => PartScope::Unscoped,
            (_, Some(_)) => PartScope::Items(staging.join(ITEMS_FILE)),
            _ => PartScope::All,
        };
        let output = staging.join("plan.log");
        let mut spec =
            self.script_spec(&plan, &output, ProcessRole::Plan { durations }, part_scope);
        spec.stderr = Some(staging.join("plan.errors.log"));
        let outcome = run_part(spec).await;
        if let Some(reason) = ended_early(&outcome) {
            return Err(reason);
        }
        let exit = outcome.exit.ok_or(NoVerdictReason::Interrupted)?;
        let stdout = read_capped(&output, 64 * 1024).unwrap_or_default();
        parse_plan_answer(exit, &String::from_utf8_lossy(&stdout))
    }

    /// `{"check": <name>, "records": [{"id", "tree_key", "parts", "durations_ms": [{"part",
    /// "ms"}]}]}`: the finished records of this check of the task, oldest first.
    fn earlier_durations(&self, tx: &mut StateTx) -> Result<serde_json::Value> {
        let records = scan_records_in(tx, &task_prefix(&self.task_id))?;
        let mut records: Vec<&EvidenceRecord> = records
            .iter()
            .filter(|record| record.check == self.decl.name && record.is_finished())
            .collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        let records: Vec<serde_json::Value> = records
            .into_iter()
            .map(|record| {
                let durations: Vec<serde_json::Value> = record
                    .parts
                    .iter()
                    .filter_map(|part| {
                        part.duration_ms
                            .map(|ms| json!({ "part": part.n, "ms": ms }))
                    })
                    .collect();
                json!({
                    "id": record.id,
                    "tree_key": record.tree_key,
                    "parts": record.variant.parts,
                    "durations_ms": durations,
                })
            })
            .collect();
        Ok(json!({ "check": self.decl.name, "records": records }))
    }

    /// The records of this task, check and tree whose whole key matches this run's.
    fn matching(&self, tx: &mut StateTx, variant: &Variant) -> Result<Vec<EvidenceRecord>> {
        let prefix = evidence_prefix(&self.task_id, &self.tree_key, &self.decl.name);
        Ok(scan_records_in(tx, &prefix)?
            .into_iter()
            .filter(|record| {
                record.task_id == self.task_id
                    && record.check == self.decl.name
                    && record.digest == self.digest
                    && record.tree_key == self.tree_key
                    && record.tree == self.tree_kind
                    && record.key_inputs == self.key_inputs
                    && record.variant == *variant
                    && !matches!(record.started_by, StartedBy::Landing { .. })
            })
            .collect())
    }

    /// Closes the unfinished records of `matching` as superseded.
    fn supersede(&self, tx: &mut StateTx, matching: &[EvidenceRecord]) -> Result<()> {
        for record in matching.iter().filter(|record| !record.is_finished()) {
            let mut closed = record.clone();
            closed.parts.retain(|part| part.ended.is_some());
            closed.ended = Some((self.now)());
            closed.verdict = Verdict::NoVerdict(NoVerdictReason::Cancelled);
            closed.reason = Some(SUPERSEDED.to_string());
            store_record(tx, &closed)?;
        }
        Ok(())
    }

    fn new_record(
        &self,
        id: &str,
        folder: &Path,
        started: String,
        variant: Variant,
    ) -> EvidenceRecord {
        EvidenceRecord {
            id: id.to_string(),
            task_id: self.task_id.clone(),
            check: self.decl.name.clone(),
            digest: self.digest.clone(),
            tree_key: self.tree_key.clone(),
            tree: self.tree_kind,
            key_inputs: self.key_inputs.clone(),
            variant,
            pin_commit: self.pin.commit().to_string(),
            master_path: self.master.to_string_lossy().into_owned(),
            command: Vec::new(),
            started,
            ended: None,
            duration_ms: None,
            verdict: Verdict::NoVerdict(NoVerdictReason::Interrupted),
            reason: None,
            exit: None,
            summary: None,
            failing: Vec::new(),
            scope_reason: None,
            started_by: self.started_by.clone(),
            parts: Vec::new(),
            output: Output::Path(folder.to_string_lossy().into_owned()),
            cut: 0,
        }
    }

    /// The scope script answered `none`: no part runs, and the record is a pass with the
    /// reason. It counts as the whole check in one part, so it satisfies the check.
    fn nothing_in_scope(
        &self,
        id: &str,
        folder: &Path,
        started: String,
        reason: String,
    ) -> Result<EvidenceRecord> {
        let variant = Variant {
            scope: Scope::None {
                reason: reason.clone(),
            },
            parts: 1,
            params: self.params.clone(),
        };
        let mut record = self.new_record(id, folder, started, variant.clone());
        record.verdict = Verdict::Pass;
        record.reason = Some(reason.clone());
        record.scope_reason = Some(reason);
        record.duration_ms = Some(0);
        record.ended = Some((self.now)());
        self.writer.transact(|tx| {
            if self.clean {
                let matching = self.matching(tx, &variant)?;
                self.supersede(tx, &matching)?;
            }
            store_record(tx, &record)
        })?;
        self.changed();
        Ok(record)
    }

    /// The run ended before any part: a finished no-verdict record with no parts.
    fn refused(
        &self,
        id: &str,
        folder: &Path,
        started: String,
        scope: Scope,
        scope_reason: Option<String>,
        reason: NoVerdictReason,
    ) -> Result<EvidenceRecord> {
        let variant = Variant {
            scope,
            parts: 0,
            params: self.params.clone(),
        };
        let mut record = self.new_record(id, folder, started, variant);
        record.reason = Some(reason.text());
        record.verdict = Verdict::NoVerdict(reason);
        record.scope_reason = scope_reason;
        record.duration_ms = Some(0);
        record.ended = Some((self.now)());
        self.writer.transact(|tx| store_record(tx, &record))?;
        self.changed();
        Ok(record)
    }

    fn result_limit(&self) -> usize {
        usize::try_from(self.settings.result_file_limit_kb.saturating_mul(1024))
            .unwrap_or(usize::MAX)
    }

    fn spec(
        &self,
        output: &Path,
        role: ProcessRole,
        scope: PartScope,
        (part, parts): (u32, u32),
    ) -> PartSpec {
        PartSpec {
            decl: self.decl.clone(),
            pin: self.pin.clone(),
            master: self.master.clone(),
            tree: self.tree.clone(),
            tree_kind: self.tree_kind,
            tree_key: self.tree_key.clone(),
            part,
            parts,
            scope,
            role,
            stderr: None,
            env: self.env.clone(),
            output: output.to_path_buf(),
            evidence: self.evidence.clone(),
            output_limit: Some(self.settings.output_limit_bytes()),
            result_limit: self.result_limit(),
            budget: process::budget(&self.decl, self.settings.default_budget_minutes),
            kill_grace: Duration::from_secs(u64::from(self.settings.kill_grace_seconds)),
            stop: self.stop.clone(),
            on_start: None,
        }
    }

    /// A spec running `script` from the pin in place of the check's own program, with the
    /// check's name, params and `unset`.
    fn script_spec(
        &self,
        script: &str,
        output: &Path,
        role: ProcessRole,
        scope: PartScope,
    ) -> PartSpec {
        let mut spec = self.spec(output, role, scope, (0, 0));
        spec.decl = CheckDecl {
            name: self.decl.name.clone(),
            run: Some(script.to_string()),
            params: self.decl.params.clone(),
            unset: self.decl.unset.clone(),
            budget_minutes: self.decl.budget_minutes,
            ..CheckDecl::default()
        };
        spec
    }
}

fn all_without_base_branch() -> Scoped {
    Scoped {
        scope: Scope::All,
        reason: Some(NO_BASE_BRANCH.to_string()),
        items: None,
    }
}

/// A UUID v7, so record ids order by time.
fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn lock(record: &Mutex<EvidenceRecord>) -> std::sync::MutexGuard<'_, EvidenceRecord> {
    record
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The SHA-256 of the items, one per line, in hex.
pub fn items_digest(items: &[String]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for item in items {
        hasher.update(item.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// A part keeps its verdict when it finished with a pass or a fail.
fn is_kept(part: &PartRecord) -> bool {
    part.ended.is_some() && matches!(part.verdict, Verdict::Pass | Verdict::Fail)
}

fn kept_parts(record: &EvidenceRecord) -> Vec<PartRecord> {
    record
        .parts
        .iter()
        .filter(|part| is_kept(part))
        .cloned()
        .collect()
}

fn has_fail(record: &EvidenceRecord) -> bool {
    record
        .parts
        .iter()
        .any(|part| is_kept(part) && part.verdict == Verdict::Fail)
}

/// The parts of `1..=total` that still need to run; none once a kept part failed and the check
/// stops at its first failure.
fn parts_to_run(record: &EvidenceRecord, total: u32, stops_early: bool) -> Vec<u32> {
    if stops_early && has_fail(record) {
        return Vec::new();
    }
    (1..=total)
        .filter(|n| {
            !record
                .parts
                .iter()
                .any(|part| part.n == *n && is_kept(part))
        })
        .collect()
}

/// Puts `part` in place of the part of the same number, keeping the parts in order.
fn put_part(record: &mut EvidenceRecord, part: PartRecord) {
    record.parts.retain(|other| other.n != part.n);
    record.parts.push(part);
    record.parts.sort_by_key(|part| part.n);
}

fn finished_part(n: u32, started: String, ended: String, outcome: &PartOutcome) -> PartRecord {
    let result = outcome.result.as_ref();
    PartRecord {
        n,
        verdict: outcome.verdict.clone(),
        reason: match &outcome.verdict {
            Verdict::NoVerdict(reason) => Some(reason.text()),
            _ => None,
        },
        exit: outcome.exit,
        started,
        ended: Some(ended),
        pid: outcome.start.as_ref().map(|start| start.pid),
        pid_start_time: outcome.start.as_ref().map(|start| start.pid_start_time),
        token: outcome.start.as_ref().map(|start| start.token.clone()),
        duration_ms: Some(u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX)),
        summary: result.map(|result| result.summary.clone()),
        failing: result
            .map(|result| result.failing.clone())
            .unwrap_or_default(),
    }
}

/// The part whose verdict the whole check took: the first fail, else the first part with that
/// no verdict, else the last part.
fn deciding_part<'a>(parts: &'a [PartRecord], verdict: &Verdict) -> Option<&'a PartRecord> {
    match verdict {
        Verdict::Pass => parts.last(),
        _ => parts
            .iter()
            .find(|part| part.verdict == *verdict)
            .or_else(|| parts.last()),
    }
}

/// Why a script's process ended without an exit of its own: stopped, out of time, or never
/// started.
fn ended_early(outcome: &PartOutcome) -> Option<NoVerdictReason> {
    match &outcome.verdict {
        Verdict::NoVerdict(
            reason @ (NoVerdictReason::Cancelled
            | NoVerdictReason::TimeBudgetReached
            | NoVerdictReason::ToolNotFound(_)
            | NoVerdictReason::StartFailed(_)
            | NoVerdictReason::Interrupted),
        ) => Some(reason.clone()),
        _ => None,
    }
}

/// At most `limit + 1` bytes of `path`, so a parser can tell it is too large; `None` when there
/// is no such file.
fn read_capped(path: &Path, limit: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Moves the files of `staging` into `folder` (replacing files of the same name) and removes
/// `staging`.
fn adopt(staging: &Path, folder: &Path) -> Result<()> {
    std::fs::create_dir_all(folder)
        .with_context(|| format!("creating the output folder {}", folder.display()))?;
    for entry in std::fs::read_dir(staging)
        .with_context(|| format!("reading the output folder {}", staging.display()))?
    {
        let entry = entry?;
        let target = folder.join(entry.file_name());
        std::fs::rename(entry.path(), &target)
            .with_context(|| format!("moving {} into place", target.display()))?;
    }
    remove_folder(staging);
    Ok(())
}

fn remove_folder(folder: &Path) {
    if let Err(error) = std::fs::remove_dir_all(folder) {
        log::warn!("checks: removing {}: {error}", folder.display());
    }
}
