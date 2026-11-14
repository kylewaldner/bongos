//! The landing transaction: a working copy's change becomes the next commit of master.
//!
//! 1. Commit what is uncommitted in the working copy. 2. Drop earlier amend commits and rebase
//! on the verified commit; a conflict stops here. 3. Run the amending checks (`amending.rs`),
//! then the built-in checks and the before-queue checks on the final tree (`check_run.rs`); a
//! failure refuses and a no verdict leaves the change waiting. 4. Wait, outside the queue and
//! its lock, until check 9 approves the change (`approval.rs`); it is judged again in the queue
//! on the tree that lands. 5. Enter the project's FIFO
//! landing queue; its head holds `flock` on `state/landing.lock`. 6. Re-verify master; when an
//! earlier landing moved it, rebase (a change with amend commits goes back to step 2 first). A
//! before-queue or amending check whose stamp differs on the tree now sends the change back to
//! step 3, and it re-enters the queue at the back; otherwise the in-queue checks run on the
//! rebased tree. 7. Squash into one commit whose parent is master. 8. State first, then the ref: one state
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
use bongos_project_repo_model::checks::{CheckPhase, SizeLimits, strip_move_meta};
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, LANDING_LOCK_FILE};
use bongos_project_repo_model::{
    Author, AuthorKind, ChangeMeta, DefinitionTree, LandingFacts, Problem, commit_message,
    strip_trailers, summarize,
};
use chrono::{DateTime, Utc};
use futures::channel::mpsc;
#[cfg(test)]
use futures::future::BoxFuture;
use futures::future::Either;
use serde::{Deserialize, Serialize};

use crate::amending::Amended;
use crate::approval::{NEEDS_A_WINDOW, PersonGrant, approved_line};
use crate::builtin_checks::{schema_check, tree_facts};
use crate::changes::{ChangeRow, ChangeState};
use crate::check_run::{CheckResult, verdict};
use crate::create::{MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, DEFAULT_IDENTITY_EMAIL, Identity, RebaseOutcome, TreeRef};
use crate::landing_queue::{Holder, LandingLock, LockRefused, QueueSlot};
use crate::loader::load_definition_tree;
use crate::master::MasterStatus;
use crate::router::{check_file_op, run_file_ops, staged_file_ops};
use crate::state::StateWriter;
use bongos_work_model::commands::Context as WorkContext;

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

/// The secret patterns a landing uses when no setting gives them (`default.json` has the same).
pub const DEFAULT_SECRET_PATTERNS: [&str; 6] = [
    "AKIA[0-9A-Z]{16}",
    "gh[pousr]_[A-Za-z0-9]{36}",
    "-----BEGIN [A-Z ]*PRIVATE KEY-----",
    "xox[abprs]-[A-Za-z0-9-]{10,}",
    "sk-ant-[A-Za-z0-9_-]{20,}",
    "sk-[A-Za-z0-9]{32,}",
];

/// The pauses between the tries of the move of `main` while it still is the old commit, so a
/// lock file another git holds for a moment is gone by the next try.
pub(crate) const REF_MOVE_BACKOFF: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(250),
    Duration::from_secs(1),
];

/// The `meta` key of the landing between its state transaction and its ref move.
pub(crate) const LANDING_IN_FLIGHT_KEY: &str = "landing_in_flight";
/// The file ops of landed changes not yet made: those of the landing in flight, and any that
/// failed after a landing. The recovery makes them again.
pub(crate) const LANDING_FILE_MOVES_KEY: &str = "landing_file_moves";

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
    /// The limits of the `size-and-kind` check.
    pub size_limits: SizeLimits,
    /// The patterns of the `secrets` check.
    pub secret_patterns: Vec<String>,
}

impl Default for LandOptions {
    fn default() -> Self {
        Self {
            squash: true,
            queue_limit: Duration::from_secs(120),
            keep_landed: false,
            branch_prefix: "work/".to_string(),
            size_limits: SizeLimits::default(),
            secret_patterns: DEFAULT_SECRET_PATTERNS.map(str::to_string).to_vec(),
        }
    }
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

/// The landing between its state transaction and its ref move: `main` goes from `old` to
/// `commit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InFlight {
    pub(crate) change_id: String,
    pub(crate) commit: String,
    pub(crate) old: String,
}

