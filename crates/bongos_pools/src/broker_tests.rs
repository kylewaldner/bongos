//! The broker against real stores in temporary directories, with the liveness of every process,
//! the clock and the guard readings decided by the test. Time moves only when the test advances
//! both the executor's clock and the stores' clock.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bongos_pools_model::{
    Blocked, GuardCheck, GuardValue, Holder, HolderLabel, HolderPath, HostRef, Level, Need,
    NeedMode, OwnerKey, PoolKind, PoolTable,
};
use bongos_project_repo::Location;
use bongos_workflow::process::ProcessRecord;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::TestAppContext;

use crate::store_test_support::{Liveness, host, writer};
use crate::{
    BrokerParts, BrokerTiming, DeclareOutcome, GuardReaders, LOCK_FILE, Lease, LockWait, MachineStore,
    MachineStores, PoolBroker, PoolConfig, PoolsEvent, ProjectRef, ProjectStore, Refused,
    ReleaseWhy, Request, ScriptOutput, ScriptRun, StoreContext, Waiting,
};

pub(crate) const START: u64 = 1_700_000_000_000;
pub(crate) const POLL: Duration = Duration::from_millis(1000);
pub(crate) const GIB: u64 = 1024 * 1024 * 1024;

/// The stores' clock, moved by the test.
#[derive(Clone)]
pub(crate) struct Clock(Arc<AtomicU64>);

impl Clock {
    pub(crate) fn new() -> Clock {
        Clock(Arc::new(AtomicU64::new(START)))
    }

    fn advance(&self, by: Duration) {
        self.0
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

/// Guard readings the test sets, and how often each was read.
#[derive(Default)]
pub(crate) struct FakeReaders {
    pub memory: Mutex<Option<u64>>,
    pub script: Mutex<Option<Result<ScriptOutput, String>>>,
    pub memory_reads: AtomicU64,
}

impl GuardReaders for FakeReaders {
    fn free_memory_bytes(&self) -> Option<u64> {
        self.memory_reads.fetch_add(1, Ordering::SeqCst);
        *self.memory.lock().unwrap()
    }

    fn free_disk_bytes(&self, _path: &Path) -> Option<u64> {
        None
    }

    fn run_script(
        &self,
        _run: ScriptRun,
        _executor: gpui::BackgroundExecutor,
    ) -> BoxFuture<'static, Result<ScriptOutput, String>> {
        let answer = self
            .script
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Err("no script answer set".to_string()));
        async move { answer }.boxed()
    }
}

/// One project's broker and what the test needs to drive it.
pub(crate) struct Project {
    pub broker: Rc<PoolBroker>,
    pub project_store: Arc<ProjectStore>,
    pub machine: Arc<MachineStore>,
    pub readers: Arc<FakeReaders>,
    pub events: Rc<RefCell<Vec<PoolsEvent>>>,
    _root: tempfile::TempDir,
}

/// Every broker of a test shares one machine directory, one liveness and one clock.
pub(crate) struct World {
    pub machine_dir: tempfile::TempDir,
    pub live: Arc<Liveness>,
    pub clock: Clock,
}

impl World {
    pub(crate) fn new() -> World {
        World {
            machine_dir: tempfile::tempdir().unwrap(),
            live: Liveness::new(),
            clock: Clock::new(),
        }
    }

    pub(crate) fn context(&self, host: HostRef) -> StoreContext {
        let live = self.live.clone();
        let clock = self.clock.clone();
        StoreContext::new(
            host,
            move |pid, start| start == 1_000 + u64::from(pid) && !live.is_dead(pid),
            move || bongos_pools_model::Timestamp(clock.0.load(Ordering::SeqCst)),
        )
    }

    /// A broker of project `id` for `host`, with `project` and `machine` pools declared and none
    /// refused.
    pub(crate) fn project(
        &self,
        cx: &mut TestAppContext,
        id: &str,
        host: HostRef,
        project: Vec<PoolConfig>,
        machine: Vec<PoolConfig>,
    ) -> Project {
        let (project, outcome) = self.project_declared(cx, id, host, project, machine);
        assert!(outcome.refused.is_empty(), "{outcome:?}");
        project
    }

