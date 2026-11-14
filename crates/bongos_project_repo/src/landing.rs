//! The landing transaction: a working copy's change becomes the next commit of master.
//!
//! 1. Commit what is uncommitted in the working copy. 2. Drop earlier amend commits and rebase
//! on the verified commit; a conflict stops here. 3. Run the amending checks, in a scratch copy
//! of the working copy's commit, and fast-forward the working copy to their commits. 5. Enter the
//! project's FIFO landing queue; its head holds `flock` on `state/landing.lock`. 6. Re-verify
//! master; when an earlier landing moved it, rebase (a change with amend commits goes back to
//! step 2 and re-enters at the back), then run the deterministic checks on the rebased tree.
//! 7. Squash into one commit whose parent is master. 8. State first, then the ref: one state
//! transaction records the staged state ops, the landing, the new verified commit, the closed
//! change row and `meta.landing_in_flight`, with the state apply hooks; only after it commits
//! does `main` move, by compare-and-swap. Then the checkout fast-forwards, its files are made
//! read-only again and the marker is cleared. 9. Master is pushed to the remote, if any.
//!
//! A crash between the state transaction and the ref move is completed at startup from the
//! recorded hashes (see `master.rs`); trailers are never used to recognize a landing.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::checks::{SCHEMA_CHECK, schema_problems};
use bongos_project_repo_model::layout::LANDING_LOCK_FILE;
use bongos_project_repo_model::{
    Author, AuthorKind, ChangeMeta, LandingFacts, Problem, commit_message, strip_trailers,
};
use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::changes::{AmendCommit, ChangeRow, ChangeState};
use crate::create::{MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, DEFAULT_IDENTITY_EMAIL, Identity, RebaseOutcome, TreeRef};
use crate::landing_queue::{Holder, LandingLock, LockRefused, QueueSlot};
use crate::loader::{definition_blobs, load_definition_tree};
use crate::master::MasterStatus;
use crate::state::StateWriter;

/// The ref master is.
pub const MAIN_REF: &str = "refs/heads/main";

/// What a lane, workflow or agent landing shows while master is split.
pub const WAITING_MASTER_SPLIT: &str = "waiting: master is split";
/// Why a person's landing is refused while master is split.
pub const MASTER_IS_SPLIT: &str = "master is split; resolve it first";
/// Why a change with amend commits cannot go back to step 2 now.
pub const UNSAVED_EDITS: &str = "working copy has unsaved edits";
/// Why a second `land` or `check` of a change waits while the first still runs.
pub const ALREADY_LANDING: &str = "the change is already being landed or checked";

/// The folder under `state/` that holds the scratch copies the amending checks run in.
const AMENDING_SCRATCH_DIR: &str = "amending";

/// The pauses between the tries of the move of `main` while it still is the old commit, so a
/// lock file another git holds for a moment is gone by the next try.
pub(crate) const REF_MOVE_BACKOFF: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(250),
    Duration::from_secs(1),
];

/// The `meta` key of the landing between its state transaction and its ref move.
pub(crate) const LANDING_IN_FLIGHT_KEY: &str = "landing_in_flight";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LandMode {
    Land,
    /// `project: check`: the checks only, without the queue.
    CheckOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandRequest {
    pub meta: ChangeMeta,
    pub mode: LandMode,
}

/// The settings a landing reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandOptions {
    pub squash: bool,
    pub queue_limit: Duration,
    pub keep_landed: bool,
    pub branch_prefix: String,
}

impl Default for LandOptions {
    fn default() -> Self {
        Self {
            squash: true,
            queue_limit: Duration::from_secs(120),
            keep_landed: false,
            branch_prefix: "work/".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum CheckOutcome {
    Pass,
    Fail {
        message: String,
    },
    /// Neither pass nor fail: the change waits with the reason.
    NoVerdict {
        reason: String,
    },
}

impl CheckOutcome {
    /// The word the `Bongos-Checks` trailer uses.
    pub fn word(&self) -> &'static str {
        match self {
            CheckOutcome::Pass => "pass",
            CheckOutcome::Fail { .. } => "fail",
            CheckOutcome::NoVerdict { .. } => "no-verdict",
        }
    }
}

/// A check's outcome on one tree. A result for another tree is never counted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check: String,
    pub tree: String,
    pub stamp: String,
    pub outcome: CheckOutcome,
}

/// What a landing ended as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingResult {
    Landed {
        commit: String,
    },
    Abandoned,
    /// The change is open and `land` was never called for it.
    NotRequested,
    /// `project: check`: the results on the change's tree.
    Checked {
        tree: String,
        checks: Vec<CheckResult>,
    },
    Refused {
        reasons: Vec<String>,
    },
    /// A textual conflict, left in the working copy with markers. No check ran.
    Conflict {
        files: Vec<String>,
    },
    /// Neither landed nor refused; the change waits with the reason.
    NoVerdict {
        reason: String,
    },
}

