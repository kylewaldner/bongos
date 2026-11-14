//! Snapshots and backup of a project's state: when snapshots are taken (a timer every interval
//! while the state changed, at least the minimum gap apart; at the end of a job; at once on
//! `project: backup now`), the push of the state branch with retries that double up to a cap,
//! thinning of old snapshots, the copy of run records to a folder of the user's choice, and the
//! status `project: backup status` shows. Every decision takes `now`, so tests run on a fixture
//! clock. Nothing here holds the state store while git runs, and nothing waits on a push but the
//! push itself.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::create::ProjectRepo;
use crate::project_values::{
    PUSH_RETRY_MAX_KEY, PUSH_RETRY_MIN_KEY, ProjectValueOverrides, RUN_RECORD_COPY_INTERVAL_KEY,
    SNAPSHOT_INTERVAL_KEY,
};
use crate::registries::{StateExporter, StateExporters};
use crate::settings::ProjectRepositorySettings;
use crate::snapshot::{self, Secrets, Withheld};
use crate::state::StateWriter;
use crate::state_branch::{self, Committed};

pub const DEFAULT_STATE_BRANCH: &str = "bongos/state";
pub const DEFAULT_SNAPSHOT_INTERVAL_MINUTES: u32 = 15;
pub const DEFAULT_MIN_SNAPSHOT_GAP_SECONDS: u32 = 60;
pub const DEFAULT_PUSH_RETRY_MIN_MINUTES: u32 = 5;
pub const DEFAULT_PUSH_RETRY_MAX_MINUTES: u32 = 60;
pub const DEFAULT_THINNING_AFTER_DAYS: u32 = 30;
pub const DEFAULT_RUN_RECORD_COPY_INTERVAL_MINUTES: u32 = 60;
/// How long a push may take before its git is stopped.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the host's timer asks whether a snapshot, push, thinning or copy is due.
pub const BACKUP_TICK: Duration = Duration::from_secs(30);

/// The sentence beside the snapshot interval.
pub const DRIVE_FAILURE_SENTENCE: &str =
    "If this drive fails, you lose up to this much run state and everything not pushed.";
/// The status of a project without a remote.
pub const NOT_BACKED_UP_NO_REMOTE: &str = "not backed up: no remote";
/// The sentence shown while no run record copy folder is set.
pub const RUN_RECORDS_LOCAL_SENTENCE: &str = "Run records stay on this machine only.";

/// The `meta` key of the last snapshot: its commit, time and generation.
pub const LAST_SNAPSHOT_KEY: &str = "last_snapshot";
/// The `meta` key of the rest of the backup's record.
const BACKUP_KEY: &str = "backup";
/// Where run records are, under each folder of `files/`.
const RUNS_DIR: &str = "runs";
const MAX_COPY_DEPTH: usize = 32;

/// What the backup reads from the `project_repository` settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupOptions {
    pub state_branch: String,
    pub default_snapshot_interval_minutes: u32,
    pub min_snapshot_gap_seconds: u32,
    pub push_retry_min_minutes: u32,
    pub push_retry_max_minutes: u32,
    pub thinning_after_days: u32,
    pub run_record_copy_dir: Option<PathBuf>,
    pub run_record_copy_interval_minutes: u32,
    pub secret_patterns: Vec<String>,
    /// Dotted keys the schema marks secret.
    pub secret_keys: Vec<String>,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            state_branch: DEFAULT_STATE_BRANCH.to_string(),
            default_snapshot_interval_minutes: DEFAULT_SNAPSHOT_INTERVAL_MINUTES,
            min_snapshot_gap_seconds: DEFAULT_MIN_SNAPSHOT_GAP_SECONDS,
            push_retry_min_minutes: DEFAULT_PUSH_RETRY_MIN_MINUTES,
            push_retry_max_minutes: DEFAULT_PUSH_RETRY_MAX_MINUTES,
            thinning_after_days: DEFAULT_THINNING_AFTER_DAYS,
            run_record_copy_dir: None,
            run_record_copy_interval_minutes: DEFAULT_RUN_RECORD_COPY_INTERVAL_MINUTES,
            secret_patterns: crate::DEFAULT_SECRET_PATTERNS.map(str::to_string).to_vec(),
            secret_keys: Vec::new(),
        }
    }
}

