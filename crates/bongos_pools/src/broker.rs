//! The broker of one project's pools in its writer process: a plain `Rc` object over the project
//! store and the machine store, the guard readers, the in-memory waiters (each a `oneshot`
//! sender) and the foreground and background executors it was built with, so callers with no
//! `App` (47's check runs, 20's runtime) can ask it.
//!
//! - [`PoolBroker::acquire`] answers at once with a [`Waiting`]; its [`Waiting::ready`] resolves
//!   to a [`Lease`] or a [`Refused`]. [`PoolBroker::try_now`] grants only what is free now, never
//!   while any waiter is queued (or about to be) on a named pool, and never waits for the machine
//!   lock: a busy lock answers "not now".
//! - Every store change runs on the background executor, one at a time, in the order asked
//!   (enqueue, withdraw, release, bind, poll, drain). Each transaction prunes, applies, grants this
//!   process's waiters and bumps the generation. A transaction touches every store this broker
//!   has a waiter in, so a release in one store wakes a waiter in the other, and a request naming
//!   a machine pool and a project pool (both halves marked `mixed`) is only ever judged by
//!   [`transact_mixed`](crate::transact_mixed).
//! - Wake-ups: a release, a new waiter, the poll every `pools.poll_interval_ms` while anything
//!   waits or a hold is bound to a process, and a guard reading (taken in the poll when due).
//! - Dropping a [`Waiting`] removes its rows in every store in one transaction; dropping a
//!   [`Lease`] releases its holds (a mixed lease in one mixed transaction).
//! - The writer lifecycle: [`PoolBroker::drain`] refuses new requests, stops the poll and removes
//!   this process's waiters and its holds not bound to a live process, keeping a list of what it
//!   gave up; [`PoolBroker::undrain`] accepts requests again and [`PoolBroker::started`] asks the
//!   listed requests again under new tickets.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bongos_pools_model::{
    CycleLink, Grant, GuardCheck, GuardReadings, Hold, Holder, HolderLabel, HostRef, Need,
    NeedMode, OwnerKey, PoolKind, PoolScope, PoolState, PoolTable, ProcRef,
    Refused as ModelRefused, Request as ModelRequest, RequestId, Timestamp,
    WaitStatus as ModelWaitStatus, admit_now, judge, wait_status,
};
use bongos_workflow::process::ProcessRecord;
use futures::channel::oneshot;
use gpui::{BackgroundExecutor, ForegroundExecutor, Task};

use crate::declare::{DeclareOutcome, KindClash, PoolConfig, ProjectRef};
use crate::guards::{GuardInputs, GuardReaders, due_guards, read_guards, read_guards_now};
use crate::machine_store::{LockBusy, LockWait, MachineStore};
use crate::mixed::{MachineHalfUnwritten, transact_mixed};
use crate::project_store::ProjectStore;
use crate::store::{PoolStore as _, StoreContext};
use crate::{PoolsSettings, machine_store_dir};

pub use bongos_pools_model::Blocked;

/// Which store a hold or waiter lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StoreKind {
    Project,
    Machine,
}

/// One hold of a lease: its store, its pool and its ticket, which a release names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HoldId {
    pub store: StoreKind,
    pub pool: String,
    pub ticket: u64,
}

/// What a caller asks for. The broker sets the holder's project and host.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub needs: Vec<Need>,
    pub holder: Holder,
}

/// Why a request is never granted.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// The pool is not declared (or the request named it in a way its kind does not allow).
    Undeclared { pool: String },
    /// The need does not fit the pool's kind (a mode on a non-rw pool, a count above the size).
    BadNeed { pool: String, reason: String },
    /// An enclosing holder holds the pool in a way the request cannot fit beside.
    OwnHold {
        pool: String,
        holder: HolderLabel,
        held: String,
        asked: String,
    },
    /// Queuing would close a cycle of waits.
    Deadlock { cycle: Vec<CycleLink> },
    /// The machine pool stands with another kind, set by another project.
    KindClash {
        pool: String,
        set_by: Option<ProjectRef>,
        text: String,
    },
    /// The writer is moving to the engine host.
    Draining,
    /// The broker is gone.
    Closed,
    /// A store could not be written.
    Store { reason: String },
}

impl Refused {
    /// The pool the refusal names, if one.
    pub fn pool(&self) -> Option<&str> {
        match self {
            Refused::Undeclared { pool }
            | Refused::BadNeed { pool, .. }
            | Refused::OwnHold { pool, .. }
            | Refused::KindClash { pool, .. } => Some(pool),
            Refused::Deadlock { cycle } => cycle.iter().find_map(|link| link.waits_for.as_deref()),
            Refused::Draining | Refused::Closed | Refused::Store { .. } => None,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Undeclared { pool } => {
                write!(f, "pool `{pool}` is not declared in pools/pools.toml")
            }
            Refused::BadNeed { pool, reason } => write!(f, "pool `{pool}`: {reason}"),
            Refused::OwnHold {
                pool,
                holder,
                held,
                asked,
            } => write!(
                f,
                "{} holds `{pool}` {held}; it cannot also wait for it {asked}",
                holder.0
            ),
            Refused::Deadlock { cycle } => {
                let links: Vec<String> = cycle
                    .iter()
                    .map(|link| match &link.waits_for {
                        Some(pool) => format!("{} waits for {pool}", link.label.0),
                        None => link.label.0.clone(),
                    })
                    .collect();
                write!(f, "would deadlock: {}", links.join("; "))
            }
            Refused::KindClash { text, .. } => f.write_str(text),
            Refused::Draining => f.write_str("the engine is moving to the host"),
            Refused::Closed => f.write_str("the pool broker is closed"),
            Refused::Store { reason } => write!(f, "the pool store failed: {reason}"),
        }
    }
}

/// Why a hold went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseWhy {
    /// Its lease was dropped.
    Released,
    /// Its holder (or the host that asked) is gone.
    Dead,
    /// Released by hand.
    ByHand { by: String },
}

/// What the broker tells its listeners.
#[derive(Debug, Clone, PartialEq)]
pub enum PoolsEvent {
    Waiting {
        pool: String,
        owner: OwnerKey,
        position: u32,
    },
    Granted {
        pool: String,
        owner: OwnerKey,
        waited: Duration,
    },
    Released {
        pool: String,
        owner: OwnerKey,
        why: ReleaseWhy,
    },
    HoldTooLong {
        pool: String,
        owner: OwnerKey,
        held: Duration,
    },
    Warning {
        text: String,
    },
}

/// Where a waiting request stands, in the pool that holds it up.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitStatus51 {
    pub pool: String,
    /// Its place in line, 1 first; 0 while its rows are not written yet.
    pub position: u32,
    pub since: Timestamp,
    /// Who holds the pool now.
    pub holders: Vec<HolderLabel>,
    /// Why it is not granted; `None` when its host grants it at its next transaction.
    pub blocked: Option<Blocked>,
    /// Beside a guard reading: a script's first line, or why the guard gave no reading.
    pub guard_note: Option<String>,
}

impl WaitStatus51 {
    /// The reason as one line ("guard on gpu failed (9.0 GB, needs 12 GB)").
    pub fn blocked_text(&self) -> Option<String> {
        let blocked = self.blocked.as_ref()?;
        let mut text = describe_blocked(blocked);
        if let Some(note) = &self.guard_note {
            text.push_str(&format!(": {note}"));
        }
        Some(text)
    }
}

fn describe_blocked(blocked: &Blocked) -> String {
    match blocked {
        Blocked::Guard {
            pool,
            reading,
            threshold,
        } => {
            let (value, needs) = match (reading, threshold) {
                (
                    bongos_pools_model::GuardValue::FreeMemoryGb(value),
                    GuardCheck::FreeMemoryGb(gb),
                )
                | (
                    bongos_pools_model::GuardValue::FreeDiskGb(value),
                    GuardCheck::FreeDiskGb { gb, .. },
                ) => (format!("{value:.1} GB"), format!("{gb} GB")),
                (bongos_pools_model::GuardValue::ScriptExit(code), _) => {
                    (format!("exit {code}"), "exit 0".to_string())
                }
                (other, check) => (format!("{other:?}"), format!("{check:?}")),
            };
            format!("guard on {pool} failed ({value}, needs {needs})")
        }
        Blocked::GuardUnread { pool } => format!("guard on {pool} not read yet"),
        Blocked::GuardSettling { pool, .. } => format!("guard on {pool} settling"),
        Blocked::Busy { pool } => format!("{pool} is full"),
        Blocked::Behind { pool, ticket } => format!("behind ticket {ticket} of {pool}"),
        Blocked::Mixed { pool, .. } => format!("behind another project's request in {pool}"),
        Blocked::OtherPool { other, .. } => format!("waiting for {other}"),
        other => format!("{other:?}"),
    }
}