    /// A broker of project `id` for `host` and what declaring its pools gave.
    pub(crate) fn project_declared(
        &self,
        cx: &mut TestAppContext,
        id: &str,
        host: HostRef,
        project: Vec<PoolConfig>,
        machine: Vec<PoolConfig>,
    ) -> (Project, DeclareOutcome) {
        let root = tempfile::tempdir().unwrap();
        let location = location(root.path(), id);
        let context = self.context(host);
        let (project_store, _) = ProjectStore::open(writer(&location), context.clone()).unwrap();
        let project_store = Arc::new(project_store);
        let stores = MachineStores::open(
            self.machine_dir.path().to_path_buf(),
            context,
            LockWait::once(),
            id,
        )
        .unwrap();
        let machine_store = stores.patient.clone();
        let readers = Arc::new(FakeReaders::default());
        let broker = Rc::new(PoolBroker::new(BrokerParts {
            project: ProjectRef {
                id: id.to_string(),
                name: format!("Project {id}"),
            },
            project_store: project_store.clone(),
            machine: Some(stores),
            readers: readers.clone(),
            definitions: None,
            disk_root: root.path().to_path_buf(),
            timing: timing(),
            foreground: cx.foreground_executor().clone(),
            background: cx.executor(),
            keep: None,
        }));
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        broker.subscribe(move |event| sink.borrow_mut().push(event.clone()));
        let declared = broker.declare(project, machine);
        cx.run_until_parked();
        let outcome = cx
            .foreground_executor()
            .block_on(declared)
            .expect("declaring the pools");
        let project = Project {
            broker,
            project_store,
            machine: machine_store,
            readers,
            events,
            _root: root,
        };
        (project, outcome)
    }

    /// Another broker of the same project with nothing declared, to stand in for one dropped.
    pub(crate) fn bare_broker(&self, cx: &TestAppContext, project: &Project) -> PoolBroker {
        PoolBroker::new(BrokerParts {
            project: project.broker.project().clone(),
            project_store: project.project_store.clone(),
            machine: None,
            readers: project.readers.clone(),
            definitions: None,
            disk_root: std::env::temp_dir(),
            timing: timing(),
            foreground: cx.foreground_executor().clone(),
            background: cx.executor(),
            keep: None,
        })
    }

    /// Moves both clocks on by `by`, one poll interval at a time, running everything due.
    pub(crate) fn advance(&self, cx: &mut TestAppContext, by: Duration) {
        let mut left = by;
        while !left.is_zero() {
            let step = left.min(POLL);
            self.clock.advance(step);
            cx.executor().advance_clock(step);
            cx.run_until_parked();
            left -= step;
        }
    }
}

pub(crate) fn timing() -> BrokerTiming {
    BrokerTiming {
        poll_interval: POLL,
        guard_interval: Duration::from_secs(5),
        guard_script_timeout: Duration::from_secs(30),
        max_hold: None,
    }
}

pub(crate) fn location(root: &Path, id: &str) -> Location {
    let folder = root.join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    Location {
        id: id.to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    }
}

pub(crate) fn pool(name: &str, kind: PoolKind, size: u32) -> PoolConfig {
    PoolConfig {
        name: name.to_string(),
        kind,
        size,
        guard: None,
        settle_seconds: 5,
    }
}

pub(crate) fn guarded(name: &str, kind: PoolKind, check: GuardCheck) -> PoolConfig {
    PoolConfig {
        guard: Some(check),
        ..pool(name, kind, 1)
    }
}

pub(crate) fn holder(owner: &str, level: Level) -> Holder {
    Holder {
        project: String::new(),
        path: HolderPath {
            owner: OwnerKey(owner.to_string()),
            level,
        },
        label: HolderLabel(format!("holder {owner}")),
        host: HostRef { pid: 0, start: 0 },
        process: None,
    }
}

pub(crate) fn need(pool: &str) -> Need {
    Need {
        pool: pool.to_string(),
        mode: None,
        count: None,
    }
}

pub(crate) fn ask(owner: &str, needs: &[Need]) -> Request {
    Request {
        needs: needs.to_vec(),
        holder: holder(owner, Level::Job),
    }
}

type Outcome = Rc<RefCell<Option<Result<Lease, Refused>>>>;

/// Awaits the wait in a task and keeps what it resolves to.
pub(crate) fn watch(cx: &TestAppContext, waiting: Waiting) -> Outcome {
    let outcome: Outcome = Rc::new(RefCell::new(None));
    let sink = outcome.clone();
    cx.foreground_executor()
        .spawn(async move {
            let answer = waiting.ready().await;
            *sink.borrow_mut() = Some(answer);
        })
        .detach();
    outcome
}