impl ProjectRepositorySettings {
    pub fn backup_options(&self) -> BackupOptions {
        BackupOptions {
            state_branch: self.state_branch.clone(),
            default_snapshot_interval_minutes: self.default_snapshot_interval_minutes,
            min_snapshot_gap_seconds: self.min_snapshot_gap_seconds,
            push_retry_min_minutes: self.push_retry_min_minutes,
            push_retry_max_minutes: self.push_retry_max_minutes,
            thinning_after_days: self.thinning_after_days,
            run_record_copy_dir: self.run_record_copy_dir.clone(),
            run_record_copy_interval_minutes: self.run_record_copy_interval_minutes,
            secret_patterns: self.secret_patterns.clone(),
            secret_keys: Vec::new(),
        }
    }
}

/// Why a snapshot is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotReason {
    /// The interval passed and the state changed.
    Timer,
    /// A job (a run, a landing) ended.
    JobEnd,
    /// `project: backup now`: taken at once, whatever the gap.
    Manual,
}

impl SnapshotReason {
    fn label(self) -> &'static str {
        match self {
            SnapshotReason::Timer => "timer",
            SnapshotReason::JobEnd => "job end",
            SnapshotReason::Manual => "backup now",
        }
    }
}

/// The last snapshot, as kept in `meta`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub commit: String,
    pub at: DateTime<Utc>,
    pub generation: u64,
}

/// What a request for a snapshot did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum SnapshotOutcome {
    /// A new snapshot is the tip of the state branch.
    Taken { commit: String },
    /// The exported files are those of the last snapshot; nothing was committed.
    Unchanged { commit: String },
    /// The state did not change since the last snapshot.
    NothingChanged,
    /// The last snapshot is too recent; the timer takes one later.
    TooSoon { seconds_left: u64 },
    /// The backup timer is stopped: the host is shutting down.
    Stopped,
}

/// The result of the last push.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    pub at: DateTime<Utc>,
    pub pushed: bool,
    pub message: String,
}

/// The result of the last copy of run records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyResult {
    pub at: DateTime<Utc>,
    pub copied: u64,
    pub problems: Vec<String>,
}

/// What the backup keeps in `meta` beside the last snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct BackupRecord {
    #[serde(default)]
    last_push: Option<PushResult>,
    /// Failed pushes since the last that went through.
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    next_push_at: Option<DateTime<Utc>>,
    /// The tip the remote was last given.
    #[serde(default)]
    pushed_commit: Option<String>,
    #[serde(default)]
    withheld: Vec<Withheld>,
    #[serde(default)]
    problems: Vec<String>,
    /// Why the last snapshot could not be taken.
    #[serde(default)]
    snapshot_error: Option<String>,
    #[serde(default)]
    run_record_copy: Option<CopyResult>,
}

/// The run record copy as `project: backup status` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecordCopyStatus {
    /// The folder, `None` when run records stay on this machine.
    pub dir: Option<PathBuf>,
    pub interval_minutes: u32,
    pub last: Option<CopyResult>,
    /// [`RUN_RECORDS_LOCAL_SENTENCE`] while no folder is set.
    pub sentence: Option<String>,
}

/// What `project: backup status` shows, computed by the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupStatus {
    pub last_snapshot: Option<SnapshotRecord>,
    pub snapshot_error: Option<String>,
    pub interval_minutes: u32,
    /// [`DRIVE_FAILURE_SENTENCE`].
    pub interval_sentence: String,
    pub state_branch: String,
    /// The remote as shown (credentials withheld), or [`NOT_BACKED_UP_NO_REMOTE`].
    pub remote: String,
    pub has_remote: bool,
    pub last_push: Option<PushResult>,
    pub next_push_at: Option<DateTime<Utc>>,
    pub unpushed_snapshots: u64,
    pub withheld: Vec<Withheld>,
    /// Exporters that failed or whose files were refused at the last snapshot.
    pub problems: Vec<String>,
    /// State rows of tasks the verified commit does not have.
    pub orphans: Vec<String>,
    pub run_record_copy: RunRecordCopyStatus,
}