/// A request given up by a drain, asked again by [`PoolBroker::started`].
#[derive(Debug, Clone, PartialEq)]
pub struct GivenUp {
    pub holder: Holder,
    pub needs: Vec<Need>,
    /// Whether it held its pools (true) or waited for them.
    pub held: bool,
}

/// How often the broker looks and how long a guard script may take.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerTiming {
    pub poll_interval: Duration,
    pub guard_interval: Duration,
    pub guard_script_timeout: Duration,
    /// A hold older than this raises [`PoolsEvent::HoldTooLong`] once.
    pub max_hold: Option<Duration>,
}

impl BrokerTiming {
    pub fn from_settings(settings: &PoolsSettings) -> BrokerTiming {
        BrokerTiming {
            poll_interval: settings.poll_interval,
            guard_interval: settings.guard_interval,
            guard_script_timeout: settings.guard_script_timeout,
            max_hold: settings.max_hold,
        }
    }
}

/// Two handles on the machine store of one project's broker: one that waits for the lock
/// (background transactions) and one that tries it once (the synchronous [`PoolBroker::try_now`]).
/// Their transactions convert only `project`'s waiters, since every broker of this process shares
/// its host.
#[derive(Clone)]
pub struct MachineStores {
    pub patient: Arc<MachineStore>,
    pub once: Arc<MachineStore>,
}

impl MachineStores {
    pub fn open(
        dir: PathBuf,
        context: StoreContext,
        wait: LockWait,
        project: &str,
    ) -> Result<MachineStores> {
        let context = context.with_project(project);
        Ok(MachineStores {
            patient: Arc::new(MachineStore::open(dir.clone(), context.clone(), wait)?),
            once: Arc::new(MachineStore::open(dir, context, LockWait::once())?),
        })
    }

    /// The store of `pools.machine_store_dir`, waiting as `pools.lock_timeout_seconds` says.
    pub fn from_settings(
        settings: &PoolsSettings,
        context: StoreContext,
        project: &str,
    ) -> Result<MachineStores> {
        MachineStores::open(
            machine_store_dir(settings),
            context,
            LockWait::from_settings(settings),
            project,
        )
    }
}

/// What a broker is built from.
pub struct BrokerParts {
    pub project: ProjectRef,
    pub project_store: Arc<ProjectStore>,
    pub machine: Option<MachineStores>,
    pub readers: Arc<dyn GuardReaders>,
    /// The pinned copy of the definitions guard scripts run in.
    pub definitions: Option<PathBuf>,
    /// The folder a disk guard reads when it names no path, and relative paths start from.
    pub disk_root: PathBuf,
    pub timing: BrokerTiming,
    pub foreground: ForegroundExecutor,
    pub background: BackgroundExecutor,
    /// Kept alive as long as the broker (the pinned definitions, say).
    pub keep: Option<Box<dyn std::any::Any>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Stores {
    project: bool,
    machine: bool,
}

impl Stores {
    fn of(store: StoreKind) -> Stores {
        match store {
            StoreKind::Project => Stores {
                project: true,
                machine: false,
            },
            StoreKind::Machine => Stores {
                project: false,
                machine: true,
            },
        }
    }

    fn of_holds(holds: &[HoldId]) -> Stores {
        holds.iter().fold(Stores::default(), |stores, hold| {
            stores.or(Stores::of(hold.store))
        })
    }

    fn or(self, other: Stores) -> Stores {
        Stores {
            project: self.project || other.project,
            machine: self.machine || other.machine,
        }
    }

    fn any(self) -> bool {
        self.project || self.machine
    }