pub(crate) fn lease(outcome: &Outcome) -> Lease {
    match outcome.borrow_mut().take() {
        Some(Ok(lease)) => lease,
        other => panic!("expected a lease, got {other:?}"),
    }
}

fn holds(table: &PoolTable, pool: &str) -> Vec<String> {
    table.pools[pool]
        .holds
        .iter()
        .map(|hold| hold.holder.path.owner.0.clone())
        .collect()
}

fn waiters(table: &PoolTable, pool: &str) -> Vec<String> {
    table.pools[pool]
        .waiters
        .iter()
        .map(|waiter| waiter.holder.path.owner.0.clone())
        .collect()
}

/// The project's table and the machine's, as the stores hold them now.
fn read(project: &Project) -> (PoolTable, PoolTable) {
    use crate::PoolStore as _;
    (
        project.project_store.read().unwrap(),
        project.machine.read().unwrap(),
    )
}

#[gpui::test]
fn a_request_with_no_needs_is_granted_without_a_store(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    let outcome = watch(cx, a.broker.acquire(ask("job", &[])));
    cx.run_until_parked();
    let lease = lease(&outcome);
    assert!(lease.hold_ids().is_empty());
}

#[gpui::test]
fn dropping_a_waiting_removes_its_rows_and_the_next_waiter_is_granted(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    let first = watch(cx, a.broker.acquire(ask("first", &[need("db")])));
    cx.run_until_parked();
    let first = lease(&first);
    let second = a.broker.acquire(ask("second", &[need("db")]));
    let third = watch(cx, a.broker.acquire(ask("third", &[need("db")])));
    cx.run_until_parked();
    assert_eq!(waiters(&read(&a).0, "db"), vec!["second", "third"]);
    assert_eq!(second.status().position, 1);

    drop(second);
    cx.run_until_parked();
    assert_eq!(waiters(&read(&a).0, "db"), vec!["third"]);

    drop(first);
    cx.run_until_parked();
    let third = lease(&third);
    assert_eq!(holds(&read(&a).0, "db"), vec!["third"]);
    drop(third);
    cx.run_until_parked();
    assert!(holds(&read(&a).0, "db").is_empty());
}

#[gpui::test]
fn a_wait_dropped_after_its_grant_releases_the_grant(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    // Granted while nobody awaits it: the lease sits in the channel.
    let waiting = a.broker.acquire(ask("job", &[need("db")]));
    cx.run_until_parked();
    assert_eq!(holds(&read(&a).0, "db"), vec!["job"]);
    drop(waiting);
    cx.run_until_parked();
    assert!(holds(&read(&a).0, "db").is_empty());

    // Dropped while its enqueue was still in flight: nothing stays.
    let waiting = a.broker.acquire(ask("late", &[need("db")]));
    drop(waiting);
    cx.run_until_parked();
    let table = read(&a).0;
    assert!(holds(&table, "db").is_empty());
    assert!(waiters(&table, "db").is_empty());
}

#[gpui::test]
fn a_lease_bound_and_dropped_at_once_leaves_nothing(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    let outcome = watch(cx, a.broker.acquire(ask("job", &[need("db")])));
    cx.run_until_parked();
    let lease = lease(&outcome);
    // The bind and the release are both in flight; the release comes last.
    lease.bind_process(ProcessRecord {
        pid: 300,
        start_time: 1_300,
    });
    drop(lease);
    cx.run_until_parked();
    assert!(holds(&read(&a).0, "db").is_empty());
}

#[gpui::test]
fn a_pruned_hold_is_released_once_and_its_late_drop_does_nothing(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    let outcome = watch(cx, a.broker.acquire(ask("job", &[need("db")])));
    cx.run_until_parked();
    let held = lease(&outcome);
    held.bind_process(ProcessRecord {
        pid: 700,
        start_time: 1_700,
    });
    cx.run_until_parked();
    world.live.kill(700);
    world.advance(cx, POLL);
    assert!(
        holds(&read(&a).0, "db").is_empty(),
        "pruned within one poll"
    );
    assert!(a.events.borrow().iter().any(|event| matches!(
        event,
        PoolsEvent::Released {
            why: ReleaseWhy::Dead,
            ..
        }
    )));
    // A process record for the pruned hold binds nothing; the late drop releases nothing.
    held.bind_process(ProcessRecord {
        pid: 701,
        start_time: 1_701,
    });
    let next = watch(cx, a.broker.acquire(ask("next", &[need("db")])));
    cx.run_until_parked();
    let next = lease(&next);
    drop(held);
    cx.run_until_parked();
    assert_eq!(holds(&read(&a).0, "db"), vec!["next"]);
    drop(next);
}