impl BackupStatus {
    /// The text of the read-only status buffer.
    pub fn render(&self) -> String {
        let time = |at: &DateTime<Utc>| at.format("%Y-%m-%d %H:%M UTC").to_string();
        let mut lines = vec!["Backup status".to_string(), String::new()];
        lines.push(match &self.last_snapshot {
            Some(last) => format!(
                "Last snapshot: {} ({}, generation {})",
                time(&last.at),
                short(&last.commit),
                last.generation
            ),
            None => "Last snapshot: none yet".to_string(),
        });
        if let Some(error) = &self.snapshot_error {
            lines.push(format!("Last snapshot failed: {error}"));
        }
        lines.push(format!(
            "Snapshot interval: {} minutes. {}",
            self.interval_minutes, self.interval_sentence
        ));
        lines.push(format!("State branch: {}", self.state_branch));
        lines.push(format!("Remote: {}", self.remote));
        if self.has_remote {
            lines.push(match &self.last_push {
                Some(push) if push.pushed => format!("Last push: {} pushed", time(&push.at)),
                Some(push) => format!("Last push: {} failed: {}", time(&push.at), push.message),
                None => "Last push: none yet".to_string(),
            });
            if let Some(next) = &self.next_push_at {
                lines.push(format!("Next push attempt: {}", time(next)));
            }
        }
        lines.push(format!("Unpushed snapshots: {}", self.unpushed_snapshots));
        lines.push(String::new());
        if self.withheld.is_empty() {
            lines.push("Withheld values: none".to_string());
        } else {
            lines.push("Withheld values (the values are not in the snapshot):".to_string());
            for withheld in &self.withheld {
                lines.push(format!("  {}: {}", withheld.file, withheld.location));
            }
        }
        for problem in &self.problems {
            lines.push(format!("Problem: {problem}"));
        }
        if self.orphans.is_empty() {
            lines.push("Orphan state: none".to_string());
        } else {
            lines.push(format!(
                "Orphan state (tasks not in the definitions): {}",
                self.orphans.join(", ")
            ));
        }
        lines.push(String::new());
        let copy = &self.run_record_copy;
        match &copy.dir {
            Some(dir) => {
                lines.push(format!(
                    "Run record copy: {} every {} minutes",
                    dir.display(),
                    copy.interval_minutes
                ));
                lines.push(match &copy.last {
                    Some(last) => format!(
                        "Last run record copy: {} ({} files copied)",
                        time(&last.at),
                        last.copied
                    ),
                    None => "Last run record copy: none yet".to_string(),
                });
                if let Some(last) = &copy.last {
                    for problem in &last.problems {
                        lines.push(format!("Run record copy problem: {problem}"));
                    }
                }
            }
            None => lines.push(format!(
                "Run record copy: {}",
                copy.sentence
                    .as_deref()
                    .unwrap_or(RUN_RECORDS_LOCAL_SENTENCE)
            )),
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// A future that resolves when a push has taken too long.
pub type PushTimeout = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// The backup's part of a project's shared engine.
pub(crate) struct BackupEngine {
    options: Mutex<BackupOptions>,
    /// `None` reads the exporters registered in the app.
    exporters: Mutex<Option<Vec<Arc<dyn StateExporter>>>>,
    /// `None` reads the overrides registered in the app.
    overrides: Mutex<Option<Arc<ProjectValueOverrides>>>,
    /// Serializes snapshots and thinning, which both move the state branch.
    branch_lock: futures::lock::Mutex<()>,
    /// One push at a time; never held by a snapshot.
    push_lock: futures::lock::Mutex<()>,
    stopped: AtomicBool,
    push_timeout: Mutex<Option<PushTimeout>>,
}

impl Default for BackupEngine {
    fn default() -> Self {
        Self {
            options: Mutex::new(BackupOptions::default()),
            exporters: Mutex::new(None),
            overrides: Mutex::new(None),
            branch_lock: futures::lock::Mutex::new(()),
            push_lock: futures::lock::Mutex::new(()),
            stopped: AtomicBool::new(false),
            push_timeout: Mutex::new(None),
        }
    }
}

/// The real push timeout: [`PUSH_TIMEOUT`] on smol's timer, which the landing's backoff uses
/// too (see `landing::backoff`).
fn real_push_timeout() -> BoxFuture<'static, ()> {
    Box::pin(crate::landing::backoff(PUSH_TIMEOUT))
}

/// `remote` as shown: the credentials of a URL withheld.
pub(crate) fn shown_remote(remote: &str) -> String {
    if let Some((scheme, rest)) = remote.split_once("://")
        && let Some((userinfo, host)) = rest.split_once('@')
        && !userinfo.contains('/')
    {
        return format!("{scheme}://{}@{host}", snapshot::WITHHELD_SECRET);
    }
    remote.to_string()
}

/// Why `remote` cannot be pushed to, if it cannot: it would be read as an option, or holds a
/// character no remote name or URL has.
pub(crate) fn remote_problem(remote: &str) -> Option<&'static str> {
    if remote.trim().is_empty() {
        return Some("is empty");
    }
    if remote.starts_with('-') {
        return Some("starts with '-'");
    }
    if remote.chars().any(char::is_control) {
        return Some("holds a control character");
    }
    if remote.to_ascii_lowercase().starts_with("ext::") {
        return Some("uses the ext transport");
    }
    None
}

/// The pause before the next push after `failures` failed pushes in a row: `min` doubling to
/// `max`.
pub fn retry_delay(failures: u32, min_minutes: u32, max_minutes: u32) -> chrono::Duration {
    let min = u64::from(min_minutes.max(1));
    let max = u64::from(max_minutes.max(min_minutes).max(1));
    let doublings = failures.saturating_sub(1).min(32);
    let minutes = min.saturating_mul(1u64 << doublings).min(max);
    chrono::Duration::minutes(i64::try_from(minutes).unwrap_or(i64::MAX / 60_000))
}

impl ProjectRepo {
    pub fn set_backup_options(&self, options: BackupOptions) {
        *self.engine.backup.options.lock() = options;
    }