/// What the landing view and the change's row show while it lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingEvent {
    /// `position` changes are ahead of it.
    Queued {
        position: usize,
    },
    Checking,
    Waiting {
        reason: String,
    },
    Refused {
        reasons: Vec<String>,
    },
    Conflict {
        files: Vec<String>,
    },
    Landed {
        commit: String,
    },
}

/// What other surfaces hear about the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoEvent {
    VerifiedCommitChanged {
        commit: String,
    },
    /// Master was changed outside bongos while `change_id` was landing: `main` is that commit,
    /// bongos is at `verified`.
    MasterSplit {
        change_id: String,
        main: String,
        verified: String,
    },
}

/// What an amending check did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AmendOutcome {
    Unchanged,
    Amended { message: String },
    NoVerdict { reason: String },
}

/// A check that may write into the change before it lands (feature 52's rebalancer). Only Rust
/// code registers one. The landing engine commits what it writes as an amend commit and drops
/// those commits again before every rebase.
pub trait AmendingCheck: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// Files under `definitions/` it may write besides task files, relative to it.
    fn stamp_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// Looks at the checked-out working copy at `writable` and may change its files.
    fn amend(&self, writable: &Path) -> BoxFuture<'static, AmendOutcome>;
}

/// The landing between its state transaction and its ref move: `main` goes from `old` to
/// `commit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InFlight {
    pub(crate) change_id: String,
    pub(crate) commit: String,
    pub(crate) old: String,
}

/// Waits `pause` before git is tried again. The landing engine holds no gpui executor and runs
/// on whatever drives its futures (the tests use `block_on`), so it waits on the reactor's timer;
/// only a ref move that failed reaches it.
#[allow(clippy::disallowed_methods)]
pub(crate) async fn backoff(pause: Duration) {
    smol::Timer::after(pause).await;
}

/// What the compare-and-swap of `main` ended as.
pub(crate) enum RefMove {
    Moved,
    /// `main` is an outside commit.
    Split {
        main: String,
        error: String,
    },
    /// It failed every time and `main` is not known to have moved.
    Unknown(String),
}

/// One row of the `landings` table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LandingRecord {
    pub(crate) commit: String,
    pub(crate) old: String,
    pub(crate) subject: String,
    pub(crate) author: Author,
    /// Ops of the staged state that kept a newer value.
    #[serde(default)]
    pub(crate) notes: Vec<String>,
    /// The store's generation when it landed, which orders the history.
    pub(crate) generation: u64,
}

/// Where a test stops or interferes with a landing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbePoint {
    InQueue,
    BeforeStateTransaction,
    AfterStateTransaction,
    BeforeRefMove,
    AfterRefMove,
}

/// Returns true to simulate a crash at that point: the landing stops as a dead process would.
#[cfg(test)]
pub(crate) type Probe = Arc<dyn Fn(ProbePoint, String) -> BoxFuture<'static, bool> + Send + Sync>;

/// The error of a simulated crash.
pub(crate) const SIMULATED_CRASH: &str = "simulated crash";

fn event_for(result: &LandingResult) -> Option<LandingEvent> {
    match result {
        LandingResult::Landed { commit } => Some(LandingEvent::Landed {
            commit: commit.clone(),
        }),
        LandingResult::Refused { reasons } => Some(LandingEvent::Refused {
            reasons: reasons.clone(),
        }),
        LandingResult::Conflict { files } => Some(LandingEvent::Conflict {
            files: files.clone(),
        }),
        LandingResult::NoVerdict { reason } => Some(LandingEvent::Waiting {
            reason: reason.clone(),
        }),
        _ => None,
    }
}

/// Lane runs, workflows, agents and the healer wait while master is split; a person's landing
/// is refused instead.
fn waits_for_split(author: &Author) -> bool {
    matches!(
        author.kind(),
        AuthorKind::Agent | AuthorKind::Workflow | AuthorKind::Healer
    )
}

/// Who the commits of `author` are by.
pub(crate) fn author_identity(author: &Author) -> Identity {
    let name = author.name.trim();
    Identity {
        name: if name.is_empty() {
            Identity::default().name
        } else {
            name.to_string()
        },
        email: DEFAULT_IDENTITY_EMAIL.to_string(),
    }
}