#[gpui::test]
fn try_now_never_jumps_a_queued_or_unwritten_waiter(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("cpu", PoolKind::Slots, 2)],
        vec![],
    );
    let first = watch(cx, a.broker.acquire(ask("first", &[need("cpu")])));
    cx.run_until_parked();
    let _first = lease(&first);
    let two = Need {
        count: Some(2),
        ..need("cpu")
    };
    let _big = a.broker.acquire(ask("big", &[two]));
    // Asked but not written yet: a free slot is still not taken.
    assert!(a.broker.try_now(ask("part", &[need("cpu")])).is_none());
    cx.run_until_parked();
    assert_eq!(waiters(&read(&a).0, "cpu"), vec!["big"]);
    // Queued: one slot is free, but the waiter is first.
    assert!(a.broker.try_now(ask("part", &[need("cpu")])).is_none());
    assert_eq!(holds(&read(&a).0, "cpu"), vec!["first"]);
}

#[gpui::test]
fn try_now_answers_not_now_while_another_process_holds_the_machine_lock(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    let lock = lock_machine(&world);
    assert!(a.broker.try_now(ask("part", &[need("gpu")])).is_none());
    drop(lock);
    let lease = a
        .broker
        .try_now(ask("part", &[need("gpu")]))
        .expect("free now");
    assert_eq!(lease.hold_ids().len(), 1);
}

#[gpui::test]
fn two_brokers_share_a_machine_pool_and_each_converts_only_its_own(cx: &mut TestAppContext) {
    let world = World::new();
    let gpu = || vec![pool("gpu", PoolKind::Slots, 1)];
    let a = world.project(cx, "a", host(1), vec![], gpu());
    let b = world.project(cx, "b", host(2), vec![], gpu());
    let first = watch(cx, a.broker.acquire(ask("a-job", &[need("gpu")])));
    cx.run_until_parked();
    let first = lease(&first);
    let second = watch(cx, b.broker.acquire(ask("b-job", &[need("gpu")])));
    cx.run_until_parked();
    assert!(second.borrow().is_none());

    drop(first);
    cx.run_until_parked();
    let machine = read(&a).1;
    assert!(holds(&machine, "gpu").is_empty());
    assert_eq!(
        waiters(&machine, "gpu"),
        vec!["b-job"],
        "the first broker never converts the second's waiter"
    );
    world.advance(cx, POLL);
    let second = lease(&second);
    assert_eq!(holds(&read(&b).1, "gpu"), vec!["b-job"]);
    drop(second);
}

#[gpui::test]
fn acquire_answers_at_once_while_another_process_holds_the_machine_lock(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    let lock = lock_machine(&world);
    // The call returns a wait at once; queuing waits for the lock in the background.
    let waiting = a.broker.acquire(ask("job", &[need("gpu")]));
    let status = waiting.status_source();
    let outcome = watch(cx, waiting);
    cx.run_until_parked();
    assert!(outcome.borrow().is_none(), "nothing resolves while the lock is busy");
    assert_eq!(status.status().position, 0, "not written yet");
    assert!(waiters(&read(&a).1, "gpu").is_empty());
    drop(lock);
    world.advance(cx, POLL);
    let lease = lease(&outcome);
    assert_eq!(lease.hold_ids().len(), 1);
    assert_eq!(holds(&read(&a).1, "gpu"), vec!["job"]);
}

#[gpui::test]
fn a_machine_pool_another_project_set_with_another_kind_is_refused(cx: &mut TestAppContext) {
    let world = World::new();
    let _a = world.project(
        cx,
        "a",
        host(1),
        vec![],
        vec![pool("gpu", PoolKind::Slots, 2)],
    );
    let (b, outcome) = world.project_declared(
        cx,
        "b",
        host(2),
        vec![],
        vec![pool("gpu", PoolKind::Mutex, 1)],
    );
    assert_eq!(outcome.refused.len(), 1);
    let refused = watch(cx, b.broker.acquire(ask("job", &[need("gpu")])));
    cx.run_until_parked();
    match refused.borrow_mut().take() {
        Some(Err(Refused::KindClash { pool, set_by, text })) => {
            assert_eq!(pool, "gpu");
            assert_eq!(set_by.map(|project| project.id), Some("a".to_string()));
            assert_eq!(text, outcome.refused[0].to_string());
        }
        other => panic!("expected a kind clash, got {other:?}"),
    }
    assert!(b.broker.try_now(ask("part", &[need("gpu")])).is_none());
    assert!(waiters(&read(&b).1, "gpu").is_empty());
}