    pub fn backup_options(&self) -> BackupOptions {
        self.engine.backup.options.lock().clone()
    }

    /// Exports with `exporters` instead of those registered in the app.
    pub fn use_exporters(&self, exporters: Vec<Arc<dyn StateExporter>>) {
        *self.engine.backup.exporters.lock() = Some(exporters);
    }

    /// Reads project values with `overrides` instead of those registered in the app.
    pub fn use_value_overrides(&self, overrides: Arc<ProjectValueOverrides>) {
        *self.engine.backup.overrides.lock() = Some(overrides);
    }

    /// Gives up a push when `timeout` resolves instead of after [`PUSH_TIMEOUT`].
    pub fn set_push_timeout(&self, timeout: PushTimeout) {
        *self.engine.backup.push_timeout.lock() = Some(timeout);
    }

    pub(crate) fn value_overrides(&self) -> Option<Arc<ProjectValueOverrides>> {
        self.engine.backup.overrides.lock().clone()
    }

    /// Stops the backup timer: from now on no snapshot, push, thinning or copy starts. Part of
    /// the host's shutdown, before the host lock is dropped.
    pub fn stop_backups(&self) {
        self.engine.backup.stopped.store(true, Ordering::SeqCst);
    }

    pub fn backups_stopped(&self) -> bool {
        self.engine.backup.stopped.load(Ordering::SeqCst)
    }

    fn exporters(&self) -> Vec<Arc<dyn StateExporter>> {
        self.engine
            .backup
            .exporters
            .lock()
            .clone()
            .unwrap_or_else(|| StateExporters::global().all())
    }

    /// The snapshot interval in minutes (at least one).
    pub fn snapshot_interval_minutes(&self) -> u32 {
        let options = self.backup_options();
        self.project_value(SNAPSHOT_INTERVAL_KEY)
            .as_u32(options.default_snapshot_interval_minutes, 1)
    }

    /// The last snapshot recorded in `meta`.
    pub fn last_snapshot(&self) -> Result<Option<SnapshotRecord>> {
        let text = self
            .state
            .as_ref()
            .context("the project's state store is not open")?
            .read(|read| read.meta(LAST_SNAPSHOT_KEY))?;
        text.map(|text| serde_json::from_str(&text).context("reading the last snapshot"))
            .transpose()
    }

    fn backup_record(&self) -> Result<BackupRecord> {
        let text = self
            .state
            .as_ref()
            .context("the project's state store is not open")?
            .read(|read| read.meta(BACKUP_KEY))?;
        Ok(text
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default())
    }

    /// Changes the backup record in one transaction that is not a change of the state.
    fn update_backup_record(
        &self,
        writer: &StateWriter,
        change: impl FnOnce(&mut BackupRecord),
    ) -> Result<()> {
        writer.transact(|tx| {
            let mut record: BackupRecord = tx
                .meta(BACKUP_KEY)?
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            change(&mut record);
            tx.set_meta_quiet(BACKUP_KEY, &serde_json::to_string(&record)?)
        })
    }