/// The file ops of a landing, as kept for its recovery.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct LandingFiles {
    pub(crate) ops: Vec<crate::router::FileOp>,
}

/// Waits `pause` before git is tried again. Only a ref move that failed (and the resolution of a
/// split master that saw `main` move) reaches it.
///
/// `smol::Timer` is the one timer allowed here: the lint steers code to gpui's executor timer,
/// but the landing engine holds no gpui executor. `land` runs it on a background task, and
/// `land_now`, the recovery and the ref watch run it on whatever drives their futures (the tests
/// use `block_on`, which has no gpui executor at all). smol's timer is driven by its own reactor
/// thread, so it wakes under any executor; injecting a sleep would only move the same call into
/// every caller.
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
    /// Who or what approved it ("approved: ...").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approval: Option<String>,
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

/// The meta a landing commits with: any `Bongos-Move` text stripped, and a subject generated
/// from the change summary when the author wrote none.
fn landing_meta(
    meta: &ChangeMeta,
    summary: &bongos_project_repo_model::ChangeSummary,
) -> ChangeMeta {
    let mut meta = strip_move_meta(meta);
    if strip_trailers(&meta.subject).trim().is_empty() {
        meta.subject = summary.generated_subject();
    }
    meta
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

    /// Lands `change_id` (or only checks it, by `request.mode`).
    pub fn land(
        &self,
        change_id: &str,
        request: LandRequest,
        cx: &gpui::App,
    ) -> gpui::Task<Result<LandingResult>> {
        self.use_registered_checks(cx);
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
        self.land_granted(change_id, request, None, now).await
    }

    /// `project: land` by the person whose context `ctx` is: refused unless it still holds the
    /// attestation of a person at a window. On the person's own change it records a `PersonLand`
    /// approval of what the working copy holds once committed; then it lands like [`Self::land`].
    pub fn land_by_person(
        &self,
        change_id: &str,
        request: LandRequest,
        ctx: WorkContext,
        cx: &gpui::App,
    ) -> gpui::Task<Result<LandingResult>> {
        self.use_registered_checks(cx);
        let repo = self.clone();
        let change_id = change_id.to_string();
        gpui::AppContext::background_spawn(cx, async move {
            repo.land_as_person(&change_id, request, ctx, Utc::now())
                .await
        })
    }

    /// [`Self::land_by_person`] without an app, at `now`.
    pub async fn land_as_person(
        &self,
        change_id: &str,
        request: LandRequest,
        mut ctx: WorkContext,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let grant = PersonGrant::from_context(&mut ctx).ok_or_else(|| anyhow!(NEEDS_A_WINDOW))?;
        self.land_granted(change_id, request, Some(grant), now)
            .await
    }

    pub(crate) async fn land_granted(
        &self,
        change_id: &str,
        request: LandRequest,
        grant: Option<PersonGrant>,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let result = self.land_steps(change_id, &request, grant, now).await;
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

    #[cfg(test)]
    pub(crate) fn set_timer(&self, timer: crate::landing_queue::Timer) {
        *self.engine.timer.lock() = Some(timer);
    }

    async fn land_steps(
        &self,
        change_id: &str,
        request: &LandRequest,
        mut grant: Option<PersonGrant>,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let writer = self.writer_or_error()?;
        let Some(_landing) = self.engine.start_landing(change_id) else {
            // A person's `project: land` of a change already landing (waiting for approval)
            // approves what it holds committed; the running landing goes on.
            if let Some(grant) = grant
                && request.mode == LandMode::Land
                && let Some(row) = self.change(change_id)?
            {
                self.record_land_approval(&writer, &row, grant, now).await?;
            }
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
        // A quick edit has only its branch: the landing checks it out.
        if !path.is_dir()
            && (!self
                .git
                .branch_exists(&self.location.repository, &row.branch)
                .await
                || self.check_out_change(change_id).await.is_err())
        {
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
            self.working_copy_subject(&path, &row.name)
        } else {
            subject
        };
        self.git.commit_if_dirty(&path, &subject, &identity).await?;
        if request.mode == LandMode::Land
            && let Some(grant) = grant.take()
        {
            self.record_land_approval(&writer, &row, grant, now).await?;
        }

        'steps: loop {
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

            // Step 3: the amending checks first, then every other check on the final tree.
            self.engine.emit(change_id, LandingEvent::Checking);
            let (amend_stamps, mut run) = match self
                .run_amending_checks(&writer, &mut row, &path, &based_on, &request.meta)
                .await?
            {
                Amended::Done { stamps, run } => (stamps, run),
                Amended::NoVerdict(reason) => return Ok(LandingResult::NoVerdict { reason }),
            };
            let started_seen = run.started_touched();
            let meta = landing_meta(&request.meta, &run.facts.summary);
            run.meta = meta.clone();
            let checked_tip = self.git.rev_parse(&path, "HEAD").await?;
            let mut checks = self.run_builtins(&writer, &run)?;
            if request.mode == LandMode::CheckOnly {
                for phase in [CheckPhase::BeforeQueue, CheckPhase::InQueue] {
                    checks.extend(self.run_registered(&writer, &mut run, phase).await?.0);
                }
                let (_, approval) = self
                    .judge_approval(&writer, &row, &mut run, &checked_tip)
                    .await?;
                checks.push(approval);
                return Ok(LandingResult::Checked {
                    tree: run.tree().to_string(),
                    checks,
                });
            }
            // The cheap checks first: a change they refuse never waits for a slow one.
            if let Some(ended) = verdict(&checks) {
                return Ok(ended);
            }
            let (before, mut seen) = self
                .run_registered(&writer, &mut run, CheckPhase::BeforeQueue)
                .await?;
            if let Some(ended) = verdict(&before) {
                return Ok(ended);
            }
            seen.extend(amend_stamps);

            // Step 4: approval, waited for here, outside the queue and its lock.
            let mut told = false;
            let approval_result = loop {
                let wake = self.engine.signal.listen();
                match self.change(change_id)?.map(|row| row.state) {
                    Some(ChangeState::Requested) => {}
                    Some(ChangeState::Abandoned) | None => return Ok(LandingResult::Abandoned),
                    Some(_) => {
                        if let Some(ended) = self.landing_result(change_id)? {
                            return Ok(ended);
                        }
                    }
                }
                // A commit made while it waited is a new author part: the checks run again.
                if self.git.rev_parse(&path, "HEAD").await? != checked_tip {
                    drop(run);
                    continue 'steps;
                }
                let (approval, result) = self
                    .judge_approval(&writer, &row, &mut run, &checked_tip)
                    .await?;
                if approval.is_approved() {
                    break result;
                }
                if !told {
                    self.engine.emit(
                        change_id,
                        LandingEvent::Waiting {
                            reason: approval.waiting_reason().unwrap_or_default(),
                        },
                    );
                    told = true;
                }
                wake.await.ok();
            };
            // The materialized trees go; the queue step reads the tree it lands.
            drop(run);

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
            // A commit made while it waited for the lock is a new author part, approved by
            // nothing yet: back to step 2, and in at the back of the queue.
            if self.git.rev_parse(&path, "HEAD").await? != checked_tip {
                drop(slot);
                continue;
            }

            // Step 6.
            match self.check_master_as(Some(change_id)).await? {
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
            let rebased = verified_now != based_on;
            if rebased {
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
            if !rebased && tip != checked_tip {
                drop(slot);
                continue;
            }
            let mut run = self
                .prepare_checks(change_id, &based_on, &tip, &meta, &row.author)
                .await?;
            // A before-queue or amending check whose stamp moved runs again: back to step 3,
            // and in at the back of the queue.
            if !self.moved_stamps(&mut run, &seen).await?.is_empty() {
                drop(run);
                drop(slot);
                continue;
            }
            // A task the change touches was started while it waited: its amendments were
            // bounded by the tasks started then, so it goes back to step 3, where they are
            // dropped and redone (or the checks judge the author's part again), and in at the
            // back of the queue.
            if !row.amend_commits.is_empty() && run.started_touched() != started_seen {
                drop(run);
                drop(slot);
                continue;
            }
            // Approval is judged again on the tree that lands: a person's row still counts when
            // the rebase kept the author part, and anything else goes back to wait at step 4.
            let (approval, _) = self.judge_approval(&writer, &row, &mut run, &tip).await?;
            let Some(approval_line) = approved_line(&approval) else {
                drop(run);
                drop(slot);
                continue;
            };
            // The in-queue checks run under the lock, so they get what is left of the queue
            // limit: a check still running then is dropped, the change waits with no verdict,
            // and the lock goes to the next change as the slot is dropped.
            let options = self.land_options();
            let remaining = options.queue_limit.saturating_sub(entered.elapsed());
            let in_queue = {
                let checks = std::pin::pin!(async {
                    let mut results = self.run_builtins(&writer, &run)?;
                    results.extend(
                        self.run_registered(&writer, &mut run, CheckPhase::InQueue)
                            .await?
                            .0,
                    );
                    anyhow::Ok(results)
                });
                let deadline = (self.engine.timer())(remaining);
                match futures::future::select(checks, deadline).await {
                    Either::Left((results, _)) => Some(results?),
                    Either::Right(_) => None,
                }
            };
            let Some(in_queue) = in_queue else {
                return Ok(LandingResult::NoVerdict {
                    reason: format!(
                        "the landing took longer than the queue limit of {} seconds; land again",
                        options.queue_limit.as_secs()
                    ),
                });
            };
            if let Some(ended) = verdict(&in_queue) {
                return Ok(ended);
            }
            let tree = run.tree().to_string();
            drop(run);

            // Step 7.
            let repository = &self.location.repository;
            let new = if options.squash {
                let words: Vec<(String, String)> = before
                    .iter()
                    .chain(&in_queue)
                    .chain(std::iter::once(&approval_result))
                    .map(|result| (result.check.clone(), result.outcome.word().to_string()))
                    .collect();
                let facts = LandingFacts {
                    change_id,
                    checks: &words,
                    tree: &tree,
                };
                let message = commit_message(&meta, &facts);
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
            log::info!("{change_id}: {approval_line}");
            return self
                .finish_landing(
                    &writer,
                    slot,
                    &mut row,
                    &meta,
                    (&based_on, &new),
                    approval_line,
                    now,
                )
                .await;
        }
    }

    /// The subject of the commit of what a working copy holds uncommitted when its author wrote
    /// none: generated from what it does to the verified definitions.
    fn working_copy_subject(&self, path: &Path, name: &str) -> String {
        let master = self
            .writer_or_error()
            .map(|writer| writer.definitions())
            .unwrap_or_default();
        match DefinitionTree::parse_dir(&path.join(DEFINITIONS_DIR)) {
            Ok(tree) => summarize(&master, &tree).generated_subject(),
            Err(_) => format!("Change {name}"),
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

    /// The problems the schema check finds in the tree `treeish`.
    pub(crate) async fn schema_problems_at(&self, treeish: &str) -> Result<Vec<Problem>> {
        let registry = self.check_registry();
        let folders: Vec<String> = registry
            .folders
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let tree = tree_facts(&self.git, &self.location.repository, treeish, &folders).await?;
        Ok(schema_check(&tree, &registry))
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
        (old, new): (&str, &str),
        approval: String,
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
        let moves = staged_file_ops(&staged);
        if let Err(error) = moves
            .iter()
            .try_for_each(|op| check_file_op(&self.location.files, op))
        {
            return Ok(LandingResult::Refused {
                reasons: vec![format!("{error:#}")],
            });
        }
        let in_flight = InFlight {
            change_id: change_id.clone(),
            commit: new.to_string(),
            old: old.to_string(),
        };
        let in_flight_text =
            toml::to_string(&in_flight).context("serializing the landing in flight")?;
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
                approval: Some(approval.clone()),
                generation: tx.generation()?,
            };
            tx.add_landing(
                &change_id,
                &toml::to_string(&record).context("serializing a landing")?,
            )?;
            tx.set_verified_commit(new, definitions.clone())?;
            tx.set_change(&landed)?;
            tx.set_meta(LANDING_IN_FLIGHT_KEY, &in_flight_text)?;
            if !moves.is_empty() {
                // After those an earlier landing could not make, which are tried again.
                let mut kept = tx
                    .meta(LANDING_FILE_MOVES_KEY)?
                    .and_then(|text| toml::from_str::<LandingFiles>(&text).ok())
                    .unwrap_or_default();
                kept.ops.extend(moves.iter().cloned());
                let text =
                    toml::to_string(&kept).context("serializing the file moves of the landing")?;
                tx.set_meta(LANDING_FILE_MOVES_KEY, &text)?;
            }
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
        // The files follow the state, before the ref; a crash before this moves them in the
        // recovery, and those that fail stay recorded for it.
        if let Err(error) = self.run_landing_file_ops(writer) {
            log::error!("{change_id} landed, but {error:#}");
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
        if let Err(error) = self.finish_checkout(writer, &in_flight).await {
            log::error!("finishing the checkout after {change_id} landed: {error:#}");
        }
        drop(slot);
        self.remove_working_copy(row).await;
        self.push_main().await;
        Ok(LandingResult::Landed {
            commit: new.to_string(),
        })
    }

    /// Makes the file ops recorded for the landings (under [`LANDING_FILE_MOVES_KEY`]), and
    /// clears the record of those made. Those that fail stay recorded, for the recovery at the
    /// next open to try again, and the error names them.
    pub(crate) fn run_landing_file_ops(&self, writer: &StateWriter) -> Result<()> {
        let Some(text) = writer.read(|read| read.meta(LANDING_FILE_MOVES_KEY))? else {
            return Ok(());
        };
        let recorded: LandingFiles =
            toml::from_str(&text).context("reading the file moves of a landing")?;
        let failed = run_file_ops(&self.location.files, &recorded.ops);
        let kept = LandingFiles {
            ops: failed.iter().map(|(op, _)| op.clone()).collect(),
        };
        let kept_text = toml::to_string(&kept).context("serializing the file moves kept")?;
        writer.transact(|tx| {
            // Only while the record is the one read: a landing that recorded more meanwhile
            // keeps its own.
            if tx.meta(LANDING_FILE_MOVES_KEY)?.as_deref() == Some(text.as_str()) {
                if kept.ops.is_empty() {
                    tx.delete_meta(LANDING_FILE_MOVES_KEY)?;
                } else {
                    tx.set_meta(LANDING_FILE_MOVES_KEY, &kept_text)?;
                }
            }
            Ok(())
        })?;
        if !failed.is_empty() {
            let reasons: Vec<String> = failed.into_iter().map(|(_, error)| error).collect();
            bail!(
                "its files could not be moved, and the recovery tries again: {}",
                reasons.join("; ")
            );
        }
        Ok(())
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
    /// marker of the landing in flight (and of a split master) is cleared, only while it still
    /// is `in_flight`: a later landing's marker is never cleared by an earlier one's completion.
    /// The marker is cleared even when the checkout could not be updated: `main` moved. When
    /// clearing it fails, the next `check_master` clears it.
    pub(crate) async fn finish_checkout(
        &self,
        writer: &StateWriter,
        in_flight: &InFlight,
    ) -> Result<()> {
        if let Err(error) = self.git.update_checkout(&self.location.repository).await {
            log::error!("updating the checkout of master: {error}");
        } else if let Err(error) = self.protect_master_files() {
            log::error!("making master read-only: {error}");
        }
        writer.transact(|tx| {
            let current = tx
                .meta(LANDING_IN_FLIGHT_KEY)?
                .and_then(|text| toml::from_str::<InFlight>(&text).ok());
            if current.as_ref() == Some(in_flight) {
                tx.delete_meta(LANDING_IN_FLIGHT_KEY)?;
                tx.delete_meta(crate::master::MASTER_SPLIT_KEY)?;
            }
            Ok(())
        })
    }

    /// Removes a landed change's worktree and branch unless the keep setting is on, or the
    /// person typed into it while it landed (what they typed did not land and is kept).
    pub(crate) async fn remove_working_copy(&self, row: &ChangeRow) {
        if self.land_options().keep_landed {
            return;
        }
        let repository = &self.location.repository;
        let path = self.working_copy_path(&row.name);
        if path.is_dir() && !self.git.is_clean(&path).await.unwrap_or(false) {
            log::info!(
                "keeping the landed working copy {}: it has edits that did not land",
                row.name
            );
            return;
        }
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
