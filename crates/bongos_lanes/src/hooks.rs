//! Job hooks: points in a lane job's start where other features run their own steps. With none
//! registered nothing runs.
//!
//! [`JobHook::before_job`] runs after the job's trees are prepared and before its run is started;
//! it may hold the lane (shown with [`JobHookCx::set_wait`]) or refuse the job. Every hook whose
//! `before_job` was called and did not refuse gets exactly one [`JobHook::after_job`]: at the end
//! of the job, on a refused start, or when the job is dropped before its run started (its pending
//! `before_job` future is then dropped). The lane is not evaluated again until `after_job`
//! resolves.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use bongos_lanes_model::{JobRecord, LaneId, LaneState, WaitReason};
use bongos_project_repo::StateWriter;
use bongos_workflow_model::record::Lifecycle;
use gpui::{App, Global, Task};

use crate::store::{put_lane, read_state_in};

/// The hook point named in a wait reason when [`JobHook::after_tree_created`] fails.
pub const AFTER_TREE_CREATED: &str = "after_tree_created";
/// The hook point a [`JobHook::before_job`] names in the wait reasons it gives.
pub const BEFORE_JOB: &str = "before_job";

/// What [`JobHook::before_job`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeJob {
    Go,
    /// The job is not started: the lane's claim is cleared, the owner record stays, and the lane
    /// waits with `reason` until the next pass.
    Refuse {
        reason: WaitReason,
    },
}

/// How a job ended, as [`JobHook::after_job`] hears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// The job's run left its lane in this state.
    Lifecycle(Lifecycle),
    /// The run engine refused to start the job's run.
    Refused { reason: String },
    /// The job was dropped before its run started: a stop, a lost claim or a later hook's refusal.
    StoppedBeforeStart,
}

/// What a [`JobHook::before_job`] can do to its lane while it is pending. Cheap to clone.
#[derive(Clone)]
pub struct JobHookCx {
    lane: LaneId,
    run_id: String,
    writer: Arc<StateWriter>,
    pending: Rc<Cell<bool>>,
    /// The hooks whose `before_job` was called and did not refuse.
    owed: Rc<RefCell<Vec<Arc<dyn JobHook>>>>,
}

impl JobHookCx {
    pub(crate) fn new(lane: LaneId, run_id: String, writer: Arc<StateWriter>) -> Self {
        Self {
            lane,
            run_id,
            writer,
            pending: Rc::new(Cell::new(true)),
            owed: Rc::default(),
        }
    }