    /// Takes a snapshot for `reason` at `now`, when one is due: a timer or job-end snapshot
    /// only when the state changed since the last and the minimum gap passed; a manual one at
    /// once. The state store is read once, then released before git runs.
    pub async fn snapshot_now(
        &self,
        reason: SnapshotReason,
        now: DateTime<Utc>,
    ) -> Result<SnapshotOutcome> {
        if self.backups_stopped() {
            return Ok(SnapshotOutcome::Stopped);
        }
        let _branch = self.engine.backup.branch_lock.lock().await;
        let writer = self.writer_or_error()?;
        let options = self.backup_options();
        if reason != SnapshotReason::Manual {
            let last = self.last_snapshot()?;
            let generation = writer.generation()?;
            match &last {
                Some(last) if last.generation == generation => {
                    return Ok(SnapshotOutcome::NothingChanged);
                }
                None if generation == 0 => return Ok(SnapshotOutcome::NothingChanged),
                _ => {}
            }
            if let Some(last) = &last {
                let gap = chrono::Duration::seconds(i64::from(options.min_snapshot_gap_seconds));
                let since = now - last.at;
                if since < gap {
                    return Ok(SnapshotOutcome::TooSoon {
                        seconds_left: u64::try_from((gap - since).num_seconds()).unwrap_or(0),
                    });
                }
            }
        }
        let result = self.take_snapshot(&writer, &options, reason, now).await;
        match &result {
            Ok(_) => {}
            Err(error) => {
                let message = format!("{error:#}");
                self.update_backup_record(&writer, |record| {
                    record.snapshot_error = Some(message);
                })
                .ok();
            }
        }
        result
    }

    async fn take_snapshot(
        &self,
        writer: &Arc<StateWriter>,
        options: &BackupOptions,
        reason: SnapshotReason,
        now: DateTime<Utc>,
    ) -> Result<SnapshotOutcome> {
        let reference = state_branch::state_ref(&options.state_branch)?;
        let secrets = Secrets::new(&options.secret_patterns, &options.secret_keys)?;
        let exporters = self.exporters();
        let exported = writer.read(|read| snapshot::export(read, &exporters, &secrets))?;
        let message = state_branch::snapshot_message(reason.label(), now, exported.generation);
        let repository = &self.location.repository;
        let mut outcome = None;
        for _ in 0..3 {
            let previous = state_branch::tip(&self.git, repository, &reference).await;
            match state_branch::commit_snapshot(
                &self.git,
                repository,
                &self.location.state,
                &reference,
                &exported.files,
                previous.as_deref(),
                &message,
            )
            .await?
            {
                Committed::Moved { commit } => {
                    outcome = Some(SnapshotOutcome::Taken { commit });
                    break;
                }
                Committed::Unchanged { commit } => {
                    outcome = Some(SnapshotOutcome::Unchanged { commit });
                    break;
                }
                Committed::LostRace => {
                    log::info!("the state branch moved during a snapshot; taking it again");
                }
            }
        }
        let Some(outcome) = outcome else {
            bail!("the state branch kept moving during the snapshot; it is taken next time");
        };
        let commit = match &outcome {
            SnapshotOutcome::Taken { commit } | SnapshotOutcome::Unchanged { commit } => {
                commit.clone()
            }
            _ => return Ok(outcome),
        };
        let record = SnapshotRecord {
            commit,
            at: now,
            generation: exported.generation,
        };
        let record_text = serde_json::to_string(&record)?;
        let withheld = exported.withheld.clone();
        let problems = exported.problems.clone();
        writer.transact(|tx| {
            tx.set_meta_quiet(LAST_SNAPSHOT_KEY, &record_text)?;
            let mut backup: BackupRecord = tx
                .meta(BACKUP_KEY)?
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            backup.withheld = withheld;
            backup.problems = problems;
            backup.snapshot_error = None;
            tx.set_meta_quiet(BACKUP_KEY, &serde_json::to_string(&backup)?)
        })?;
        Ok(outcome)
    }

