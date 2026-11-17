//! The pools a check's parts wait for, as a feature that manages resource pools provides them.
//! This crate defines the seam; that feature implements it and registers it in
//! [`CheckPoolsRegistry`]. With no implementation every part runs at once.
//!
//! A part asks [`CheckPools::acquire`] before it starts and waits in line, first come first
//! served; the [`PoolWait`] it gets tells where it stands while it waits. Extra parts of a
//! parallel check ask [`CheckPools::try_acquire_now`], which grants only a slot that is free now
//! and never one while another request waits in line for that pool, so they never push another
//! run's wait back. A granted [`PoolLease`] holds its slots until it is dropped: the
//! implementation's [`PoolLeaseHandle`] frees them in its own `Drop`.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bongos_workflow::RunId;
use bongos_workflow_model::NodeKey;
use gpui::{App, BackgroundExecutor, Global, Task};

/// The resource pools of a project.
pub trait CheckPools: 'static {
    /// Queues the request, first come first served; the wait resolves with the lease.
    fn acquire(&self, request: PoolRequest) -> PoolWait;
    /// A lease when every slot asked for is free now and no request waits in line for those
    /// pools; never queues.
    fn try_acquire_now(&self, request: PoolRequest) -> Option<PoolLease>;
}

/// What one part asks of the pools.
#[derive(Clone, Debug, PartialEq)]
pub struct PoolRequest {
    pub project_id: String,
    /// The check's `needs` entries, unparsed: a pool name, or a table with a `pool`.
    pub needs: Vec<toml::Value>,
    pub holder: Holder,
}

/// Who asks: the part, and until its process starts the bongos process holding the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub task_id: String,
    pub check: String,
    pub part: u32,
    /// The workflow node the check runs for, if any.
    pub run: Option<(RunId, NodeKey)>,
    pub host_pid: u32,
}

/// Where a waiting request stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitStatus {
    /// The pool it waits for.
    pub pool: String,
    /// Its place in line, 1 first.
    pub position: u32,
    /// When it started waiting.
    pub since: Instant,
    /// Who holds the pool now, named by the pool implementation.
    pub holders: Vec<String>,
    /// Why the request is blocked (all slots taken, an exclusive holder, a guard).
    pub blocked: Option<String>,
}

/// A request waiting in line.
pub struct PoolWait {
    status: Box<dyn Fn() -> WaitStatus>,
    ready: Task<Result<PoolLease, String>>,
}

impl PoolWait {
    /// `status` answers where the request stands now; `ready` resolves with the lease, or with
    /// why the request can never be granted. Dropping the wait leaves the line.
    pub fn new(
        status: Box<dyn Fn() -> WaitStatus>,
        ready: Task<Result<PoolLease, String>>,
    ) -> Self {
        Self { status, ready }
    }

    pub fn status(&self) -> WaitStatus {
        (self.status)()
    }

    /// Awaited in place.
    pub fn ready(&mut self) -> &mut Task<Result<PoolLease, String>> {
        &mut self.ready
    }
}

impl std::fmt::Debug for PoolWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolWait")
            .field("status", &self.status())
            .finish()
    }
}

/// The implementation's side of a lease. It frees the slots in its own `Drop`, once.
pub trait PoolLeaseHandle: 'static {
    /// The part's process now holds the slots: `pid` started at `start_time` (as the process
    /// table gives it), so a hold can be freed when that process dies.
    fn bind_process(&self, pid: u32, start_time: u64);
}

/// Granted slots, held until dropped. Not `Clone`: one lease frees its slots once.
pub struct PoolLease {
    handle: Box<dyn PoolLeaseHandle>,
}

impl PoolLease {
    pub fn new(handle: Box<dyn PoolLeaseHandle>) -> Self {
        Self { handle }
    }

    pub fn bind_process(&self, pid: u32, start_time: u64) {
        self.handle.bind_process(pid, start_time);
    }
}

impl std::fmt::Debug for PoolLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolLease").finish_non_exhaustive()
    }
}

/// The registered pool implementation; none until a pool feature sets one.
#[derive(Default)]
pub struct CheckPoolsRegistry {
    pools: Option<Rc<dyn CheckPools>>,
}

impl Global for CheckPoolsRegistry {}

impl CheckPoolsRegistry {
    pub fn set(pools: Rc<dyn CheckPools>, cx: &mut App) {
        cx.default_global::<Self>().pools = Some(pools);
    }

    /// The registered implementation; `None` runs every part at once.
    pub fn get(cx: &App) -> Option<Rc<dyn CheckPools>> {
        cx.try_global::<Self>()
            .and_then(|registry| registry.pools.clone())
    }
}

/// The time a check run reads: when a wait started, and sleeping for the wait limit and
/// between no-verdict reruns.
pub trait CheckClock: 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>>;
}

/// A [`CheckClock`] over an executor's clock and timers.
pub struct ExecutorClock(pub BackgroundExecutor);

impl CheckClock for ExecutorClock {
    fn now(&self) -> Instant {
        self.0.now()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(self.0.timer(duration))
    }
}