    fn covers(self, other: Stores) -> bool {
        (self.project || !other.project) && (self.machine || !other.machine)
    }
}

type Reply = oneshot::Sender<Result<Lease, Refused>>;

/// Who gets a request's grant.
enum Answer {
    /// A [`Waiting`] that has not resolved.
    Waiter(Reply),
    /// A lease whose holds a drain removed, asked again.
    Lease(Weak<LeaseInner>),
}

impl Answer {
    fn alive(&self) -> bool {
        match self {
            Answer::Waiter(reply) => !reply.is_canceled(),
            Answer::Lease(lease) => lease.strong_count() > 0,
        }
    }
}

struct Pending {
    request: ModelRequest,
    answer: Answer,
    /// The stores its needs name: a withdrawal clears all of them.
    needs_stores: Stores,
    /// The stores its rows were written to.
    stores: Stores,
    queued: bool,
    granted_in: Stores,
    partial: Vec<HoldId>,
    satisfied: Vec<String>,
    since: Timestamp,
}

enum Op {
    Enqueue(RequestId),
    Withdraw {
        request: RequestId,
        stores: Stores,
    },
    Release {
        request: RequestId,
        holds: Vec<HoldId>,
    },
    Bind {
        holds: Vec<HoldId>,
        process: ProcRef,
    },
    Poll,
    Drain {
        /// Whether holds not bound to a live process go too (a drain) or only waiters (closing).
        holds: bool,
        done: Option<oneshot::Sender<()>>,
    },
}

#[derive(Default)]
struct State {
    scopes: BTreeMap<String, PoolScope>,
    clashes: BTreeMap<String, KindClash>,
    pending: BTreeMap<RequestId, Pending>,
    leases: BTreeMap<RequestId, Weak<LeaseInner>>,
    ops: VecDeque<Op>,
    driving: bool,
    retry: Vec<Op>,
    draining: bool,
    closed: bool,
    given_up: Vec<(ModelRequest, Answer, bool)>,
    timer_on: bool,
    timer_generation: u64,
    project_table: PoolTable,
    machine_table: PoolTable,
    guard_notes: BTreeMap<String, String>,
    flagged: BTreeSet<HoldId>,
}

struct Shared {
    parts: BrokerParts,
    context: StoreContext,
    guards: GuardInputs,
    state: RefCell<State>,
    listeners: RefCell<Vec<Rc<dyn Fn(&PoolsEvent)>>>,
}

/// Request ids are unique in this process, so no two brokers (or a broker and its successor)
/// ever share one.
static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_request() -> RequestId {
    RequestId(NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
}

/// The broker of one project. Dropping it fails every pending wait and removes this process's
/// waiter rows; live leases still release their holds when dropped.
pub struct PoolBroker {
    shared: Rc<Shared>,
}

impl PoolBroker {
    /// A broker over `parts`. Its host, probe and clock are those of the project store. Declare
    /// the pools ([`PoolBroker::declare`]) before asking for them.
    pub fn new(parts: BrokerParts) -> PoolBroker {
        let context = parts.project_store.context().clone();
        let guards = GuardInputs {
            readers: parts.readers.clone(),
            definitions: parts.definitions.clone(),
            disk_root: parts.disk_root.clone(),
            interval: parts.timing.guard_interval,
            script_timeout: parts.timing.guard_script_timeout,
        };
        let shared = Rc::new(Shared {
            parts,
            context,
            guards,
            state: RefCell::new(State::default()),
            listeners: RefCell::new(Vec::new()),
        });
        // Prunes the machine store before the first grant (the project store pruned at open).
        shared.push(Op::Poll);
        PoolBroker { shared }
    }

    pub fn project(&self) -> &ProjectRef {
        &self.shared.parts.project
    }

    pub fn host(&self) -> HostRef {
        self.shared.context.host
    }

    /// Calls `listener` with every event from now on.
    pub fn subscribe(&self, listener: impl Fn(&PoolsEvent) + 'static) {
        self.shared.listeners.borrow_mut().push(Rc::new(listener));
    }

    /// Registers the project's pools in its store and its machine pools in the machine store. A
    /// machine pool another project set with another kind is refused: requests naming it get
    /// [`Refused::KindClash`].
    pub fn declare(
        &self,
        project: Vec<PoolConfig>,
        machine: Vec<PoolConfig>,
    ) -> Task<Result<DeclareOutcome>> {
        let shared = self.shared.clone();
        let project_store = shared.parts.project_store.clone();
        let machine_store = shared.parts.machine.as_ref().map(|m| m.patient.clone());
        let project_ref = shared.parts.project.clone();
        let (project_configs, machine_configs) = (project.clone(), machine.clone());
        let work = shared.parts.background.spawn(async move {
            let mut outcome = project_store.declare(&project_configs)?.value;
            if !machine_configs.is_empty() {
                let Some(machine_store) = machine_store else {
                    anyhow::bail!("machine pools are declared but no machine store is open");
                };
                let machine_outcome = machine_store.declare(&project_ref, &machine_configs)?.value;
                outcome.warnings.extend(machine_outcome.warnings);
                outcome.refused.extend(machine_outcome.refused);
            }
            Ok(outcome)
        });
        self.shared.parts.foreground.spawn(async move {
            let outcome = work.await?;
            {
                let mut state = shared.state.borrow_mut();
                for config in &project {
                    state.scopes.insert(config.name.clone(), PoolScope::Project);
                }
                for config in &machine {
                    state.scopes.insert(config.name.clone(), PoolScope::Machine);
                }
                for clash in &outcome.refused {
                    state.scopes.remove(&clash.pool);
                    state.clashes.insert(clash.pool.clone(), clash.clone());
                }
            }
            for warning in &outcome.warnings {
                log::warn!("pools: {warning}");
                shared.emit(&PoolsEvent::Warning {
                    text: warning.to_string(),
                });
            }
            shared.push(Op::Poll);
            Ok(outcome)
        })
    }

    /// Queues the request; the wait resolves to the lease or to why it is never granted.
    pub fn acquire(&self, request: Request) -> Waiting {
        self.shared.acquire(request)
    }

    /// A lease when every pool named is free now and no request waits (or is about to) for any
    /// of them; never queues and never waits for the machine lock.
    pub fn try_now(&self, request: Request) -> Option<Lease> {
        self.shared.try_now(request)
    }

    /// The writer moves away: refuse new requests, stop the poll, remove this process's waiters
    /// and its holds not bound to a live process, and keep the list of what was given up.
    pub fn drain(&self) -> Task<()> {
        let (done, finished) = oneshot::channel();
        {
            let mut state = self.shared.state.borrow_mut();
            state.draining = true;
            state.timer_on = false;
            state.timer_generation += 1;
        }
        self.shared.push(Op::Drain {
            holds: true,
            done: Some(done),
        });
        self.shared.parts.foreground.spawn(async move {
            finished.await.ok();
        })
    }

    /// The writer stays here after a drain: requests are accepted again.
    pub fn undrain(&self) {
        self.shared.state.borrow_mut().draining = false;
    }

    /// This process is the writer (again): drops the given-up requests nobody waits for any more,
    /// asks the others again under new tickets, and starts the poll.
    pub fn started(&self) {
        let given_up = {
            let mut state = self.shared.state.borrow_mut();
            state.draining = false;
            let mut given_up = std::mem::take(&mut state.given_up);
            // What held its pools before asks first, so it does not queue behind what only
            // waited; each kind keeps its order.
            given_up.sort_by_key(|(_, _, held)| !*held);
            given_up
        };
        for (request, answer, _) in given_up {
            if !answer.alive() || self.shared.state.borrow().closed {
                continue;
            }
            let needs_stores = self.shared.stores_of(&request.needs);
            let id = request.id;
            self.shared.state.borrow_mut().pending.insert(
                id,
                Pending {
                    since: self.shared.context.now(),
                    request,
                    answer,
                    needs_stores,
                    stores: Stores::default(),
                    queued: false,
                    granted_in: Stores::default(),
                    partial: Vec::new(),
                    satisfied: Vec::new(),
                },
            );
            self.shared.push(Op::Enqueue(id));
        }
        self.shared.ensure_timer();
    }

    /// What the last drain gave up and [`PoolBroker::started`] has not asked again yet.
    pub fn given_up(&self) -> Vec<GivenUp> {
        self.shared
            .state
            .borrow()
            .given_up
            .iter()
            .map(|(request, _, held)| GivenUp {
                holder: request.holder.clone(),
                needs: request.needs.clone(),
                held: *held,
            })
            .collect()
    }

    /// Whether the broker refuses new requests (it is draining or closed).
    pub fn is_draining(&self) -> bool {
        let state = self.shared.state.borrow();
        state.draining || state.closed
    }

    /// The tables as the last transaction left them: the project's and the machine's.
    pub fn tables(&self) -> (PoolTable, PoolTable) {
        let state = self.shared.state.borrow();
        (state.project_table.clone(), state.machine_table.clone())
    }

    /// The process clock this broker reads.
    pub fn now(&self) -> Timestamp {
        self.shared.context.now()
    }

    /// Its host, probe and clock.
    pub fn context(&self) -> StoreContext {
        self.shared.context.clone()
    }

    pub fn foreground(&self) -> &ForegroundExecutor {
        &self.shared.parts.foreground
    }

    pub fn background(&self) -> &BackgroundExecutor {
        &self.shared.parts.background
    }
}

impl Drop for PoolBroker {
    fn drop(&mut self) {
        let pending = {
            let mut state = self.shared.state.borrow_mut();
            state.closed = true;
            state.timer_on = false;
            state.timer_generation += 1;
            state.given_up.clear();
            std::mem::take(&mut state.pending)
        };
        // Each waiter's `ready` sees its sender gone and answers `Closed`.
        drop(pending);
        self.shared.push(Op::Drain {
            holds: false,
            done: None,
        });
    }
}

/// A request in line. Dropping it leaves the line in every store.
pub struct Waiting {
    shared: Rc<Shared>,
    request: RequestId,
    first_pool: Option<String>,
    since: Timestamp,
    receiver: Option<oneshot::Receiver<Result<Lease, Refused>>>,
}

impl Waiting {
    /// Resolves to the lease, or to why the request is never granted.
    pub async fn ready(mut self) -> Result<Lease, Refused> {
        let Some(receiver) = self.receiver.take() else {
            return Err(Refused::Closed);
        };
        match receiver.await {
            Ok(answer) => answer,
            Err(_) => Err(if self.shared.state.borrow().closed {
                Refused::Closed
            } else {
                Refused::Draining
            }),
        }
    }

    /// Where it stands now.
    pub fn status(&self) -> WaitStatus51 {
        self.status_source().status()
    }

    /// A handle answering [`Waiting::status`] that outlives the wait's future.
    pub fn status_source(&self) -> StatusSource {
        StatusSource {
            shared: Rc::downgrade(&self.shared),
            request: self.request,
            first_pool: self.first_pool.clone().unwrap_or_default(),
            since: self.since,
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.shared.forget(self.request);
    }
}

/// Answers where one request stands.
#[derive(Clone)]
pub struct StatusSource {
    shared: Weak<Shared>,
    request: RequestId,
    first_pool: String,
    since: Timestamp,
}

impl StatusSource {
    pub fn status(&self) -> WaitStatus51 {
        let fallback = WaitStatus51 {
            pool: self.first_pool.clone(),
            position: 0,
            since: self.since,
            holders: Vec::new(),
            blocked: None,
            guard_note: None,
        };
        match self.shared.upgrade() {
            Some(shared) => shared.status(self.request).unwrap_or(fallback),
            None => fallback,
        }
    }
}

/// Granted pools, held until dropped.
pub struct Lease {
    inner: Rc<LeaseInner>,
}

struct LeaseInner {
    shared: Rc<Shared>,
    request: ModelRequest,
    holds: RefCell<Vec<HoldId>>,
    satisfied: Vec<String>,
    process: Cell<Option<ProcRef>>,
}

impl Lease {
    /// The holder's own process now holds the pools: the hold lives as long as it does.
    pub fn bind_process(&self, record: ProcessRecord) {
        let process = ProcRef {
            pid: record.pid,
            start: record.start_time,
        };
        self.inner.process.set(Some(process));
        let holds = self.inner.holds.borrow().clone();
        if !holds.is_empty() {
            self.inner.shared.push(Op::Bind { holds, process });
            self.inner.shared.ensure_timer();
        }
    }

    /// The holds of this lease (none in a pool an enclosing holder covers, or in a guard pool).
    pub fn hold_ids(&self) -> Vec<HoldId> {
        self.inner.holds.borrow().clone()
    }

    /// The pools an enclosing holder's hold covers.
    pub fn satisfied(&self) -> &[String] {
        &self.inner.satisfied
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("holds", &self.inner.holds.borrow())
            .finish_non_exhaustive()
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        let request = self.request.id;
        let asked_again = {
            let mut state = self.shared.state.borrow_mut();
            state.leases.remove(&request);
            state.given_up.retain(|(given, _, _)| given.id != request);
            state.pending.remove(&request)
        };
        if let Some(pending) = asked_again {
            self.shared.push(Op::Withdraw {
                request,
                stores: pending.needs_stores,
            });
        }
        let holds = std::mem::take(&mut *self.holds.borrow_mut());
        if !holds.is_empty() {
            self.shared.push(Op::Release { request, holds });
        }
    }
}

/// What one transaction (or two, after a failed `machine.toml` write) did.
struct Done<T> {
    /// `None` when the change landed in the project store only (a failed `machine.toml` write).
    value: Option<T>,
    machine_grants: Vec<Grant>,
    project_grants: Vec<Grant>,
    pruned: Vec<(StoreKind, String, Hold)>,
    project_table: Option<PoolTable>,
    machine_table: Option<PoolTable>,
    notes: BTreeMap<String, String>,
}

enum TxError {
    /// The machine lock stayed busy: try again at the next poll.
    Busy(String),
    /// The project half committed, `machine.toml` was not written, even on a retry.
    HalfUnwritten {
        project_grants: Vec<Grant>,
        reason: String,
    },
    Failed(String),
}

impl TxError {
    fn reason(&self) -> String {
        match self {
            TxError::Busy(reason)
            | TxError::Failed(reason)
            | TxError::HalfUnwritten { reason, .. } => reason.clone(),
        }
    }
}

/// What the enqueue transaction decided.
enum Enqueued {
    Queued {
        stores: Stores,
        satisfied: Vec<String>,
    },
    Satisfied(Vec<String>),
    Refused(Refused),
}

type Change<T> =
    Box<dyn FnOnce(Option<&mut PoolTable>, Option<&mut PoolTable>) -> Result<T> + Send>;

fn classify(error: anyhow::Error) -> TxError {
    if error.downcast_ref::<LockBusy>().is_some() {
        TxError::Busy(format!("{error:#}"))
    } else {
        TxError::Failed(format!("{error:#}"))
    }
}

fn pruned_of(
    store: StoreKind,
    pruned: bongos_pools_model::Pruned,
) -> Vec<(StoreKind, String, Hold)> {
    pruned
        .holds
        .into_iter()
        .map(|(pool, hold)| (store, pool, hold))
        .collect()
}

/// Everything one background transaction needs.
struct TxInput {
    stores: Stores,
    extra: BTreeSet<String>,
    host: HostRef,
    now: Timestamp,
    project: Arc<ProjectStore>,
    machine: Option<Arc<MachineStore>>,
    guards: GuardInputs,
    executor: BackgroundExecutor,
}

async fn run_tx<T: Send + 'static>(input: TxInput, change: Change<T>) -> Result<Done<T>, TxError> {
    let project_before = input
        .project
        .read()
        .map_err(|error| TxError::Failed(format!("{error:#}")))?;
    let machine_before = match &input.machine {
        Some(machine) => Some(
            machine
                .read()
                .map_err(|error| TxError::Failed(format!("{error:#}")))?,
        ),
        None => None,
    };
    let mut tables: Vec<&PoolTable> = Vec::new();
    if input.stores.project {
        tables.push(&project_before);
    }
    if input.stores.machine {
        tables.extend(machine_before.as_ref());
    }
    let due = due_guards(
        &tables,
        &input.host,
        &input.extra,
        input.now,
        input.guards.interval,
    );
    let fresh = read_guards(&input.guards, due, input.now, &input.executor).await;
    let readings = fresh.readings;
    let mut done = Done {
        value: None,
        machine_grants: Vec::new(),
        project_grants: Vec::new(),
        pruned: Vec::new(),
        project_table: None,
        machine_table: None,
        notes: fresh.notes,
    };
    match (input.stores.machine, input.stores.project, &input.machine) {
        (true, true, Some(machine)) => {
            let first = transact_mixed(machine, &input.project, {
                let readings = readings.clone();
                move |m, p| {
                    m.record_readings(&readings);
                    p.record_readings(&readings);
                    change(Some(m), Some(p))
                }
            });
            match first {
                Ok(mixed) => {
                    done.value = Some(mixed.value);
                    done.machine_grants = mixed.machine_grants;
                    done.project_grants = mixed.project_grants;
                    done.pruned = pruned_of(StoreKind::Machine, mixed.machine_pruned);
                    done.pruned
                        .extend(pruned_of(StoreKind::Project, mixed.project_pruned));
                }
                Err(error) => {
                    let half = match error.downcast::<MachineHalfUnwritten>() {
                        Ok(half) => half,
                        Err(error) => return Err(classify(error)),
                    };
                    // The project half committed: a second mixed transaction converts the
                    // machine halves whose waiters are written, before anything is released.
                    let retry = transact_mixed(machine, &input.project, move |m, p| {
                        m.record_readings(&readings);
                        p.record_readings(&readings);
                        Ok(())
                    });
                    match retry {
                        Ok(mixed) => {
                            done.project_grants = half.project_grants;
                            done.project_grants.extend(mixed.project_grants);
                            done.machine_grants = mixed.machine_grants;
                            done.pruned = pruned_of(StoreKind::Machine, mixed.machine_pruned);
                            done.pruned
                                .extend(pruned_of(StoreKind::Project, mixed.project_pruned));
                        }
                        Err(error) => {
                            return Err(TxError::HalfUnwritten {
                                project_grants: half.project_grants,
                                reason: format!("{error:#}"),
                            });
                        }
                    }
                }
            }
        }
        (true, _, Some(machine)) => {
            let transacted = machine
                .try_transact_full(move |m| {
                    m.record_readings(&readings);
                    change(Some(m), None)
                })
                .map_err(classify)?;
            done.value = Some(transacted.value);
            done.machine_grants = transacted.grants;
            done.pruned = pruned_of(StoreKind::Machine, transacted.pruned);
        }
        (_, true, _) => {
            let transacted = input
                .project
                .try_transact_full(move |p| {
                    p.record_readings(&readings);
                    change(None, Some(p))
                })
                .map_err(classify)?;
            done.value = Some(transacted.value);
            done.project_grants = transacted.grants;
            done.pruned = pruned_of(StoreKind::Project, transacted.pruned);
        }
        _ => {
            done.value =
                Some(change(None, None).map_err(|error| TxError::Failed(format!("{error:#}")))?);
        }
    }
    done.project_table = input.project.read().ok();
    done.machine_table = input
        .machine
        .as_ref()
        .and_then(|machine| machine.read().ok());
    Ok(done)
}

/// Removes the waiters and holds of `request` (of `host`) from `table`; returns the holds.
fn remove_request(
    table: &mut PoolTable,
    request: RequestId,
    host: &HostRef,
) -> Vec<(String, Hold)> {
    let mut removed = Vec::new();
    for (name, pool) in &mut table.pools {
        pool.waiters
            .retain(|waiter| !(waiter.request == request && waiter.holder.host == *host));
        let (gone, kept): (Vec<Hold>, Vec<Hold>) = std::mem::take(&mut pool.holds)
            .into_iter()
            .partition(|hold| hold.request == request && hold.holder.host == *host);
        pool.holds = kept;
        removed.extend(gone.into_iter().map(|hold| (name.clone(), hold)));
    }
    removed
}

fn need_words(pool: &PoolState, need: &Need) -> String {
    match pool.kind {
        PoolKind::Rw => match need.mode.unwrap_or(NeedMode::Shared) {
            NeedMode::Shared => "shared".to_string(),
            NeedMode::Exclusive => "exclusive".to_string(),
        },
        PoolKind::Guard => "past its guard".to_string(),
        PoolKind::Mutex => "as its mutex".to_string(),
        PoolKind::Slots => match need.count() {
            1 => "1 slot".to_string(),
            count => format!("{count} slots"),
        },
    }
}

/// The refusal text of an enclosing hold the request cannot fit beside: the holder's label and
/// how it holds the pool.
fn own_hold(tables: &[&PoolTable], request: &ModelRequest, pool_name: &str) -> Refused {
    let pool = tables.iter().find_map(|table| table.pools.get(pool_name));
    let need = request.needs.iter().find(|need| need.pool == pool_name);
    let (Some(pool), Some(need)) = (pool, need) else {
        return Refused::OwnHold {
            pool: pool_name.to_string(),
            holder: request.holder.label.clone(),
            held: String::new(),
            asked: String::new(),
        };
    };
    let enclosing: Vec<&Hold> = pool
        .holds
        .iter()
        .filter(|hold| hold.holder.encloses(&request.holder))
        .collect();
    let holder = enclosing
        .first()
        .map(|hold| hold.holder.label.clone())
        .unwrap_or_else(|| request.holder.label.clone());
    let held = match pool.kind {
        PoolKind::Slots | PoolKind::Mutex => {
            let count: u32 = enclosing.iter().map(|hold| hold.need.count()).sum();
            match count {
                1 => "1 slot".to_string(),
                count => format!("{count} slots"),
            }
        }
        _ => enclosing
            .first()
            .map(|hold| need_words(pool, &hold.need))
            .unwrap_or_default(),
    };
    Refused::OwnHold {
        pool: pool_name.to_string(),
        holder,
        held,
        asked: need_words(pool, need),
    }
}

/// Whether `need` suits the kind of `pool`.
fn check_need(name: &str, pool: &PoolState, need: &Need) -> Result<(), Refused> {
    if need.mode.is_some() && pool.kind != PoolKind::Rw {
        return Err(Refused::BadNeed {
            pool: name.to_string(),
            reason: format!(
                "`mode` is only valid on an rw pool, and it is {}",
                pool.kind.word()
            ),
        });
    }
    if need.count.is_some() && matches!(pool.kind, PoolKind::Rw | PoolKind::Guard) {
        return Err(Refused::BadNeed {
            pool: name.to_string(),
            reason: format!(
                "`count` is only valid on a slots pool, and it is {}",
                pool.kind.word()
            ),
        });
    }
    let size = match pool.kind {
        PoolKind::Slots => Some(pool.size),
        PoolKind::Mutex => Some(1),
        _ => None,
    };
    if let Some(size) = size {
        if need.count() > size {
            return Err(Refused::BadNeed {
                pool: name.to_string(),
                reason: format!("count {} is above its size {size}", need.count()),
            });
        }
    }
    Ok(())
}

/// The enqueue transaction: judges the request against both tables (enclosing holds, deadlock),
/// then queues what remains, each half in its own table, `mixed` when it spans both.
fn enqueue_in(
    machine: Option<&mut PoolTable>,
    project: Option<&mut PoolTable>,
    request: &ModelRequest,
    machine_pools: &BTreeSet<String>,
    now: Timestamp,
) -> Result<Enqueued> {
    let satisfied = {
        let mut tables: Vec<&PoolTable> = Vec::new();
        tables.extend(machine.as_deref());
        tables.extend(project.as_deref());
        for need in &request.needs {
            let Some(pool) = tables.iter().find_map(|table| table.pools.get(&need.pool)) else {
                return Ok(Enqueued::Refused(Refused::Undeclared {
                    pool: need.pool.clone(),
                }));
            };
            if let Err(refused) = check_need(&need.pool, pool, need) {
                return Ok(Enqueued::Refused(refused));
            }
        }
        match judge(&tables, request) {
            Ok(satisfied) => satisfied,
            Err(ModelRefused::OwnHold { pool }) => {
                return Ok(Enqueued::Refused(own_hold(&tables, request, &pool)));
            }
            Err(ModelRefused::Deadlock { cycle }) => {
                return Ok(Enqueued::Refused(Refused::Deadlock { cycle }));
            }
        }
    };
    let remaining = satisfied.remaining(request);
    if remaining.needs.is_empty() {
        return Ok(Enqueued::Satisfied(satisfied.pools));
    }
    let (machine_needs, project_needs): (Vec<Need>, Vec<Need>) = remaining
        .needs
        .iter()
        .cloned()
        .partition(|need| machine_pools.contains(&need.pool));
    let mixed = !machine_needs.is_empty() && !project_needs.is_empty();
    let stores = Stores {
        project: !project_needs.is_empty(),
        machine: !machine_needs.is_empty(),
    };
    let queue = |table: Option<&mut PoolTable>, needs: Vec<Need>| -> Result<Option<Refused>> {
        if needs.is_empty() {
            return Ok(None);
        }
        let table = table.ok_or_else(|| anyhow!("a store of the request is not open"))?;
        let half = ModelRequest {
            needs,
            mixed,
            ..request.clone()
        };
        Ok(table.enqueue(&half, now).err().map(|error| match error {
            bongos_pools_model::QueueError::UnknownPool(pool) => Refused::Undeclared { pool },
            bongos_pools_model::QueueError::NamedTwice(pool) => Refused::BadNeed {
                pool,
                reason: "named twice in one request".to_string(),
            },
            bongos_pools_model::QueueError::AboveSize { pool, count, size } => Refused::BadNeed {
                pool,
                reason: format!("count {count} is above its size {size}"),
            },
        }))
    };
    // Queuing never fails after the checks above; an error aborts the whole transaction, so
    // nothing of a half is ever written alone.
    if let Some(refused) = queue(machine, machine_needs)? {
        anyhow::bail!("{refused}");
    }
    if let Some(refused) = queue(project, project_needs)? {
        anyhow::bail!("{refused}");
    }
    Ok(Enqueued::Queued {
        stores,
        satisfied: satisfied.pools,
    })
}

impl Shared {
    fn emit(&self, event: &PoolsEvent) {
        let listeners: Vec<_> = self.listeners.borrow().clone();
        for listener in listeners {
            listener(event);
        }
    }

    fn push(self: &Rc<Self>, op: Op) {
        self.state.borrow_mut().ops.push_back(op);
        self.kick();
    }

    fn kick(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if state.driving || state.ops.is_empty() {
                return;
            }
            state.driving = true;
        }
        let this = self.clone();
        self.parts
            .foreground
            .spawn(async move {
                loop {
                    let op = {
                        let mut state = this.state.borrow_mut();
                        match state.ops.pop_front() {
                            Some(op) => op,
                            None => {
                                state.driving = false;
                                break;
                            }
                        }
                    };
                    this.run(op).await;
                }
                this.ensure_timer();
            })
            .detach();
    }

    fn scope(&self, pool: &str) -> Option<PoolScope> {
        self.state.borrow().scopes.get(pool).copied()
    }

    fn stores_of(&self, needs: &[Need]) -> Stores {
        needs.iter().fold(Stores::default(), |stores, need| {
            stores.or(match self.scope(&need.pool) {
                Some(PoolScope::Machine) => Stores::of(StoreKind::Machine),
                _ => Stores::of(StoreKind::Project),
            })
        })
    }

    fn machine_pools(&self) -> BTreeSet<String> {
        self.state
            .borrow()
            .scopes
            .iter()
            .filter(|(_, scope)| **scope == PoolScope::Machine)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The stores this broker has queued waiters in.
    fn waiting_stores(&self) -> Stores {
        self.state
            .borrow()
            .pending
            .values()
            .filter(|pending| pending.queued)
            .fold(Stores::default(), |stores, pending| {
                stores.or(pending.stores)
            })
    }

    /// The stores holding a hold of this broker bound to a process (pruned by the poll).
    fn bound_stores(&self) -> Stores {
        self.state
            .borrow()
            .leases
            .values()
            .filter_map(Weak::upgrade)
            .filter(|lease| lease.process.get().is_some())
            .fold(Stores::default(), |stores, lease| {
                stores.or(Stores::of_holds(&lease.holds.borrow()))
            })
    }

    fn needs_poll(&self) -> bool {
        let state = self.state.borrow();
        if state.closed || state.draining {
            return false;
        }
        let waiting = !state.pending.is_empty() || !state.retry.is_empty();
        drop(state);
        waiting || self.bound_stores().any()
    }

    fn ensure_timer(self: &Rc<Self>) {
        if self.state.borrow().timer_on || !self.needs_poll() {
            return;
        }
        let generation = {
            let mut state = self.state.borrow_mut();
            state.timer_on = true;
            state.timer_generation
        };
        let weak = Rc::downgrade(self);
        let background = self.parts.background.clone();
        let interval = self.parts.timing.poll_interval;
        self.parts
            .foreground
            .spawn(async move {
                loop {
                    background.timer(interval).await;
                    let Some(this) = weak.upgrade() else {
                        return;
                    };
                    if this.state.borrow().timer_generation != generation {
                        return;
                    }
                    if !this.needs_poll() {
                        this.state.borrow_mut().timer_on = false;
                        return;
                    }
                    let queued = this
                        .state
                        .borrow()
                        .ops
                        .iter()
                        .any(|op| matches!(op, Op::Poll));
                    if !queued {
                        this.push(Op::Poll);
                    }
                }
            })
            .detach();
    }

    fn tx_input(&self, stores: Stores, extra: BTreeSet<String>, patient: bool) -> TxInput {
        TxInput {
            stores,
            extra,
            host: self.context.host,
            now: self.context.now(),
            project: self.parts.project_store.clone(),
            machine: self.parts.machine.as_ref().map(|machine| {
                if patient {
                    machine.patient.clone()
                } else {
                    machine.once.clone()
                }
            }),
            guards: self.guards.clone(),
            executor: self.parts.background.clone(),
        }
    }

    /// Runs `change` over `stores` and every store this broker waits in, on the background
    /// executor, then takes in what it pruned and granted.
    async fn tx<T: Send + 'static>(
        self: &Rc<Self>,
        stores: Stores,
        extra: BTreeSet<String>,
        change: Change<T>,
    ) -> Result<Done<T>, TxError> {
        let mut stores = stores.or(self.waiting_stores());
        if self.parts.machine.is_none() {
            stores.machine = false;
        }
        let input = self.tx_input(stores, extra, true);
        let result = self
            .parts
            .background
            .spawn(async move { run_tx(input, change).await })
            .await;
        match &result {
            Ok(done) => self.absorb(done),
            Err(TxError::HalfUnwritten {
                project_grants,
                reason,
            }) => {
                // Committed in the project store, never in the machine store, even on a retry. A
                // request of the project store alone is granted; a mixed one, whose machine half
                // stays a waiter, is failed and both its halves withdrawn.
                self.take_grants(StoreKind::Project, project_grants);
                let stuck: BTreeSet<RequestId> = project_grants
                    .iter()
                    .map(|grant| grant.request)
                    .filter(|request| self.state.borrow().pending.contains_key(request))
                    .collect();
                for request in stuck {
                    self.fail(
                        request,
                        Refused::Store {
                            reason: reason.clone(),
                        },
                    );
                }
            }
            Err(_) => {}
        }
        result
    }

    /// Takes in a transaction's tables, prunings, notes and grants.
    fn absorb<T>(self: &Rc<Self>, done: &Done<T>) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(table) = &done.project_table {
                state.project_table = table.clone();
            }
            if let Some(table) = &done.machine_table {
                state.machine_table = table.clone();
            }
            for (pool, note) in &done.notes {
                state.guard_notes.insert(pool.clone(), note.clone());
            }
        }
        for (_, pool, hold) in &done.pruned {
            log::info!(
                "pools: released {pool}: holder {} exited",
                hold.holder.label.0
            );
            self.emit(&PoolsEvent::Released {
                pool: pool.clone(),
                owner: hold.holder.path.owner.clone(),
                why: ReleaseWhy::Dead,
            });
        }
        self.take_grants(StoreKind::Machine, &done.machine_grants);
        self.take_grants(StoreKind::Project, &done.project_grants);
    }

    fn pool_kind(&self, store: StoreKind, pool: &str) -> Option<PoolKind> {
        let state = self.state.borrow();
        let table = match store {
            StoreKind::Project => &state.project_table,
            StoreKind::Machine => &state.machine_table,
        };
        table.pools.get(pool).map(|pool| pool.kind)
    }

    fn take_grants(self: &Rc<Self>, store: StoreKind, grants: &[Grant]) {
        for grant in grants {
            let holds: Vec<HoldId> = grant
                .pools
                .iter()
                .filter(|granted| self.pool_kind(store, &granted.pool) != Some(PoolKind::Guard))
                .map(|granted| HoldId {
                    store,
                    pool: granted.pool.clone(),
                    ticket: granted.ticket,
                })
                .collect();
            let complete = {
                let mut state = self.state.borrow_mut();
                match state.pending.get_mut(&grant.request) {
                    Some(pending) => {
                        pending.partial.extend(holds.iter().cloned());
                        pending.granted_in = pending.granted_in.or(Stores::of(store));
                        Some(pending.queued && pending.granted_in.covers(pending.stores))
                    }
                    None => None,
                }
            };
            match complete {
                Some(true) => self.deliver(grant.request),
                Some(false) => {}
                // Nobody waits for it any more (its wait was dropped while the grant was in
                // flight): release what was granted.
                None => {
                    if !holds.is_empty() {
                        self.push(Op::Release {
                            request: grant.request,
                            holds,
                        });
                    }
                }
            }
        }
    }

    /// Hands the request's lease to whoever waits for it.
    fn deliver(self: &Rc<Self>, request: RequestId) {
        let Some(pending) = self.state.borrow_mut().pending.remove(&request) else {
            return;
        };
        let now = self.context.now();
        let waited = Duration::from_millis(now.0.saturating_sub(pending.since.0));
        for hold in &pending.partial {
            log::info!(
                "pools: granted {} to {} after {} s",
                hold.pool,
                pending.request.holder.label.0,
                waited.as_secs()
            );
            self.emit(&PoolsEvent::Granted {
                pool: hold.pool.clone(),
                owner: pending.request.holder.path.owner.clone(),
                waited,
            });
        }
        match pending.answer {
            Answer::Waiter(reply) => {
                let lease = self.lease(pending.request, pending.partial, pending.satisfied);
                // A wait dropped after the grant: the lease comes back and is dropped, which
                // releases it.
                if let Err(unwanted) = reply.send(Ok(lease)) {
                    drop(unwanted);
                }
            }
            Answer::Lease(weak) => match weak.upgrade() {
                Some(lease) => {
                    *lease.holds.borrow_mut() = pending.partial;
                    self.state
                        .borrow_mut()
                        .leases
                        .insert(request, Rc::downgrade(&lease));
                }
                None => {
                    if !pending.partial.is_empty() {
                        self.push(Op::Release {
                            request,
                            holds: pending.partial,
                        });
                    }
                }
            },
        }
    }

    fn lease(
        self: &Rc<Self>,
        request: ModelRequest,
        holds: Vec<HoldId>,
        satisfied: Vec<String>,
    ) -> Lease {
        let id = request.id;
        let inner = Rc::new(LeaseInner {
            shared: self.clone(),
            request,
            holds: RefCell::new(holds),
            satisfied,
            process: Cell::new(None),
        });
        self.state
            .borrow_mut()
            .leases
            .insert(id, Rc::downgrade(&inner));
        Lease { inner }
    }

    /// Answers the request with `refused` and withdraws whatever of it was written.
    fn fail(self: &Rc<Self>, request: RequestId, refused: Refused) {
        let Some(pending) = self.state.borrow_mut().pending.remove(&request) else {
            return;
        };
        log::info!(
            "pools: refused {}: {refused}",
            pending.request.holder.label.0
        );
        let stores = pending.needs_stores;
        match pending.answer {
            Answer::Waiter(reply) => {
                reply.send(Err(refused)).ok();
            }
            Answer::Lease(_) => {}
        }
        self.push(Op::Withdraw { request, stores });
    }

    /// The wait of `request` was dropped: it leaves the line in every store.
    fn forget(self: &Rc<Self>, request: RequestId) {
        let pending = {
            let mut state = self.state.borrow_mut();
            state.given_up.retain(|(given, _, _)| given.id != request);
            state.pending.remove(&request)
        };
        if let Some(pending) = pending {
            self.push(Op::Withdraw {
                request,
                stores: pending.needs_stores,
            });
        }
    }

    fn holder_of(&self, mut holder: Holder) -> Holder {
        holder.project = self.parts.project.id.clone();
        holder.host = self.context.host;
        holder
    }

    /// Refuses at once what can never be granted here: draining, an undeclared pool, a kind
    /// clash.
    fn refuse_early(&self, needs: &[Need]) -> Option<Refused> {
        let state = self.state.borrow();
        if state.closed {
            return Some(Refused::Closed);
        }
        if state.draining {
            return Some(Refused::Draining);
        }
        for need in needs {
            if let Some(clash) = state.clashes.get(&need.pool) {
                return Some(Refused::KindClash {
                    pool: need.pool.clone(),
                    set_by: clash.set_by.clone(),
                    text: clash.to_string(),
                });
            }
            if !state.scopes.contains_key(&need.pool) {
                return Some(Refused::Undeclared {
                    pool: need.pool.clone(),
                });
            }
        }
        None
    }

    fn acquire(self: &Rc<Self>, request: Request) -> Waiting {
        let (reply, receiver) = oneshot::channel();
        let id = next_request();
        let now = self.context.now();
        let holder = self.holder_of(request.holder);
        let waiting = Waiting {
            shared: self.clone(),
            request: id,
            first_pool: request.needs.first().map(|need| need.pool.clone()),
            since: now,
            receiver: Some(receiver),
        };
        let model = ModelRequest {
            id,
            holder,
            mixed: false,
            needs: request.needs,
        };
        if let Some(refused) = self.refuse_early(&model.needs) {
            log::info!("pools: refused {}: {refused}", model.holder.label.0);
            reply.send(Err(refused)).ok();
            return waiting;
        }
        if model.needs.is_empty() {
            let lease = self.lease(model, Vec::new(), Vec::new());
            reply.send(Ok(lease)).ok();
            return waiting;
        }
        let needs_stores = self.stores_of(&model.needs);
        self.state.borrow_mut().pending.insert(
            id,
            Pending {
                request: model,
                answer: Answer::Waiter(reply),
                needs_stores,
                stores: Stores::default(),
                queued: false,
                granted_in: Stores::default(),
                partial: Vec::new(),
                satisfied: Vec::new(),
                since: now,
            },
        );
        self.push(Op::Enqueue(id));
        waiting
    }

    async fn run(self: &Rc<Self>, op: Op) {
        match op {
            Op::Enqueue(id) => self.run_enqueue(id).await,
            Op::Withdraw { request, stores } => {
                let host = self.context.host;
                let result = self
                    .tx(
                        stores,
                        BTreeSet::new(),
                        Box::new(move |machine, project| {
                            for table in [machine, project].into_iter().flatten() {
                                remove_request(table, request, &host);
                            }
                            Ok(())
                        }),
                    )
                    .await;
                self.after(result, Op::Withdraw { request, stores });
            }
            Op::Release { request, holds } => {
                let host = self.context.host;
                let project_id = self.parts.project.id.clone();
                let tickets = holds.clone();
                let result = self
                    .tx(
                        Stores::of_holds(&holds),
                        BTreeSet::new(),
                        Box::new(move |mut machine, mut project| {
                            let mut released = Vec::new();
                            for hold in &tickets {
                                let table = match hold.store {
                                    StoreKind::Machine => machine.as_deref_mut(),
                                    StoreKind::Project => project.as_deref_mut(),
                                };
                                let Some(table) = table else {
                                    continue;
                                };
                                let mine = table.pools.get(&hold.pool).is_some_and(|pool| {
                                    pool.holds.iter().any(|held| {
                                        held.ticket == hold.ticket
                                            && held.holder.host == host
                                            && held.holder.project == project_id
                                    })
                                });
                                if mine {
                                    released.extend(
                                        table
                                            .release(&hold.pool, hold.ticket)
                                            .map(|held| (hold.pool.clone(), held)),
                                    );
                                }
                            }
                            Ok(released)
                        }),
                    )
                    .await;
                if let Ok(Done {
                    value: Some(released),
                    ..
                }) = &result
                {
                    for (pool, hold) in released {
                        log::info!("pools: released {pool} by {}", hold.holder.label.0);
                        self.emit(&PoolsEvent::Released {
                            pool: pool.clone(),
                            owner: hold.holder.path.owner.clone(),
                            why: ReleaseWhy::Released,
                        });
                    }
                }
                self.after(result, Op::Release { request, holds });
            }
            Op::Bind { holds, process } => {
                let tickets = holds.clone();
                let result = self
                    .tx(
                        Stores::of_holds(&holds),
                        BTreeSet::new(),
                        Box::new(move |mut machine, mut project| {
                            let mut bound = 0;
                            for hold in &tickets {
                                let table = match hold.store {
                                    StoreKind::Machine => machine.as_deref_mut(),
                                    StoreKind::Project => project.as_deref_mut(),
                                };
                                let Some(pool) =
                                    table.and_then(|table| table.pools.get_mut(&hold.pool))
                                else {
                                    continue;
                                };
                                for held in &mut pool.holds {
                                    if held.ticket == hold.ticket {
                                        held.holder.process = Some(process);
                                        bound += 1;
                                    }
                                }
                            }
                            Ok(bound)
                        }),
                    )
                    .await;
                if let Ok(Done { value: Some(0), .. }) = &result {
                    // The hold went before its process was known (pruned, drained or released):
                    // nothing to bind.
                    log::info!("pools: process {} bound to no hold", process.pid);
                }
                self.after(result, Op::Bind { holds, process });
            }
            Op::Poll => self.run_poll().await,
            Op::Drain { holds, done } => {
                self.run_drain(holds).await;
                if let Some(done) = done {
                    done.send(()).ok();
                }
            }
        }
    }

    /// Keeps an op that could not run for the next poll.
    fn after<T>(self: &Rc<Self>, result: Result<Done<T>, TxError>, op: Op) {
        match result {
            Ok(Done { value: Some(_), .. }) => {}
            Ok(_) | Err(_) => {
                if let Err(error) = &result {
                    log::warn!(
                        "pools: a store change will be tried again: {}",
                        error.reason()
                    );
                }
                self.state.borrow_mut().retry.push(op);
            }
        }
    }

    async fn run_enqueue(self: &Rc<Self>, id: RequestId) {
        let (request, needs_stores) = {
            let state = self.state.borrow();
            let Some(pending) = state.pending.get(&id) else {
                return;
            };
            if pending.queued {
                return;
            }
            (pending.request.clone(), pending.needs_stores)
        };
        // Covered whole by the holds of enclosing holders (this broker's own grants, so the last
        // tables it read show them): granted at once, without touching a store.
        let covered = {
            let state = self.state.borrow();
            let tables = [&state.project_table, &state.machine_table];
            judge(&tables, &request)
                .ok()
                .filter(|satisfied| satisfied.remaining(&request).needs.is_empty())
        };
        if let Some(satisfied) = covered {
            if let Some(pending) = self.state.borrow_mut().pending.get_mut(&id) {
                pending.satisfied = satisfied.pools;
                pending.queued = true;
            }
            self.deliver(id);
            return;
        }
        let extra: BTreeSet<String> = request.needs.iter().map(|need| need.pool.clone()).collect();
        let machine_pools = self.machine_pools();
        let now = self.context.now();
        let result = self
            .tx(
                needs_stores,
                extra,
                Box::new(move |machine, project| {
                    enqueue_in(machine, project, &request, &machine_pools, now)
                }),
            )
            .await;
        match result {
            Ok(Done {
                value: Some(Enqueued::Refused(refused)),
                ..
            }) => self.fail(id, refused),
            Ok(Done {
                value: Some(Enqueued::Satisfied(satisfied)),
                ..
            }) => {
                if let Some(pending) = self.state.borrow_mut().pending.get_mut(&id) {
                    pending.satisfied = satisfied;
                    pending.queued = true;
                }
                self.deliver(id);
            }
            Ok(Done {
                value: Some(Enqueued::Queued { stores, satisfied }),
                ..
            }) => {
                // The grants of this very transaction were taken in before `queued` was set.
                let complete = {
                    let mut state = self.state.borrow_mut();
                    match state.pending.get_mut(&id) {
                        Some(pending) => {
                            pending.stores = stores;
                            pending.queued = true;
                            pending.satisfied = satisfied;
                            Some(pending.granted_in.covers(stores))
                        }
                        None => None,
                    }
                };
                match complete {
                    Some(true) => self.deliver(id),
                    Some(false) => self.announce(id),
                    None => {}
                }
            }
            Ok(Done { value: None, .. }) => {
                // The project half committed but `machine.toml` was not written even on the
                // retry: the request cannot stand in one store alone.
                self.fail(
                    id,
                    Refused::Store {
                        reason: "machine.toml could not be written".to_string(),
                    },
                );
            }
            Err(TxError::Busy(reason)) => {
                log::info!("pools: queuing waits for the machine store: {reason}");
                self.state.borrow_mut().retry.push(Op::Enqueue(id));
            }
            Err(error) => self.fail(
                id,
                Refused::Store {
                    reason: error.reason(),
                },
            ),
        }
    }

    fn announce(self: &Rc<Self>, id: RequestId) {
        let Some(status) = self.status(id) else {
            return;
        };
        let owner = {
            let state = self.state.borrow();
            let Some(pending) = state.pending.get(&id) else {
                return;
            };
            pending.request.holder.path.owner.clone()
        };
        self.emit(&PoolsEvent::Waiting {
            pool: status.pool,
            owner,
            position: status.position,
        });
    }

    async fn run_poll(self: &Rc<Self>) {
        let retry = std::mem::take(&mut self.state.borrow_mut().retry);
        {
            let mut state = self.state.borrow_mut();
            for op in retry.into_iter().rev() {
                state.ops.push_front(op);
            }
        }
        let stores = self.waiting_stores().or(self.bound_stores());
        let first = self.state.borrow().project_table.pools.is_empty()
            && self.state.borrow().machine_table.pools.is_empty();
        // The first poll reads (and prunes) both stores.
        let stores = if first {
            Stores {
                project: true,
                machine: true,
            }
        } else {
            stores
        };
        if stores.any() {
            let result = self
                .tx(stores, BTreeSet::new(), Box::new(|_, _| Ok(())))
                .await;
            if let Err(error) = result {
                log::warn!("pools: polling the stores: {}", error.reason());
            }
        }
        self.flag_long_holds();
    }

    fn flag_long_holds(self: &Rc<Self>) {
        let Some(max_hold) = self.parts.timing.max_hold else {
            return;
        };
        let now = self.context.now();
        let host = self.context.host;
        let project_id = &self.parts.project.id;
        let mut events = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            let mut found = Vec::new();
            for (store, table) in [
                (StoreKind::Project, &state.project_table),
                (StoreKind::Machine, &state.machine_table),
            ] {
                for (name, pool) in &table.pools {
                    let ours = |hold: &&Hold| {
                        hold.holder.host == host && hold.holder.project == *project_id
                    };
                    for hold in pool.holds.iter().filter(ours) {
                        let held = Duration::from_millis(now.0.saturating_sub(hold.since.0));
                        if held > max_hold {
                            found.push((
                                HoldId {
                                    store,
                                    pool: name.clone(),
                                    ticket: hold.ticket,
                                },
                                hold.holder.path.owner.clone(),
                                held,
                            ));
                        }
                    }
                }
            }
            for (id, owner, held) in found {
                if state.flagged.insert(id.clone()) {
                    events.push(PoolsEvent::HoldTooLong {
                        pool: id.pool,
                        owner,
                        held,
                    });
                }
            }
        }
        for event in events {
            self.emit(&event);
        }
    }

    async fn run_drain(self: &Rc<Self>, holds: bool) {
        let host = self.context.host;
        if holds {
            let mut state = self.state.borrow_mut();
            let pending = std::mem::take(&mut state.pending);
            for (_, pending) in pending {
                state
                    .given_up
                    .push((pending.request, pending.answer, false));
            }
            let leases: Vec<Rc<LeaseInner>> =
                state.leases.values().filter_map(Weak::upgrade).collect();
            drop(state);
            for lease in leases {
                let live = lease
                    .process
                    .get()
                    .is_some_and(|process| self.context.alive(process.pid, process.start));
                if live || lease.holds.borrow().is_empty() {
                    continue;
                }
                lease.holds.borrow_mut().clear();
                self.state.borrow_mut().given_up.push((
                    lease.request.clone(),
                    Answer::Lease(Rc::downgrade(&lease)),
                    true,
                ));
            }
        }
        let context = self.context.clone();
        let project_id = self.parts.project.id.clone();
        let remove = move |table: &mut PoolTable| {
            let ours = |holder: &Holder| holder.host == host && holder.project == project_id;
            for pool in table.pools.values_mut() {
                pool.waiters.retain(|waiter| !ours(&waiter.holder));
                if holds {
                    pool.holds.retain(|hold| {
                        !ours(&hold.holder)
                            || hold
                                .holder
                                .process
                                .is_some_and(|process| context.alive(process.pid, process.start))
                    });
                }
            }
        };
        // One transaction per store, never both at once. Each converts only this project's
        // waiters of this host (the stores' context names the project), and the change removed
        // them all, so nothing is converted here, another project's waiters least of all.
        let project = self.parts.project_store.clone();
        let machine = self.parts.machine.as_ref().map(|m| m.patient.clone());
        let remove_machine = remove.clone();
        let result = self
            .parts
            .background
            .spawn(async move {
                let project_result = project.transact(remove).map(|_| ());
                let machine_result = match &machine {
                    Some(machine) => machine.transact(remove_machine).map(|_| ()),
                    None => Ok(()),
                };
                (
                    project_result,
                    machine_result,
                    project.read().ok(),
                    machine.as_ref().and_then(|machine| machine.read().ok()),
                )
            })
            .await;
        let (project_result, machine_result, project_table, machine_table) = result;
        for error in [project_result.err(), machine_result.err()]
            .into_iter()
            .flatten()
        {
            log::error!("pools: draining the pool stores: {error:#}");
        }
        let mut state = self.state.borrow_mut();
        if let Some(table) = project_table {
            state.project_table = table;
        }
        if let Some(table) = machine_table {
            state.machine_table = table;
        }
        state
            .retry
            .retain(|op| !matches!(op, Op::Enqueue(_) | Op::Poll));
    }

    fn status(&self, request: RequestId) -> Option<WaitStatus51> {
        let state = self.state.borrow();
        let mut best: Option<WaitStatus51> = None;
        for table in [&state.project_table, &state.machine_table] {
            let ModelWaitStatus::Waiting(waits) = wait_status(table, request) else {
                continue;
            };
            for wait in waits {
                let pool = &table.pools[&wait.pool];
                let since = pool
                    .waiters
                    .iter()
                    .find(|waiter| waiter.ticket == wait.ticket)
                    .map_or(Timestamp::default(), |waiter| waiter.since);
                let status = WaitStatus51 {
                    pool: wait.pool.clone(),
                    position: u32::try_from(wait.ahead + 1).unwrap_or(u32::MAX),
                    since,
                    holders: pool
                        .holds
                        .iter()
                        .map(|hold| hold.holder.label.clone())
                        .collect(),
                    guard_note: state.guard_notes.get(&wait.pool).cloned(),
                    blocked: wait.blocked,
                };
                let better = match &best {
                    None => true,
                    Some(best) => best.blocked.is_none() && status.blocked.is_some(),
                };
                if better {
                    best = Some(status);
                }
            }
        }
        best
    }

    fn try_now(self: &Rc<Self>, request: Request) -> Option<Lease> {
        if self.refuse_early(&request.needs).is_some() {
            return None;
        }
        let model = ModelRequest {
            id: next_request(),
            holder: self.holder_of(request.holder),
            mixed: false,
            needs: request.needs,
        };
        if model.needs.is_empty() {
            return Some(self.lease(model, Vec::new(), Vec::new()));
        }
        let named: BTreeSet<String> = model.needs.iter().map(|need| need.pool.clone()).collect();
        // A request asked before this one but not written yet would be jumped.
        let in_flight = self.state.borrow().pending.values().any(|pending| {
            !pending.queued
                && pending
                    .request
                    .needs
                    .iter()
                    .any(|need| named.contains(&need.pool))
        });
        if in_flight {
            return None;
        }
        let now = self.context.now();
        let readings = {
            let state = self.state.borrow();
            let tables = [&state.project_table, &state.machine_table];
            let due = due_guards(
                &tables,
                &self.context.host,
                &named,
                now,
                self.guards.interval,
            );
            // A due script guard is never run here: the foreground does not wait for it.
            read_guards_now(&self.guards, due, now)?
        };
        let machine_pools = self.machine_pools();
        let stores = self.stores_of(&model.needs);
        let result = admit_in_stores(
            &self.parts.project_store,
            self.parts.machine.as_ref().map(|machine| &machine.once),
            stores,
            &model,
            &machine_pools,
            &readings,
            now,
        );
        match result {
            Ok((holds, satisfied, done)) => {
                self.absorb(&done);
                for hold in &holds {
                    log::info!(
                        "pools: granted {} to {} at once",
                        hold.pool,
                        model.holder.label.0
                    );
                    self.emit(&PoolsEvent::Granted {
                        pool: hold.pool.clone(),
                        owner: model.holder.path.owner.clone(),
                        waited: Duration::ZERO,
                    });
                }
                Some(self.lease(model, holds, satisfied))
            }
            Err(error) => {
                log::debug!("pools: not now for {}: {error:#}", model.holder.label.0);
                None
            }
        }
    }
}