/// The files among `files` (relative to `directory`) that still hold conflict markers.
fn files_with_markers(directory: &Path, files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|file| {
            std::fs::read_to_string(directory.join(file)).is_ok_and(|text| {
                text.lines()
                    .any(|line| line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> "))
            })
        })
        .cloned()
        .collect()
}

impl ProjectRepo {
    pub(crate) fn writer_or_error(&self) -> Result<Arc<StateWriter>> {
        self.state
            .as_ref()
            .and_then(|state| state.writer())
            .cloned()
            .context("this project is open in another bongos process; work there or close it")
    }

    pub fn set_land_options(&self, options: LandOptions) {
        *self.engine.options.lock() = options;
    }

    pub fn land_options(&self) -> LandOptions {
        self.engine.options.lock().clone()
    }

    /// Adds an amending check this project's landings run. Step 7 replaces this with the global
    /// registry of landing checks.
    pub fn add_amending_check(&self, check: Arc<dyn AmendingCheck>) {
        self.engine.amending.lock().push(check);
    }

    /// The events of every landing of this project from now on, with the change id.
    pub fn landing_events(&self) -> mpsc::UnboundedReceiver<(String, LandingEvent)> {
        let (sender, receiver) = mpsc::unbounded();
        self.engine.landing_events.lock().push(sender);
        receiver
    }

    /// The repository's events from now on.
    pub fn repo_events(&self) -> mpsc::UnboundedReceiver<RepoEvent> {
        let (sender, receiver) = mpsc::unbounded();
        self.engine.repo_events.lock().push(sender);
        receiver
    }

    /// The change holding the landing lock in this process.
    pub fn landing_holder(&self) -> Option<String> {
        self.engine.holder_id()
    }

    /// The stored check results of `change_id`, by check name.
    pub fn check_results(&self, change_id: &str) -> Result<Vec<CheckResult>> {
        let rows = self.read_state(|read| read.check_results(change_id))?;
        rows.into_iter()
            .map(|(check, record)| {
                toml::from_str(&record).with_context(|| format!("reading the result of {check}"))
            })
            .collect()
    }

    /// Lands `change_id` (or only checks it, by `request.mode`).
    pub fn land(
        &self,
        change_id: &str,
        request: LandRequest,
        cx: &gpui::App,
    ) -> gpui::Task<Result<LandingResult>> {
        let repo = self.clone();
        let change_id = change_id.to_string();
        gpui::AppContext::background_spawn(cx, async move {
            repo.land_now(&change_id, request, Utc::now()).await
        })
    }

    /// [`Self::land`] without an app, at `now`.
    pub async fn land_now(
        &self,
        change_id: &str,
        request: LandRequest,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let result = self.land_steps(change_id, &request, now).await;
        if let Ok(result) = &result
            && let Some(event) = event_for(result)
        {
            self.engine.emit(change_id, event);
        }
        self.engine.signal.notify();
        result
    }

    /// Resolves once `change_id` landed or was abandoned; at once when it already did, or when
    /// it was never requested.
    pub async fn wait_for_landing(&self, change_id: &str) -> Result<LandingResult> {
        loop {
            let wake = self.engine.signal.listen();
            if let Some(result) = self.landing_result(change_id)? {
                return Ok(result);
            }
            wake.await.ok();
        }
    }