#[gpui::test]
fn two_brokers_of_one_process_never_convert_each_others_waiters(cx: &mut TestAppContext) {
    let world = World::new();
    let gpu = || vec![pool("gpu", PoolKind::Slots, 1)];
    // Both brokers run in the same process: one host.
    let a = world.project(cx, "a", host(1), vec![], gpu());
    let b = world.project(cx, "b", host(1), vec![], gpu());

    // A transaction of the first broker (the release of its hold) leaves the second's waiter for
    // the second broker, which converts it at its next poll.
    let first = watch(cx, a.broker.acquire(ask("a-job", &[need("gpu")])));
    cx.run_until_parked();
    let first = lease(&first);
    let second = watch(cx, b.broker.acquire(ask("b-job", &[need("gpu")])));
    cx.run_until_parked();
    drop(first);
    cx.run_until_parked();
    let machine = read(&a).1;
    assert!(holds(&machine, "gpu").is_empty());
    assert_eq!(waiters(&machine, "gpu"), vec!["b-job"]);
    world.advance(cx, POLL);
    let second = lease(&second);
    assert_eq!(holds(&read(&b).1, "gpu"), vec!["b-job"]);
    drop(second);
    cx.run_until_parked();

    // The first broker's drain (whose transaction removes its unbound hold) leaves the second's
    // waiter for the second broker too.
    let held = watch(cx, a.broker.acquire(ask("a-held", &[need("gpu")])));
    cx.run_until_parked();
    let _held = lease(&held);
    let next = watch(cx, b.broker.acquire(ask("b-next", &[need("gpu")])));
    cx.run_until_parked();
    let drained = a.broker.drain();
    cx.run_until_parked();
    cx.foreground_executor().block_on(drained);
    let machine = read(&a).1;
    assert!(holds(&machine, "gpu").is_empty(), "{machine:?}");
    assert_eq!(waiters(&machine, "gpu"), vec!["b-next"]);
    world.advance(cx, POLL);
    let next = lease(&next);
    assert_eq!(holds(&read(&b).1, "gpu"), vec!["b-next"]);
    drop(next);
    cx.run_until_parked();
    assert!(holds(&read(&b).1, "gpu").is_empty());
}

#[gpui::test]
fn a_machine_guard_uses_the_set_by_figure_and_one_stored_reading(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![],
        vec![guarded(
            "ram",
            PoolKind::Guard,
            GuardCheck::FreeMemoryGb(12.0),
        )],
    );
    let b = world.project(
        cx,
        "b",
        host(2),
        vec![],
        vec![guarded(
            "ram",
            PoolKind::Guard,
            GuardCheck::FreeMemoryGb(4.0),
        )],
    );
    *a.readers.memory.lock().unwrap() = Some(10 * GIB);
    *b.readers.memory.lock().unwrap() = Some(10 * GIB);
    let a_wait = a.broker.acquire(ask("a-job", &[need("ram")]));
    cx.run_until_parked();
    let b_wait = b.broker.acquire(ask("b-job", &[need("ram")]));
    cx.run_until_parked();
    assert_eq!(a.readers.memory_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        b.readers.memory_reads.load(Ordering::SeqCst),
        0,
        "the stored reading is fresh: the second broker reads nothing"
    );
    for waiting in [&a_wait, &b_wait] {
        match waiting.status().blocked {
            Some(Blocked::Guard {
                reading: GuardValue::FreeMemoryGb(value),
                threshold: GuardCheck::FreeMemoryGb(figure),
                ..
            }) => {
                assert!((value - 10.0).abs() < 1e-9);
                assert_eq!(figure, 12.0, "the figure of the project that set the pool");
            }
            other => panic!("expected the guard, got {other:?}"),
        }
    }
}