    /// Pushes the state branch now, whatever the retry delay. Without a remote it does nothing.
    /// Returns whether it pushed. A failure is recorded with the next try's time; the snapshot
    /// stays local.
    pub async fn push_now(&self, now: DateTime<Utc>) -> Result<bool> {
        let Some(remote) = self.location.remote.clone() else {
            return Ok(false);
        };
        let _push = self.engine.backup.push_lock.lock().await;
        let writer = self.writer_or_error()?;
        let options = self.backup_options();
        let reference = state_branch::state_ref(&options.state_branch)?;
        let repository = &self.location.repository;
        let Some(tip) = state_branch::tip(&self.git, repository, &reference).await else {
            return Ok(false);
        };
        let record = self.backup_record()?;
        let outcome = match remote_problem(&remote) {
            Some(problem) => Err(format!("the remote {problem}; not pushing")),
            None => {
                // After thinning the remote's tip is no longer under ours: replace it, but only
                // while it is still what this machine last pushed.
                let lease = match &record.pushed_commit {
                    Some(pushed)
                        if *pushed != tip
                            && !self.git.is_ancestor(repository, pushed, &tip).await =>
                    {
                        Some(format!("{}:{pushed}", options.state_branch))
                    }
                    _ => None,
                };
                let timeout = self
                    .engine
                    .backup
                    .push_timeout
                    .lock()
                    .clone()
                    .map_or_else(real_push_timeout, |timeout| timeout());
                self.git
                    .push_branch(
                        repository,
                        &remote,
                        &options.state_branch,
                        lease.as_deref(),
                        timeout,
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        };
        let secrets = Secrets::new(&options.secret_patterns, &options.secret_keys).ok();
        let min = self
            .project_value(PUSH_RETRY_MIN_KEY)
            .as_u32(options.push_retry_min_minutes, 1);
        let max = self
            .project_value(PUSH_RETRY_MAX_KEY)
            .as_u32(options.push_retry_max_minutes, 1);
        let pushed = outcome.is_ok();
        self.update_backup_record(&writer, |record| match outcome {
            Ok(()) => {
                record.failures = 0;
                record.next_push_at = None;
                record.pushed_commit = Some(tip);
                record.last_push = Some(PushResult {
                    at: now,
                    pushed: true,
                    message: "pushed".to_string(),
                });
            }
            Err(message) => {
                // The remote's URL may hold credentials, and git's message the URL.
                let message = message.replace(&remote, &shown_remote(&remote));
                let message = match &secrets {
                    Some(secrets) => secrets.clean(&message),
                    None => message,
                };
                record.failures = record.failures.saturating_add(1);
                record.next_push_at = Some(now + retry_delay(record.failures, min, max));
                record.last_push = Some(PushResult {
                    at: now,
                    pushed: false,
                    message,
                });
            }
        })?;
        Ok(pushed)
    }

    /// Pushes when there is a remote, a snapshot it does not have, and no retry pause running.
    pub async fn push_if_due(&self, now: DateTime<Utc>) -> Result<bool> {
        if self.location.remote.is_none() || self.backups_stopped() {
            return Ok(false);
        }
        let record = self.backup_record()?;
        if record.next_push_at.is_some_and(|next| now < next) {
            return Ok(false);
        }
        let reference = state_branch::state_ref(&self.backup_options().state_branch)?;
        let tip = state_branch::tip(&self.git, &self.location.repository, &reference).await;
        if tip.is_none() || tip == record.pushed_commit {
            return Ok(false);
        }
        self.push_now(now).await
    }

    /// Thins snapshots older than `thinning_after_days` to the last of each day.
    pub async fn thin_snapshots(&self, now: DateTime<Utc>) -> Result<bool> {
        if self.backups_stopped() {
            return Ok(false);
        }
        let _branch = self.engine.backup.branch_lock.lock().await;
        let options = self.backup_options();
        let reference = state_branch::state_ref(&options.state_branch)?;
        let cutoff = now - chrono::Duration::days(i64::from(options.thinning_after_days));
        Ok(
            state_branch::thin(&self.git, &self.location.repository, &reference, cutoff)
                .await?
                .is_some(),
        )
    }

    /// `project: backup now`: a snapshot at once, and the push started on its own task so
    /// nothing waits for the remote.
    pub async fn backup_now(&self, now: DateTime<Utc>) -> Result<SnapshotOutcome> {
        let outcome = self.snapshot_now(SnapshotReason::Manual, now).await?;
        self.spawn_push(now, true);
        Ok(outcome)
    }

    /// Starts a push off the caller's path: `force` ignores the retry pause.
    pub(crate) fn spawn_push(&self, now: DateTime<Utc>, force: bool) {
        if self.location.remote.is_none() {
            return;
        }
        let repo = self.clone();
        smol::spawn(async move {
            let pushed = if force {
                repo.push_now(now).await
            } else {
                repo.push_if_due(now).await
            };
            if let Err(error) = pushed {
                log::error!("pushing the state branch: {error:#}");
            }
        })
        .detach();
    }

    /// One beat of the host's backup timer at `now`: a snapshot when the interval passed and
    /// the state changed, thinning, a push when due, a copy of run records when due. Each part's
    /// failure is recorded and the rest still runs.
    pub async fn backup_tick(&self, now: DateTime<Utc>) {
        if self.backups_stopped() {
            return;
        }
        let interval = chrono::Duration::minutes(i64::from(self.snapshot_interval_minutes()));
        let due = match self.last_snapshot() {
            Ok(Some(last)) => now - last.at >= interval,
            Ok(None) => true,
            Err(error) => {
                log::error!("reading the last snapshot: {error:#}");
                false
            }
        };
        if due && let Err(error) = self.snapshot_now(SnapshotReason::Timer, now).await {
            log::error!("taking a snapshot: {error:#}");
        }
        if let Err(error) = self.thin_snapshots(now).await {
            log::error!("thinning snapshots: {error:#}");
        }
        if let Err(error) = self.push_if_due(now).await {
            log::error!("pushing the state branch: {error:#}");
        }
        if let Err(error) = self.copy_run_records_if_due(now) {
            log::error!("copying run records: {error:#}");
        }
    }

    /// Copies new and changed run records when a copy folder is set and the interval passed.
    /// Returns whether it ran.
    pub fn copy_run_records_if_due(&self, now: DateTime<Utc>) -> Result<bool> {
        if self.backups_stopped() {
            return Ok(false);
        }
        let options = self.backup_options();
        let Some(dir) = options.run_record_copy_dir.clone() else {
            return Ok(false);
        };
        let interval = self
            .project_value(RUN_RECORD_COPY_INTERVAL_KEY)
            .as_u32(options.run_record_copy_interval_minutes, 1);
        let record = self.backup_record()?;
        if let Some(last) = &record.run_record_copy
            && now - last.at < chrono::Duration::minutes(i64::from(interval))
        {
            return Ok(false);
        }
        let writer = self.writer_or_error()?;
        let (copied, problems) = copy_run_records(&self.location.files, &dir);
        self.update_backup_record(&writer, |record| {
            record.run_record_copy = Some(CopyResult {
                at: now,
                copied,
                problems,
            });
        })?;
        Ok(true)
    }

    /// The status `project: backup status` shows, at `now`. Reads only.
    pub async fn backup_status(&self, _now: DateTime<Utc>) -> Result<BackupStatus> {
        let options = self.backup_options();
        let record = self.backup_record()?;
        let last_snapshot = self.last_snapshot()?;
        let (state_branch, unpushed) = match state_branch::state_ref(&options.state_branch) {
            Ok(reference) => (
                options.state_branch.clone(),
                state_branch::unpushed(
                    &self.git,
                    &self.location.repository,
                    &reference,
                    record.pushed_commit.as_deref(),
                )
                .await
                .unwrap_or(0),
            ),
            Err(error) => (format!("{error:#}"), 0),
        };
        let orphans = match self.state.as_ref().and_then(|state| state.writer()) {
            Some(writer) => {
                let definitions = writer.definitions();
                if definitions.top.is_some() {
                    let known = definitions
                        .nodes()
                        .iter()
                        .map(|node| node.definition.id.clone())
                        .collect();
                    writer.read(|read| read.orphans(&known))?
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        };
        let (remote, has_remote) = match &self.location.remote {
            Some(remote) => (shown_remote(remote), true),
            None => (NOT_BACKED_UP_NO_REMOTE.to_string(), false),
        };
        let copy_interval = self
            .project_value(RUN_RECORD_COPY_INTERVAL_KEY)
            .as_u32(options.run_record_copy_interval_minutes, 1);
        Ok(BackupStatus {
            last_snapshot,
            snapshot_error: record.snapshot_error,
            interval_minutes: self.snapshot_interval_minutes(),
            interval_sentence: DRIVE_FAILURE_SENTENCE.to_string(),
            state_branch,
            remote,
            has_remote,
            last_push: record.last_push,
            next_push_at: record.next_push_at,
            unpushed_snapshots: unpushed,
            withheld: record.withheld,
            problems: record.problems,
            orphans,
            run_record_copy: RunRecordCopyStatus {
                sentence: options
                    .run_record_copy_dir
                    .is_none()
                    .then(|| RUN_RECORDS_LOCAL_SENTENCE.to_string()),
                dir: options.run_record_copy_dir,
                interval_minutes: copy_interval,
                last: record.run_record_copy,
            },
        })
    }
}

/// Copies every file under `files/*/runs/` that is new or changed (by size and modification
/// time) to the same place under `destination`. Never deletes, never follows a symbolic link,
/// never panics: what could not be done is in the problems.
pub(crate) fn copy_run_records(files: &Path, destination: &Path) -> (u64, Vec<String>) {
    let mut problems = Vec::new();
    if let Some(problem) = destination_problem(files, destination) {
        problems.push(problem);
        return (0, problems);
    }
    let Ok(projects) = std::fs::read_dir(files) else {
        return (0, problems);
    };
    let mut copied = 0;
    let mut links = 0;
    for entry in projects.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            links += 1;
            continue;
        }
        if !kind.is_dir() {
            continue;
        }
        let runs = entry.path().join(RUNS_DIR);
        match std::fs::symlink_metadata(&runs) {
            Ok(metadata) if metadata.file_type().is_symlink() => links += 1,
            Ok(metadata) if metadata.is_dir() => {
                let relative = PathBuf::from(entry.file_name()).join(RUNS_DIR);
                copy_tree(
                    &runs,
                    &destination.join(&relative),
                    0,
                    &mut copied,
                    &mut links,
                    &mut problems,
                );
            }
            _ => {}
        }
    }
    if links > 0 {
        problems.push(format!("{links} symbolic links were not followed"));
    }
    (copied, problems)
}

/// Why run records cannot be copied to `destination`, if they cannot.
fn destination_problem(files: &Path, destination: &Path) -> Option<String> {
    let shown = destination.display();
    if !destination.is_absolute() {
        return Some(format!(
            "the run record copy folder {shown} is not an absolute path"
        ));
    }
    if destination
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Some(format!(
            "the run record copy folder {shown} has a `..` in it"
        ));
    }
    let Ok(metadata) = std::fs::metadata(destination) else {
        return Some(format!("the run record copy folder {shown} does not exist"));
    };
    if !metadata.is_dir() {
        return Some(format!(
            "the run record copy folder {shown} is not a folder"
        ));
    }
    let Ok(real_destination) = destination.canonicalize() else {
        return Some(format!("the run record copy folder {shown} cannot be read"));
    };
    if let Ok(real_files) = files.canonicalize()
        && real_destination.starts_with(&real_files)
    {
        return Some(format!(
            "the run record copy folder {shown} is inside the project's files"
        ));
    }
    None
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    copied: &mut u64,
    links: &mut u64,
    problems: &mut Vec<String>,
) {
    if depth > MAX_COPY_DEPTH {
        problems.push(format!("{} is nested too deep; left out", source.display()));
        return;
    }
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) => {
            problems.push(format!("reading {}: {error}", source.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                problems.push(format!("reading {}: {error}", path.display()));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            *links += 1;
        } else if metadata.is_dir() {
            copy_tree(&path, &target, depth + 1, copied, links, problems);
        } else if metadata.is_file() {
            match copy_if_changed(&path, &target, &metadata) {
                Ok(true) => *copied += 1,
                Ok(false) => {}
                Err(error) => problems.push(format!("copying {}: {error:#}", path.display())),
            }
        }
    }
}

/// Copies `source` to `target` unless `target` has its size and modification time; then gives
/// `target` the modification time of `source`.
fn copy_if_changed(source: &Path, target: &Path, metadata: &std::fs::Metadata) -> Result<bool> {
    let modified = metadata.modified().ok();
    if let Ok(existing) = std::fs::symlink_metadata(target) {
        if existing.file_type().is_symlink() || existing.is_dir() {
            bail!("{} is not a plain file", target.display());
        }
        if existing.len() == metadata.len() && existing.modified().ok() == modified {
            return Ok(false);
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(source, target).with_context(|| format!("writing {}", target.display()))?;
    if let Some(modified) = modified {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(target)
            .with_context(|| format!("opening {}", target.display()))?;
        file.set_modified(modified)
            .with_context(|| format!("dating {}", target.display()))?;
    }
    Ok(true)
}