type Admitted = (Vec<HoldId>, Vec<String>);

/// Admits the needs of `model` that live in `table` at once, or fails, changing nothing.
fn admit_half(
    table: &mut PoolTable,
    model: &ModelRequest,
    needs: Vec<Need>,
    readings: &GuardReadings,
    now: Timestamp,
    store: StoreKind,
) -> Result<Admitted> {
    if needs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    table.record_readings(readings);
    let half = ModelRequest {
        needs,
        ..model.clone()
    };
    let grant = admit_now(table, &half, &GuardReadings::default(), now)
        .map_err(|blocked| anyhow!("not now: {blocked:?}"))?;
    let holds = grant
        .pools
        .iter()
        .filter(|granted| {
            table
                .pools
                .get(&granted.pool)
                .is_some_and(|pool| pool.kind != PoolKind::Guard)
        })
        .map(|granted| HoldId {
            store,
            pool: granted.pool.clone(),
            ticket: granted.ticket,
        })
        .collect();
    Ok((holds, grant.satisfied))
}

/// The synchronous admission of [`PoolBroker::try_now`]: one transaction over the stores the
/// request names, the machine lock tried once. A blocked pool or a busy lock is an error and
/// writes nothing.
fn admit_in_stores(
    project: &Arc<ProjectStore>,
    machine: Option<&Arc<MachineStore>>,
    stores: Stores,
    model: &ModelRequest,
    machine_pools: &BTreeSet<String>,
    readings: &GuardReadings,
    now: Timestamp,
) -> Result<(Vec<HoldId>, Vec<String>, Done<()>)> {
    let (machine_needs, project_needs): (Vec<Need>, Vec<Need>) = model
        .needs
        .iter()
        .cloned()
        .partition(|need| machine_pools.contains(&need.pool));
    let mut done = Done {
        value: Some(()),
        machine_grants: Vec::new(),
        project_grants: Vec::new(),
        pruned: Vec::new(),
        project_table: None,
        machine_table: None,
        notes: BTreeMap::new(),
    };
    let (holds, satisfied) = match (stores.machine, stores.project) {
        (true, true) => {
            let machine = machine.ok_or_else(|| anyhow!("no machine store is open"))?;
            let mixed = transact_mixed(machine, project, |m, p| {
                let (mut holds, mut satisfied) =
                    admit_half(m, model, machine_needs, readings, now, StoreKind::Machine)?;
                let (more_holds, more_satisfied) =
                    admit_half(p, model, project_needs, readings, now, StoreKind::Project)?;
                holds.extend(more_holds);
                satisfied.extend(more_satisfied);
                Ok((holds, satisfied))
            })?;
            done.machine_grants = mixed.machine_grants;
            done.project_grants = mixed.project_grants;
            done.pruned = pruned_of(StoreKind::Machine, mixed.machine_pruned);
            done.pruned
                .extend(pruned_of(StoreKind::Project, mixed.project_pruned));
            mixed.value
        }
        (true, false) => {
            let machine = machine.ok_or_else(|| anyhow!("no machine store is open"))?;
            let transacted = machine.try_transact_full(|m| {
                admit_half(m, model, machine_needs, readings, now, StoreKind::Machine)
            })?;
            done.machine_grants = transacted.grants;
            done.pruned = pruned_of(StoreKind::Machine, transacted.pruned);
            transacted.value
        }
        _ => {
            let transacted = project.try_transact_full(|p| {
                admit_half(p, model, project_needs, readings, now, StoreKind::Project)
            })?;
            done.project_grants = transacted.grants;
            done.pruned = pruned_of(StoreKind::Project, transacted.pruned);
            transacted.value
        }
    };
    done.project_table = project.read().ok();
    done.machine_table = machine.and_then(|machine| machine.read().ok());
    Ok((holds, satisfied, done))
}