#[gpui::test]
fn release_and_cancel_of_a_mixed_request_remove_both_halves(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    let both = [need("gpu"), need("db")];
    let mixed = watch(cx, a.broker.acquire(ask("mixed", &both)));
    cx.run_until_parked();
    let mixed = lease(&mixed);
    let (project, machine) = read(&a);
    assert_eq!(holds(&project, "db"), vec!["mixed"]);
    assert_eq!(holds(&machine, "gpu"), vec!["mixed"]);
    drop(mixed);
    cx.run_until_parked();
    let (project, machine) = read(&a);
    assert!(holds(&project, "db").is_empty());
    assert!(holds(&machine, "gpu").is_empty());

    // Cancelled while waiting: both halves go, and the machine pool is not blocked by a lone half.
    let db = watch(cx, a.broker.acquire(ask("db-only", &[need("db")])));
    cx.run_until_parked();
    let db = lease(&db);
    let waiting = a.broker.acquire(ask("mixed", &both));
    cx.run_until_parked();
    let (project, machine) = read(&a);
    assert_eq!(waiters(&project, "db"), vec!["mixed"]);
    assert_eq!(waiters(&machine, "gpu"), vec!["mixed"]);
    assert!(machine.pools["gpu"].waiters[0].mixed);
    assert!(project.pools["db"].waiters[0].mixed);
    drop(waiting);
    cx.run_until_parked();
    let (project, machine) = read(&a);
    assert!(waiters(&project, "db").is_empty());
    assert!(waiters(&machine, "gpu").is_empty());
    let gpu = watch(cx, a.broker.acquire(ask("gpu-only", &[need("gpu")])));
    cx.run_until_parked();
    let _gpu = lease(&gpu);
    drop(db);
}

#[gpui::test]
fn a_mixed_grant_whose_machine_write_failed_is_completed_by_a_retry(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    let db = watch(cx, a.broker.acquire(ask("db-only", &[need("db")])));
    cx.run_until_parked();
    let db = lease(&db);
    let mixed = watch(
        cx,
        a.broker.acquire(ask("mixed", &[need("gpu"), need("db")])),
    );
    cx.run_until_parked();
    assert!(mixed.borrow().is_none());

    // The release commits in the project store, converting the project half; machine.toml is not
    // written. The retry converts the machine half before anything is released.
    a.machine.fail_next_write.store(true, Ordering::SeqCst);
    drop(db);
    cx.run_until_parked();
    let mixed = lease(&mixed);
    assert_eq!(mixed.hold_ids().len(), 2);
    let (project, machine) = read(&a);
    assert_eq!(holds(&project, "db"), vec!["mixed"]);
    assert_eq!(holds(&machine, "gpu"), vec!["mixed"]);
    drop(mixed);
    cx.run_until_parked();
    let (project, machine) = read(&a);
    assert!(holds(&project, "db").is_empty());
    assert!(holds(&machine, "gpu").is_empty());
}

#[gpui::test]
fn a_mixed_request_whose_first_machine_write_failed_is_queued_again(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    // The enqueue commits in the project store (granting the project half there) and fails to
    // write machine.toml; the retry writes it with no change. The request is queued again, not
    // refused.
    a.machine.fail_next_write.store(true, Ordering::SeqCst);
    let mixed = watch(
        cx,
        a.broker.acquire(ask("mixed", &[need("gpu"), need("db")])),
    );
    cx.run_until_parked();
    let mixed = lease(&mixed);
    assert_eq!(mixed.hold_ids().len(), 2);
    let (project, machine) = read(&a);
    assert_eq!(holds(&project, "db"), vec!["mixed"]);
    assert_eq!(holds(&machine, "gpu"), vec!["mixed"]);
    assert!(waiters(&project, "db").is_empty());
    assert!(waiters(&machine, "gpu").is_empty());
    drop(mixed);
    cx.run_until_parked();
    let (project, machine) = read(&a);
    assert!(holds(&project, "db").is_empty());
    assert!(holds(&machine, "gpu").is_empty());
}

#[gpui::test]
fn a_failing_guard_keeps_a_request_waiting_and_a_passing_one_admits_it(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![guarded(
            "ram",
            PoolKind::Guard,
            GuardCheck::FreeMemoryGb(12.0),
        )],
        vec![],
    );
    *a.readers.memory.lock().unwrap() = Some(9 * GIB);
    let waiting = a.broker.acquire(ask("job", &[need("ram")]));
    let status = waiting.status_source();
    let outcome = watch(cx, waiting);
    cx.run_until_parked();
    let text = status.status().blocked_text().unwrap();
    assert_eq!(text, "guard on ram failed (9.0 GB, needs 12 GB)");
    world.advance(cx, Duration::from_secs(4));
    assert!(
        outcome.borrow().is_none(),
        "no new reading before the interval"
    );

    *a.readers.memory.lock().unwrap() = Some(20 * GIB);
    world.advance(cx, Duration::from_secs(1));
    let lease = lease(&outcome);
    assert!(lease.hold_ids().is_empty(), "a guard pool records no hold");
}