    #[allow(unused_variables)]
    async fn probe(&self, point: ProbePoint, change_id: &str) -> bool {
        #[cfg(test)]
        {
            let probe = self.engine.probe.lock().clone();
            if let Some(probe) = probe {
                return probe(point, change_id.to_string()).await;
            }
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn set_probe(&self, probe: Probe) {
        *self.engine.probe.lock() = Some(probe);
    }

    async fn land_steps(
        &self,
        change_id: &str,
        request: &LandRequest,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let writer = self.writer_or_error()?;
        let Some(_landing) = self.engine.start_landing(change_id) else {
            return Ok(LandingResult::NoVerdict {
                reason: ALREADY_LANDING.to_string(),
            });
        };
        let mut row = self
            .change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
        match row.state {
            ChangeState::Landed => {
                return Ok(LandingResult::Landed {
                    commit: row.commit.unwrap_or_default(),
                });
            }
            ChangeState::Abandoned => return Ok(LandingResult::Abandoned),
            ChangeState::Open if request.mode == LandMode::Land => {
                row.state = ChangeState::Requested;
                row.updated = now;
                writer.transact(|tx| tx.set_change(&row))?;
            }
            ChangeState::Open | ChangeState::Requested => {}
        }
        let path = self.working_copy_path(&row.name);
        if !path.is_dir() {
            return Ok(LandingResult::NoVerdict {
                reason: format!("the working copy {} is not checked out", row.name),
            });
        }
        let identity = author_identity(&request.meta.author);
        let waits = waits_for_split(&row.author);

        if request.mode == LandMode::Land
            && let Some(ended) = self.master_gate(change_id, waits).await?
        {
            return Ok(ended);
        }

        // Step 1: a stopped rebase whose files are resolved continues; then what is
        // uncommitted is committed.
        if self.git.rebase_in_progress(&path).await {
            let unmerged = self.git.conflicted_files(&path).await?;
            let marked = files_with_markers(&path, &unmerged);
            if !marked.is_empty() {
                return Ok(LandingResult::Conflict { files: marked });
            }
            if let RebaseOutcome::Conflict { files } =
                self.git.rebase_continue(&path, &identity).await?
            {
                return Ok(LandingResult::Conflict { files });
            }
        }
        let subject = strip_trailers(&request.meta.subject);
        let subject = if subject.is_empty() {
            format!("change {}", row.name)
        } else {
            subject
        };
        self.git.commit_if_dirty(&path, &subject, &identity).await?;

        loop {
            // Edits typed while the change waited (for a split master, or to go round again
            // after an earlier landing) are never swept into an amend commit or a rebase.
            if !self.git.is_clean(&path).await? {
                return Ok(LandingResult::NoVerdict {
                    reason: UNSAVED_EDITS.to_string(),
                });
            }
            // Step 2.
            if let Some(ended) = self
                .drop_amendments(&writer, &mut row, &path, &identity)
                .await?
            {
                return Ok(ended);
            }
            let verified = self.shown_commit().await?.0;
            if let RebaseOutcome::Conflict { files } =
                self.git.rebase(&path, &verified, &identity).await?
            {
                return Ok(LandingResult::Conflict { files });
            }
            let mut based_on = verified;

            // Step 3.
            self.engine.emit(change_id, LandingEvent::Checking);
            if let Some(reason) = self.run_amending_checks(&writer, &mut row, &path).await? {
                return Ok(LandingResult::NoVerdict { reason });
            }
            if request.mode == LandMode::CheckOnly {
                let tip = self.git.rev_parse(&path, "HEAD").await?;
                let tree = self.git.tree_of(&path, &tip).await?;
                let checks = self.run_checks(&writer, change_id, &tree).await?;
                return Ok(LandingResult::Checked { tree, checks });
            }

            // Step 5.
            let slot = match self.enter_queue(&writer, change_id, waits).await? {
                Ok(slot) => slot,
                Err(ended) => return Ok(ended),
            };
            let entered = Instant::now();
            if self.probe(ProbePoint::InQueue, change_id).await {
                slot.keep();
                bail!(SIMULATED_CRASH);
            }

            // Abandoned (or landed elsewhere) while it waited for the lock.
            if request.mode == LandMode::Land
                && let Some(ended) = self.landing_result(change_id)?
            {
                return Ok(ended);
            }

            // Step 6.
            match self.check_master().await? {
                MasterStatus::Clean => {}
                MasterStatus::Split(_) if waits => {
                    drop(slot);
                    continue;
                }
                status => {
                    return Ok(LandingResult::Refused {
                        reasons: vec![status.refusal()],
                    });
                }
            }
            let verified_now = self.shown_commit().await?.0;
            if verified_now != based_on {
                if !self.git.is_clean(&path).await? {
                    return Ok(LandingResult::NoVerdict {
                        reason: UNSAVED_EDITS.to_string(),
                    });
                }
                if !row.amend_commits.is_empty() {
                    // Dropped before the rebase, then amended again on the rebased author part,
                    // and back in at the end of the queue.
                    drop(slot);
                    continue;
                }
                if let RebaseOutcome::Conflict { files } =
                    self.git.rebase(&path, &verified_now, &identity).await?
                {
                    return Ok(LandingResult::Conflict { files });
                }
                based_on = verified_now;
            }
            let tip = self.git.rev_parse(&path, "HEAD").await?;
            let tree = self.git.tree_of(&path, &tip).await?;
            let checks = self.run_checks(&writer, change_id, &tree).await?;
            let failures: Vec<String> = checks
                .iter()
                .filter_map(|result| match &result.outcome {
                    CheckOutcome::Fail { message } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            if !failures.is_empty() {
                return Ok(LandingResult::Refused { reasons: failures });
            }
            if let Some(reason) = checks.iter().find_map(|result| match &result.outcome {
                CheckOutcome::NoVerdict { reason } => Some(reason.clone()),
                _ => None,
            }) {
                return Ok(LandingResult::NoVerdict { reason });
            }
            let options = self.land_options();
            if entered.elapsed() > options.queue_limit {
                return Ok(LandingResult::NoVerdict {
                    reason: format!(
                        "the landing took longer than the queue limit of {} seconds; land again",
                        options.queue_limit.as_secs()
                    ),
                });
            }

            // Step 7.
            let repository = &self.location.repository;
            let new = if options.squash {
                let words: Vec<(String, String)> = checks
                    .iter()
                    .map(|result| (result.check.clone(), result.outcome.word().to_string()))
                    .collect();
                let facts = LandingFacts {
                    change_id,
                    checks: &words,
                    tree: &tree,
                };
                let message = commit_message(&request.meta, &facts);
                self.git
                    .commit_tree(repository, &tree, &[&based_on], &message, &identity)
                    .await?
            } else {
                tip
            };
            let main = self.git.rev_parse(repository, MAIN_BRANCH).await?;
            if main != based_on {
                return Ok(LandingResult::Refused {
                    reasons: vec![format!(
                        "master moved to {main} during the landing; land again"
                    )],
                });
            }

            // Steps 8 and 9.
            return self
                .finish_landing(&writer, slot, &mut row, &request.meta, &based_on, &new, now)
                .await;
        }
    }

    /// Refuses or waits while master is not where the verified commit says. `None` to go on.
    async fn master_gate(&self, change_id: &str, waits: bool) -> Result<Option<LandingResult>> {
        let mut told = false;
        loop {
            let wake = self.engine.signal.listen();
            if self.is_abandoned(change_id)? {
                return Ok(Some(LandingResult::Abandoned));
            }
            match self.check_master().await? {
                MasterStatus::Clean | MasterStatus::InFlight => return Ok(None),
                MasterStatus::Split(_) if waits => {
                    if !told {
                        self.engine.emit(
                            change_id,
                            LandingEvent::Waiting {
                                reason: WAITING_MASTER_SPLIT.to_string(),
                            },
                        );
                        told = true;
                    }
                    wake.await.ok();
                }
                status => {
                    return Ok(Some(LandingResult::Refused {
                        reasons: vec![status.refusal()],
                    }));
                }
            }
        }
    }

    fn is_abandoned(&self, change_id: &str) -> Result<bool> {
        Ok(self
            .change(change_id)?
            .is_some_and(|row| row.state == ChangeState::Abandoned))
    }

    /// Step 5: waits for the head of the queue and the landing lock. `Err` ends the landing.
    async fn enter_queue(
        &self,
        writer: &Arc<StateWriter>,
        change_id: &str,
        waits: bool,
    ) -> Result<Result<QueueSlot, LandingResult>> {
        writer.transact(|tx| tx.queue_push(change_id))?;
        let mut shown: Option<Option<usize>> = None;
        loop {
            let wake = self.engine.signal.listen();
            if self.split_master()?.is_some() {
                if waits {
                    if shown != Some(None) {
                        self.engine.emit(
                            change_id,
                            LandingEvent::Waiting {
                                reason: WAITING_MASTER_SPLIT.to_string(),
                            },
                        );
                        shown = Some(None);
                    }
                    wake.await.ok();
                    continue;
                }
                writer.transact(|tx| tx.queue_remove(change_id))?;
                return Ok(Err(LandingResult::Refused {
                    reasons: vec![MASTER_IS_SPLIT.to_string()],
                }));
            }
            let queue = writer.read(|read| read.landing_queue())?;
            let Some(position) = queue.iter().position(|id| id == change_id) else {
                // Abandoned while it waited, which took it out of the queue.
                if self.is_abandoned(change_id)? {
                    return Ok(Err(LandingResult::Abandoned));
                }
                writer.transact(|tx| tx.queue_push(change_id))?;
                continue;
            };
            if position == 0 {
                let mut holder = self.engine.holder.lock();
                if holder.is_none() {
                    let path = self.location.state.join(LANDING_LOCK_FILE);
                    match LandingLock::try_take(&path) {
                        Ok(lock) => {
                            *holder = Some(Holder::new(change_id, lock));
                            drop(holder);
                            return Ok(Ok(QueueSlot::new(
                                self.engine.clone(),
                                writer.clone(),
                                change_id,
                            )));
                        }
                        Err(refused) => {
                            drop(holder);
                            writer.transact(|tx| tx.queue_remove(change_id))?;
                            return match refused {
                                LockRefused::HeldBy(pid) => Ok(Err(LandingResult::NoVerdict {
                                    reason: format!(
                                        "another process holds the landing lock ({})",
                                        pid.map_or("pid unknown".to_string(), |pid| format!(
                                            "pid {pid}"
                                        ))
                                    ),
                                })),
                                LockRefused::Failed(error) => Err(error),
                            };
                        }
                    }
                }
            }
            if shown != Some(Some(position)) {
                self.engine
                    .emit(change_id, LandingEvent::Queued { position });
                shown = Some(Some(position));
            }
            wake.await.ok();
        }
    }

    /// Takes the amend commits off the branch (they sit on top of the author's commits).
    async fn drop_amendments(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
        identity: &Identity,
    ) -> Result<Option<LandingResult>> {
        let (Some(first), Some(last)) = (row.amend_commits.first(), row.amend_commits.last())
        else {
            return Ok(None);
        };
        let (first, last) = (first.commit.clone(), last.commit.clone());
        let tip = self.git.rev_parse(path, "HEAD").await?;
        if self.git.is_ancestor(path, &last, &tip).await {
            if !self.git.is_clean(path).await? {
                return Ok(Some(LandingResult::NoVerdict {
                    reason: UNSAVED_EDITS.to_string(),
                }));
            }
            let base = format!("{first}^");
            if tip == last {
                self.git.reset_keep(path, &base).await?;
            } else if let RebaseOutcome::Conflict { files } =
                self.git.rebase_onto(path, &base, &last, identity).await?
            {
                return Ok(Some(LandingResult::Conflict { files }));
            }
        }
        row.amend_commits.clear();
        writer.transact(|tx| tx.set_change(row))?;
        Ok(None)
    }

    /// Step 3's amending checks; `Some` is a reason for no verdict.
    async fn run_amending_checks(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
    ) -> Result<Option<String>> {
        let checks = self.engine.amending.lock().clone();
        if checks.is_empty() {
            return Ok(None);
        }
        // The checks run in a scratch copy of the working copy's commit, never in the working
        // copy: what the person types while a check runs is neither committed nor discarded.
        let scratch = self
            .location
            .state
            .join(AMENDING_SCRATCH_DIR)
            .join(&row.name);
        self.remove_scratch(path, &scratch).await?;
        if let Some(parent) = scratch.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let result = self
            .amend_in_scratch(writer, row, path, &scratch, checks)
            .await;
        if let Err(error) = self.remove_scratch(path, &scratch).await {
            log::error!("removing the amending scratch copy: {error:#}");
        }
        result
    }

    async fn remove_scratch(&self, path: &Path, scratch: &Path) -> Result<()> {
        if scratch.exists() {
            self.git.worktree_remove(path, scratch).await?;
        }
        Ok(())
    }

    async fn amend_in_scratch(
        &self,
        writer: &StateWriter,
        row: &mut ChangeRow,
        path: &Path,
        scratch: &Path,
        checks: Vec<Arc<dyn AmendingCheck>>,
    ) -> Result<Option<String>> {
        if !self.git.is_clean(path).await? {
            return Ok(Some(UNSAVED_EDITS.to_string()));
        }
        let mut head = self.git.rev_parse(path, "HEAD").await?;
        self.git.worktree_add_detached(path, scratch, &head).await?;
        for check in checks {
            let mut amended = false;
            loop {
                match check.amend(scratch).await {
                    AmendOutcome::Unchanged => break,
                    AmendOutcome::NoVerdict { reason } => {
                        return Ok(Some(format!("{}: {reason}", check.name())));
                    }
                    AmendOutcome::Amended { message } => {
                        if amended {
                            return Ok(Some(format!("{}: check keeps amending", check.name())));
                        }
                        let subject = format!("amend: {} {message}", check.name());
                        let Some(commit) = self
                            .git
                            .commit_if_dirty(scratch, &subject, &Identity::default())
                            .await?
                        else {
                            break;
                        };
                        // The working copy follows only when the person committed nothing
                        // meanwhile and edited no file the amend commit changes; `--keep`
                        // leaves their other edits as they are.
                        if self.git.rev_parse(path, "HEAD").await? != head
                            || self.git.reset_keep(path, &commit).await.is_err()
                        {
                            return Ok(Some(UNSAVED_EDITS.to_string()));
                        }
                        head = commit.clone();
                        row.amend_commits.push(AmendCommit {
                            commit,
                            check: check.name().to_string(),
                        });
                        writer.transact(|tx| tx.set_change(row))?;
                        amended = true;
                    }
                }
            }
        }
        // Edits typed while the checks ran are still in the working copy, uncommitted.
        if !self.git.is_clean(path).await? {
            return Ok(Some(UNSAVED_EDITS.to_string()));
        }
        Ok(None)
    }

    /// The problems the schema check finds in the tree `treeish`.
    pub(crate) async fn schema_problems_at(&self, treeish: &str) -> Result<Vec<Problem>> {
        let (paths, files) =
            definition_blobs(&self.git, &self.location.repository, treeish).await?;
        let extra: Vec<String> = self
            .engine
            .amending
            .lock()
            .iter()
            .flat_map(|check| check.stamp_paths())
            .collect();
        Ok(schema_problems(
            paths.iter().map(String::as_str),
            files,
            &extra,
        ))
    }

    /// The deterministic checks on `tree`, each result stored against the tree.
    async fn run_checks(
        &self,
        writer: &StateWriter,
        change_id: &str,
        tree: &str,
    ) -> Result<Vec<CheckResult>> {
        let problems = self.schema_problems_at(tree).await?;
        let outcome = if problems.is_empty() {
            CheckOutcome::Pass
        } else {
            let list: Vec<String> = problems.iter().map(ToString::to_string).collect();
            CheckOutcome::Fail {
                message: format!("{SCHEMA_CHECK}: {}", list.join("; ")),
            }
        };
        let result = CheckResult {
            check: SCHEMA_CHECK.to_string(),
            tree: tree.to_string(),
            stamp: tree.to_string(),
            outcome,
        };
        let record = toml::to_string(&result).context("serializing a check result")?;
        writer.transact(|tx| tx.set_check_result(change_id, SCHEMA_CHECK, &record))?;
        Ok(vec![result])
    }

    /// Makes the hooks see the definitions of `commit` as the old ones.
    pub(crate) async fn ensure_definitions(
        &self,
        writer: &StateWriter,
        commit: &str,
    ) -> Result<()> {
        if writer.definitions_commit().as_deref() == Some(commit) {
            return Ok(());
        }
        let definitions = load_definition_tree(
            &self.git,
            &self.location.repository,
            &TreeRef::Commit(CommitId(commit.to_string())),
        )
        .await?;
        writer.set_definitions_at(commit, Arc::new(definitions));
        Ok(())
    }

    /// Steps 8 and 9: state first, then the ref.
    #[allow(clippy::too_many_arguments)]
    async fn finish_landing(
        &self,
        writer: &Arc<StateWriter>,
        slot: QueueSlot,
        row: &mut ChangeRow,
        meta: &ChangeMeta,
        old: &str,
        new: &str,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let change_id = row.change_id.clone();
        let repository = self.location.repository.clone();
        let definitions = Arc::new(
            load_definition_tree(
                &self.git,
                &repository,
                &TreeRef::Commit(CommitId(new.to_string())),
            )
            .await?,
        );
        self.ensure_definitions(writer, old).await?;
        let staged = writer
            .read(|read| read.staged(&change_id))?
            .unwrap_or_default();
        let in_flight = toml::to_string(&InFlight {
            change_id: change_id.clone(),
            commit: new.to_string(),
            old: old.to_string(),
        })
        .context("serializing the landing in flight")?;
        let mut landed = row.clone();
        landed.state = ChangeState::Landed;
        landed.commit = Some(new.to_string());
        landed.updated = now;
        self.probe(ProbePoint::BeforeStateTransaction, &change_id)
            .await;
        let transaction = writer.transact_with(&meta.author, &staged, |tx| {
            // Read again inside the transaction: an abandon that committed first wins.
            let current = tx.change(&change_id)?;
            if current.as_ref().map(|row| row.state) != Some(ChangeState::Requested) {
                return Ok(Some(match current {
                    Some(ChangeRow {
                        state: ChangeState::Landed,
                        commit,
                        ..
                    }) => LandingResult::Landed {
                        commit: commit.unwrap_or_default(),
                    },
                    Some(ChangeRow {
                        state: ChangeState::Open,
                        ..
                    }) => LandingResult::NotRequested,
                    _ => LandingResult::Abandoned,
                }));
            }
            let notes = if staged.is_empty() {
                Vec::new()
            } else {
                let notes = tx.apply_ops(&staged)?;
                tx.drop_staged(&change_id)?;
                notes
            };
            let record = LandingRecord {
                commit: new.to_string(),
                old: old.to_string(),
                subject: strip_trailers(&meta.subject),
                author: meta.author.clone(),
                notes,
                generation: tx.generation()?,
            };
            tx.add_landing(
                &change_id,
                &toml::to_string(&record).context("serializing a landing")?,
            )?;
            tx.set_verified_commit(new, definitions.clone())?;
            tx.set_change(&landed)?;
            tx.set_meta(LANDING_IN_FLIGHT_KEY, &in_flight)?;
            tx.queue_remove(&change_id)?;
            Ok(None)
        });
        match transaction {
            Ok(None) => {}
            Ok(Some(ended)) => return Ok(ended),
            Err(error) => {
                // Rolled back: `main` never moved and the verified commit is still `old`.
                return Ok(LandingResult::NoVerdict {
                    reason: format!("{error:#}"),
                });
            }
        }
        *row = landed;
        self.engine
            .emit_repo(crate::RepoEvent::VerifiedCommitChanged {
                commit: new.to_string(),
            });
        if self
            .probe(ProbePoint::AfterStateTransaction, &change_id)
            .await
        {
            slot.keep();
            bail!(SIMULATED_CRASH);
        }
        self.probe(ProbePoint::BeforeRefMove, &change_id).await;
        // From here the state says landed, so nothing returns an error: what fails is logged
        // and left to the recovery at the next open.
        match self.move_main(&repository, new, old).await {
            RefMove::Moved => {}
            RefMove::Split { main, error } => {
                log::error!("master was changed outside bongos while {change_id} landed: {error}");
                // The lock stays held until a person resolves the split.
                slot.keep();
                if let Err(error) = self.record_split(writer, &change_id, &main, new, old).await {
                    log::error!("recording the split master: {error:#}");
                }
                self.remove_working_copy(row).await;
                return Ok(LandingResult::Landed {
                    commit: new.to_string(),
                });
            }
            RefMove::Unknown(error) => {
                // The marker of the landing in flight stays and the lock is released: the
                // next `check_master` (the ref watch, or the next landing's step 6) finishes
                // the move, and nothing lands on top before it did.
                log::error!("moving master to {new} for {change_id}: {error}");
                drop(slot);
                return Ok(LandingResult::Landed {
                    commit: new.to_string(),
                });
            }
        }
        if self.probe(ProbePoint::AfterRefMove, &change_id).await {
            slot.keep();
            bail!(SIMULATED_CRASH);
        }
        if let Err(error) = self.finish_checkout(writer).await {
            log::error!("finishing the checkout after {change_id} landed: {error:#}");
        }
        drop(slot);
        self.remove_working_copy(row).await;
        self.push_main().await;
        Ok(LandingResult::Landed {
            commit: new.to_string(),
        })
    }

    /// Moves `main` from `old` to `new` by compare-and-swap. A failure while `main` still is
    /// `old` (a lock file another git holds for a moment) is tried again after a pause.
    pub(crate) async fn move_main(&self, repository: &Path, new: &str, old: &str) -> RefMove {
        let mut last = String::new();
        let pauses = std::iter::once(None).chain(REF_MOVE_BACKOFF.into_iter().map(Some));
        for pause in pauses {
            if let Some(pause) = pause {
                backoff(pause).await;
            }
            let error = match self
                .git
                .update_ref_cas(repository, MAIN_REF, new, old)
                .await
            {
                Ok(()) => return RefMove::Moved,
                Err(error) => error.to_string(),
            };
            match self.git.rev_parse(repository, MAIN_BRANCH).await {
                Ok(main) if main == new => return RefMove::Moved,
                Ok(main) if main == old => last = error,
                Ok(main) => return RefMove::Split { main, error },
                Err(read) => last = format!("{error}; reading main: {read}"),
            }
        }
        RefMove::Unknown(last)
    }

    /// After the ref moved: the checkout follows, its files are read-only again, and the
    /// marker of the landing in flight (and of a split master) is cleared.
    /// The marker is cleared even when the checkout could not be updated: `main` moved. When
    /// clearing it fails, the next `check_master` clears it.
    pub(crate) async fn finish_checkout(&self, writer: &StateWriter) -> Result<()> {
        if let Err(error) = self.git.update_checkout(&self.location.repository).await {
            log::error!("updating the checkout of master: {error}");
        } else if let Err(error) = self.protect_master_files() {
            log::error!("making master read-only: {error}");
        }
        writer.transact(|tx| {
            tx.delete_meta(LANDING_IN_FLIGHT_KEY)?;
            tx.delete_meta(crate::master::MASTER_SPLIT_KEY)
        })
    }

    /// Removes a landed change's worktree and branch unless the keep setting is on.
    pub(crate) async fn remove_working_copy(&self, row: &ChangeRow) {
        if self.land_options().keep_landed {
            return;
        }
        let repository = &self.location.repository;
        let path = self.working_copy_path(&row.name);
        if let Err(error) = self.git.worktree_remove(repository, &path).await {
            log::error!("removing the landed working copy {}: {error}", row.name);
        }
        if self.git.branch_exists(repository, &row.branch).await
            && let Err(error) = self.git.branch_delete(repository, &row.branch).await
        {
            log::error!("deleting the landed branch {}: {error}", row.branch);
        }
    }
}