    /// Shows the lane `Waiting` with `reason` while `before_job` is pending. Does nothing once it
    /// resolved or was dropped, or when the lane no longer holds the job.
    pub fn set_wait(&self, reason: WaitReason) {
        if !self.pending.get() {
            return;
        }
        let result = self.writer.transact(|tx| {
            let state = read_state_in(tx)?;
            let Some(lane) = state.lanes.get(&self.lane) else {
                return Ok(());
            };
            if lane.job.as_ref().map(|job| job.run_id.as_str()) != Some(self.run_id.as_str()) {
                return Ok(());
            }
            let mut updated = lane.clone();
            updated.state = LaneState::Waiting;
            updated.reason = Some(reason);
            if updated != *lane {
                put_lane(tx, &updated)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            log::error!("lanes: showing the wait of {}: {error:#}", self.lane);
        }
    }

    /// Ends the time `set_wait` may write.
    pub(crate) fn finish(&self) {
        self.pending.set(false);
    }

    /// Whether a hook is owed an `after_job`.
    pub(crate) fn owes(&self) -> bool {
        !self.owed.borrow().is_empty()
    }

    /// The hooks that are owed an `after_job`, which no longer are once taken.
    pub(crate) fn take_owed(&self) -> Vec<Arc<dyn JobHook>> {
        std::mem::take(&mut *self.owed.borrow_mut())
    }
}

/// One feature's hooks. Each default does nothing.
///
/// A `before_job` future must be safe to drop: dropping it releases whatever it was waiting for.
/// `after_job` releases anything `before_job` took, including a hold taken before the future was
/// dropped.
pub trait JobHook {
    /// Runs once after a lane tree (a tip or a second tree) of `repo` is made at `path` for the
    /// first time, before the job uses it. The lane waits for the task; an error leaves the lane
    /// waiting with its message.
    fn after_tree_created(
        &self,
        _lane: &LaneId,
        _repo: &str,
        _path: &Path,
        _cx: &mut App,
    ) -> Task<Result<(), String>> {
        Task::ready(Ok(()))
    }

    /// Runs after the job's trees are prepared and before its run is started. The job starts
    /// once every hook said [`BeforeJob::Go`].
    fn before_job(
        &self,
        _lane: &LaneId,
        _job: &JobRecord,
        _hcx: JobHookCx,
        _cx: &mut App,
    ) -> Task<BeforeJob> {
        Task::ready(BeforeJob::Go)
    }

    /// Runs once for every `before_job` that did not refuse, when the job ends.
    fn after_job(
        &self,
        _lane: &LaneId,
        _job: &JobRecord,
        _end: &JobEnd,
        _cx: &mut App,
    ) -> Task<()> {
        Task::ready(())
    }
}

/// The registered job hooks, called in the order they were registered.
#[derive(Default)]
pub struct JobHooks {
    hooks: Vec<Arc<dyn JobHook>>,
}

impl Global for JobHooks {}

impl JobHooks {
    pub fn register(hook: Arc<dyn JobHook>, cx: &mut App) {
        cx.default_global::<Self>().hooks.push(hook);
    }

    /// Every registered hook.
    pub fn hooks(cx: &App) -> Vec<Arc<dyn JobHook>> {
        cx.try_global::<Self>()
            .map(|hooks| hooks.hooks.clone())
            .unwrap_or_default()
    }

    /// Calls every hook's `before_job` in order, each after the one before said `Go`. The first
    /// refusal is the answer and the later hooks are not asked. Each hook asked is recorded in
    /// `hcx` as owed an `after_job` before its future is awaited, and taken off again when it
    /// refuses. Dropping the task drops the pending hook's future.
    pub fn before_job(
        lane: &LaneId,
        job: &JobRecord,
        hcx: JobHookCx,
        cx: &mut App,
    ) -> Task<BeforeJob> {
        let hooks = Self::hooks(cx);
        if hooks.is_empty() {
            return Task::ready(BeforeJob::Go);
        }
        let lane = lane.clone();
        let job = job.clone();
        cx.spawn(async move |cx| {
            for hook in hooks {
                hcx.owed.borrow_mut().push(hook.clone());
                let decision = cx.update(|cx| hook.before_job(&lane, &job, hcx.clone(), cx));
                if let BeforeJob::Refuse { reason } = decision.await {
                    hcx.owed.borrow_mut().pop();
                    return BeforeJob::Refuse { reason };
                }
            }
            BeforeJob::Go
        })
    }

    /// Calls every registered hook's `after_job` in order, each after the one before resolved.
    pub fn after_job(lane: &LaneId, job: &JobRecord, end: &JobEnd, cx: &mut App) -> Task<()> {
        after_job_of(Self::hooks(cx), lane, job, end, cx)
    }
}

/// Calls `after_job` of `hooks` in order, each after the one before resolved.
pub(crate) fn after_job_of(
    hooks: Vec<Arc<dyn JobHook>>,
    lane: &LaneId,
    job: &JobRecord,
    end: &JobEnd,
    cx: &mut App,
) -> Task<()> {
    if hooks.is_empty() {
        return Task::ready(());
    }
    let lane = lane.clone();
    let job = job.clone();
    let end = end.clone();
    cx.spawn(async move |cx| {
        for hook in hooks {
            let done = cx.update(|cx| hook.after_job(&lane, &job, &end, cx));
            done.await;
        }
    })
}