#[gpui::test]
fn a_script_guard_that_cannot_run_fails_with_its_reason(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![guarded(
            "net",
            PoolKind::Guard,
            GuardCheck::Script("pools/net.sh".to_string()),
        )],
        vec![],
    );
    *a.readers.script.lock().unwrap() = Some(Err(
        "guard script pools/net.sh timed out after 30 s".to_string()
    ));
    let waiting = a.broker.acquire(ask("job", &[need("net")]));
    cx.run_until_parked();
    let status = waiting.status();
    assert!(matches!(
        status.blocked,
        Some(Blocked::Guard {
            reading: GuardValue::ScriptExit(-1),
            ..
        })
    ));
    assert_eq!(
        status.guard_note.as_deref(),
        Some("guard script pools/net.sh timed out after 30 s")
    );
}

#[gpui::test]
fn requests_are_refused_for_an_undeclared_pool_and_an_own_incompatible_hold(
    cx: &mut TestAppContext,
) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("jobs", PoolKind::Rw, 1)],
        vec![],
    );
    let undeclared = watch(cx, a.broker.acquire(ask("job", &[need("nope")])));
    cx.run_until_parked();
    assert_eq!(
        undeclared
            .borrow_mut()
            .take()
            .unwrap()
            .unwrap_err()
            .to_string(),
        "pool `nope` is not declared in pools/pools.toml"
    );

    let job = watch(cx, a.broker.acquire(ask("run-1", &[need("jobs")])));
    cx.run_until_parked();
    let _job = lease(&job);
    let exclusive = Need {
        mode: Some(NeedMode::Exclusive),
        ..need("jobs")
    };
    let step = Request {
        needs: vec![exclusive],
        holder: holder(
            "run-1",
            Level::Step {
                key: "merge".to_string(),
            },
        ),
    };
    let refused = watch(cx, a.broker.acquire(step));
    cx.run_until_parked();
    assert_eq!(
        refused
            .borrow_mut()
            .take()
            .unwrap()
            .unwrap_err()
            .to_string(),
        "holder run-1 holds `jobs` shared; it cannot also wait for it exclusive"
    );
    // A shared request inside the run is covered by the run's own hold.
    let shared = Request {
        needs: vec![need("jobs")],
        holder: holder(
            "run-1",
            Level::Step {
                key: "build".to_string(),
            },
        ),
    };
    let generation = {
        use crate::PoolStore as _;
        a.project_store.read().unwrap();
        a.project_store.generation()
    };
    let covered = watch(cx, a.broker.acquire(shared));
    cx.run_until_parked();
    let covered = lease(&covered);
    assert_eq!(covered.satisfied(), ["jobs".to_string()]);
    assert!(covered.hold_ids().is_empty());
    {
        use crate::PoolStore as _;
        a.project_store.read().unwrap();
        assert_eq!(
            a.project_store.generation(),
            generation,
            "granted without touching a store"
        );
    }
    drop(covered);
    cx.run_until_parked();
    assert_eq!(holds(&read(&a).0, "jobs"), vec!["run-1"]);
}

#[gpui::test]
fn a_drain_keeps_a_live_bound_hold_and_a_take_back_asks_again(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("cpu", PoolKind::Slots, 2)],
        vec![],
    );
    let bound = watch(cx, a.broker.acquire(ask("bound", &[need("cpu")])));
    let unbound = watch(cx, a.broker.acquire(ask("unbound", &[need("cpu")])));
    cx.run_until_parked();
    let bound = lease(&bound);
    let unbound = lease(&unbound);
    bound.bind_process(ProcessRecord {
        pid: 600,
        start_time: 1_600,
    });
    let waiting = watch(cx, a.broker.acquire(ask("waiting", &[need("cpu")])));
    cx.run_until_parked();
    assert_eq!(waiters(&read(&a).0, "cpu"), vec!["waiting"]);

    let drained = a.broker.drain();
    cx.run_until_parked();
    cx.foreground_executor().block_on(drained);
    let table = read(&a).0;
    assert_eq!(
        holds(&table, "cpu"),
        vec!["bound"],
        "a live bound hold stays"
    );
    assert!(waiters(&table, "cpu").is_empty());
    let mut given_up: Vec<(String, bool)> = a
        .broker
        .given_up()
        .into_iter()
        .map(|given| (given.holder.path.owner.0, given.held))
        .collect();
    given_up.sort();
    assert_eq!(
        given_up,
        vec![
            ("unbound".to_string(), true),
            ("waiting".to_string(), false)
        ]
    );
    let refused = watch(cx, a.broker.acquire(ask("new", &[need("cpu")])));
    cx.run_until_parked();
    assert_eq!(
        refused.borrow_mut().take().unwrap().unwrap_err(),
        Refused::Draining
    );
    assert!(waiting.borrow().is_none());

    // The host died during the handover: the writer stays and asks again under new tickets.
    a.broker.undrain();
    a.broker.started();
    cx.run_until_parked();
    assert!(a.broker.given_up().is_empty());
    let table = read(&a).0;
    let mut held = holds(&table, "cpu");
    held.sort();
    assert_eq!(held, vec!["bound", "unbound"]);
    assert_eq!(unbound.hold_ids().len(), 1, "the lease holds again");
    assert_eq!(waiters(&table, "cpu"), vec!["waiting"]);
    drop(bound);
    cx.run_until_parked();
    let waiting = lease(&waiting);
    drop((waiting, unbound));
    cx.run_until_parked();
    assert!(holds(&read(&a).0, "cpu").is_empty());
}

/// Takes the machine lock as another process would, until the file is dropped.
fn lock_machine(world: &World) -> std::fs::File {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(world.machine_dir.path().join(LOCK_FILE))
        .unwrap();
    // SAFETY: flock only takes a lock on this open file.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    lock
}

#[gpui::test]
fn a_drain_keeps_the_hold_of_a_live_lease_whose_bind_waits_for_a_retry(cx: &mut TestAppContext) {
    let world = World::new();
    let a = world.project(
        cx,
        "a",
        host(1),
        vec![],
        vec![pool("gpu", PoolKind::Slots, 1)],
    );
    let outcome = watch(cx, a.broker.acquire(ask("job", &[need("gpu")])));
    cx.run_until_parked();
    let job = lease(&outcome);
    // The bind cannot take the machine lock: it waits in the retry list for the next poll.
    let lock = lock_machine(&world);
    job.bind_process(ProcessRecord {
        pid: 600,
        start_time: 1_600,
    });
    cx.run_until_parked();
    drop(lock);
    assert!(read(&a).1.pools["gpu"].holds[0].holder.process.is_none());

    let drained = a.broker.drain();
    cx.run_until_parked();
    cx.foreground_executor().block_on(drained);
    let machine = read(&a).1;
    assert_eq!(holds(&machine, "gpu"), vec!["job"], "the live lease keeps its hold");
    assert_eq!(
        machine.pools["gpu"].holds[0].holder.process.map(|process| process.pid),
        Some(600),
        "bound to its process by the drain"
    );
    assert!(a.broker.given_up().is_empty());
    assert_eq!(job.hold_ids().len(), 1);
    drop(job);
    cx.run_until_parked();
    assert!(holds(&read(&a).1, "gpu").is_empty());
}

#[gpui::test]
fn a_dropped_broker_fails_its_waits_and_removes_their_rows(cx: &mut TestAppContext) {
    let world = World::new();
    let mut a = world.project(
        cx,
        "a",
        host(1),
        vec![pool("db", PoolKind::Mutex, 1)],
        vec![],
    );
    let first = watch(cx, a.broker.acquire(ask("first", &[need("db")])));
    cx.run_until_parked();
    let first = lease(&first);
    let second = watch(cx, a.broker.acquire(ask("second", &[need("db")])));
    cx.run_until_parked();
    assert_eq!(waiters(&read(&a).0, "db"), vec!["second"]);

    let stand_in = Rc::new(world.bare_broker(cx, &a));
    let broker = std::mem::replace(&mut a.broker, stand_in);
    drop(broker);
    cx.run_until_parked();
    assert_eq!(
        second.borrow_mut().take().unwrap().unwrap_err(),
        Refused::Closed
    );
    let table = read(&a).0;
    assert!(waiters(&table, "db").is_empty());
    assert_eq!(
        holds(&table, "db"),
        vec!["first"],
        "a live lease keeps its hold"
    );
    drop(first);
    cx.run_until_parked();
    assert!(
        holds(&read(&a).0, "db").is_empty(),
        "and releases it when dropped"
    );
}
