//! A fake pool and a fake clock, built only through the public seam, and the tests of the fake.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::executor::block_on;
use gpui::{RunnableMeta, Task};

use crate::pools::{
    CheckClock, CheckPools, Holder, PoolLease, PoolLeaseHandle, PoolRequest, PoolWait, WaitStatus,
};

/// A clock that only moves when told. An automatic one moves forward by each sleep and
/// returns at once; a manual one wakes a sleep when [`FakeClock::advance`] passes its end.
pub(crate) struct FakeClock {
    now: Cell<Instant>,
    automatic: bool,
    sleeps: RefCell<Vec<Duration>>,
    timers: RefCell<Vec<(Instant, oneshot::Sender<()>)>>,
}

impl FakeClock {
    fn new(automatic: bool) -> Rc<Self> {
        Rc::new(Self {
            // Only a starting point; nothing compares it with the calendar.
            now: Cell::new(Instant::now()),
            automatic,
            sleeps: RefCell::default(),
            timers: RefCell::default(),
        })
    }

    pub(crate) fn automatic() -> Rc<Self> {
        Self::new(true)
    }

    pub(crate) fn manual() -> Rc<Self> {
        Self::new(false)
    }

    /// Every duration asked of `sleep`, in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }

    pub(crate) fn advance(&self, by: Duration) {
        let now = self.now.get() + by;
        self.now.set(now);
        let due: Vec<oneshot::Sender<()>> = {
            let mut timers = self.timers.borrow_mut();
            let (due, waiting) = timers.drain(..).partition(|(end, _)| *end <= now);
            *timers = waiting;
            due.into_iter().map(|(_, sender)| sender).collect()
        };
        for sender in due {
            sender.send(()).ok();
        }
    }
}

impl CheckClock for FakeClock {
    fn now(&self) -> Instant {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        self.sleeps.borrow_mut().push(duration);
        if self.automatic {
            self.now.set(self.now.get() + duration);
            return Box::pin(std::future::ready(()));
        }
        let (sender, receiver) = oneshot::channel();
        self.timers
            .borrow_mut()
            .push((self.now.get() + duration, sender));
        Box::pin(async move {
            receiver.await.ok();
        })
    }
}

/// A gpui task over `future`, polled on the thread that wakes it, with no executor.
fn local_task<T: 'static>(future: impl Future<Output = T> + 'static) -> Task<T> {
    let (runnable, task) = async_task::Builder::new()
        .metadata(RunnableMeta::new_with_callers_location())
        .spawn_local(
            move |_| future,
            |runnable: async_task::Runnable<RunnableMeta>| {
                runnable.run();
            },
        );
    runnable.run();
    Task::from_async_task(task)
}

/// One pool of counted slots, first come first served.
#[derive(Clone)]
pub(crate) struct FakePool {
    name: String,
    clock: Rc<FakeClock>,
    state: Rc<RefCell<FakeState>>,
}

#[derive(Default)]
pub(crate) struct FakeState {
    slots: u32,
    next_id: u64,
    next_waiter: u64,
    held: BTreeMap<u64, (String, u32)>,
    queue: VecDeque<Waiter>,
    /// The holder names granted, in order.
    pub(crate) granted: Vec<String>,
    /// The lease ids released, in order.
    pub(crate) released: Vec<u64>,
    /// `(lease id, pid, start time)` for each `bind_process`.
    pub(crate) binds: Vec<(u64, u32, u64)>,
    pub(crate) granted_now: u32,
    pub(crate) refused_now: u32,
    /// The most slots held at once.
    pub(crate) max_used: u32,
    /// What happened, in order: "grant <holder>", "refused now <holder>", "served <name>".
    pub(crate) log: Vec<String>,
    /// After the first grant, another run's request for this many slots joins the line; it is
    /// served and done at once when its turn comes.
    pub(crate) queue_after_first_grant: Option<u32>,
}

struct Waiter {
    /// `None` for another run's request.
    id: Option<u64>,
    name: String,
    count: u32,
    /// `None` for another run's request that is done as soon as it is served.
    sender: Option<oneshot::Sender<u64>>,
}

impl Waiter {
    fn gone(&self) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.is_canceled())
    }
}

impl FakeState {
    fn used(&self) -> u32 {
        self.held.values().map(|(_, count)| count).sum()
    }

    fn free(&self) -> u32 {
        self.slots - self.used()
    }

    /// Drops waiters whose wait was dropped.
    fn prune(&mut self) {
        self.queue.retain(|waiter| !waiter.gone());
    }

    fn grant(&mut self, name: &str, count: u32) -> u64 {
        assert!(
            count <= self.free(),
            "{name} granted {count} of {} free",
            self.free()
        );
        self.next_id += 1;
        let id = self.next_id;
        self.held.insert(id, (name.to_string(), count));
        self.max_used = self.max_used.max(self.used());
        self.granted.push(name.to_string());
        self.log.push(format!("grant {name}"));
        if let Some(count) = self.queue_after_first_grant.take() {
            self.queue.push_back(Waiter {
                id: None,
                name: "other run".to_string(),
                count,
                sender: None,
            });
        }
        id
    }

    /// Grants the waiters at the front of the line while their slots are free.
    fn serve(&mut self) -> Vec<(oneshot::Sender<u64>, u64)> {
        let mut grants = Vec::new();
        loop {
            self.prune();
            let Some(front) = self.queue.front() else {
                break;
            };
            if front.count > self.free() {
                break;
            }
            let waiter = self.queue.pop_front().expect("a front waiter");
            match waiter.sender {
                Some(sender) => {
                    let id = self.grant(&waiter.name, waiter.count);
                    grants.push((sender, id));
                }
                None => self.log.push(format!("served {}", waiter.name)),
            }
        }
        grants
    }

    fn holders(&self) -> Vec<String> {
        self.held.values().map(|(name, _)| name.clone()).collect()
    }
}

fn release(state: &Rc<RefCell<FakeState>>, id: u64) {
    let grants = {
        let mut state = state.borrow_mut();
        assert!(
            state.held.remove(&id).is_some(),
            "lease {id} released twice"
        );
        state.released.push(id);
        state.serve()
    };
    // Sent with no borrow held: the wait's task runs as it is woken.
    for (sender, granted) in grants {
        if sender.send(granted).is_err() {
            release(state, granted);
        }
    }
}

struct FakeHandle {
    id: u64,
    state: Rc<RefCell<FakeState>>,
}

impl PoolLeaseHandle for FakeHandle {
    fn bind_process(&self, pid: u32, start_time: u64) {
        self.state
            .borrow_mut()
            .binds
            .push((self.id, pid, start_time));
    }
}

impl Drop for FakeHandle {
    fn drop(&mut self) {
        release(&self.state, self.id);
    }
}

pub(crate) fn holder_name(holder: &Holder) -> String {
    format!("{}/{} part {}", holder.task_id, holder.check, holder.part)
}

impl FakePool {
    pub(crate) fn new(name: &str, slots: u32, clock: Rc<FakeClock>) -> Self {
        Self {
            name: name.to_string(),
            clock,
            state: Rc::new(RefCell::new(FakeState {
                slots,
                ..FakeState::default()
            })),
        }
    }

    pub(crate) fn state(&self) -> std::cell::RefMut<'_, FakeState> {
        self.state.borrow_mut()
    }

    fn lease(&self, id: u64) -> PoolLease {
        PoolLease::new(Box::new(FakeHandle {
            id,
            state: self.state.clone(),
        }))
    }

    /// Takes `count` slots for `name` outside any check; they must be free.
    pub(crate) fn hold(&self, name: &str, count: u32) -> PoolLease {
        let id = self.state.borrow_mut().grant(name, count);
        self.lease(id)
    }

    /// The live waiters in line.
    pub(crate) fn queued(&self) -> usize {
        let mut state = self.state.borrow_mut();
        state.prune();
        state.queue.len()
    }

    pub(crate) fn holders(&self) -> Vec<String> {
        self.state.borrow().holders()
    }

    /// Every lease granted was released, each exactly once, and none is held.
    pub(crate) fn assert_all_released(&self) {
        let state = self.state.borrow();
        assert!(state.held.is_empty(), "still held: {:?}", state.holders());
        let mut released = state.released.clone();
        released.sort_unstable();
        let expected: Vec<u64> = (1..=state.next_id).collect();
        assert_eq!(released, expected, "each lease is released once");
    }

    /// The pool and slot count a request asks for.
    fn parse(&self, request: &PoolRequest) -> Result<u32, String> {
        let (pool, count) = match request.needs.first() {
            Some(toml::Value::String(pool)) => (pool.clone(), 1),
            Some(toml::Value::Table(table)) => (
                table
                    .get("pool")
                    .and_then(|pool| pool.as_str())
                    .unwrap_or_default()
                    .to_string(),
                table
                    .get("count")
                    .and_then(|count| count.as_integer())
                    .map_or(1, |count| count as u32),
            ),
            _ => return Err("no pool named".to_string()),
        };
        if pool != self.name {
            return Err(format!("unknown pool {pool}"));
        }
        Ok(count)
    }

    fn status_of(&self, waiter: Option<u64>) -> Box<dyn Fn() -> WaitStatus> {
        let state = self.state.clone();
        let pool = self.name.clone();
        let since = self.clock.now();
        Box::new(move || {
            let mut state = state.borrow_mut();
            state.prune();
            let place = waiter.and_then(|waiter| {
                state
                    .queue
                    .iter()
                    .position(|queued| queued.id == Some(waiter))
            });
            let blocked = place.map(|place| {
                if place > 0 {
                    format!("{place} waiting ahead")
                } else {
                    format!("all {} slots held", state.slots)
                }
            });
            WaitStatus {
                pool: pool.clone(),
                position: place.map_or(0, |place| place as u32 + 1),
                since,
                holders: state.holders(),
                blocked,
            }
        })
    }
}

impl CheckPools for FakePool {
    fn acquire(&self, request: PoolRequest) -> PoolWait {
        let count = match self.parse(&request) {
            Ok(count) => count,
            Err(why) => return PoolWait::new(self.status_of(None), Task::ready(Err(why))),
        };
        let name = holder_name(&request.holder);
        let mut state = self.state.borrow_mut();
        state.prune();
        if state.queue.is_empty() && count <= state.free() {
            let id = state.grant(&name, count);
            drop(state);
            return PoolWait::new(self.status_of(None), Task::ready(Ok(self.lease(id))));
        }
        state.next_waiter += 1;
        let waiter = state.next_waiter;
        let (sender, receiver) = oneshot::channel();
        state.queue.push_back(Waiter {
            id: Some(waiter),
            name,
            count,
            sender: Some(sender),
        });
        drop(state);
        let pool = self.clone();
        let ready = local_task(async move {
            let id = receiver
                .await
                .map_err(|_| "the pool went away".to_string())?;
            Ok(pool.lease(id))
        });
        PoolWait::new(self.status_of(Some(waiter)), ready)
    }

    fn try_acquire_now(&self, request: PoolRequest) -> Option<PoolLease> {
        let count = self.parse(&request).ok()?;
        let name = holder_name(&request.holder);
        let mut state = self.state.borrow_mut();
        state.prune();
        if !state.queue.is_empty() || count > state.free() {
            state.refused_now += 1;
            state.log.push(format!("refused now {name}"));
            return None;
        }
        state.granted_now += 1;
        let id = state.grant(&name, count);
        drop(state);
        Some(self.lease(id))
    }
}

pub(crate) fn request(task: &str, part: u32, needs: toml::Value) -> PoolRequest {
    PoolRequest {
        project_id: "project-1".to_string(),
        needs: vec![needs],
        holder: Holder {
            task_id: task.to_string(),
            check: "tests".to_string(),
            part,
            run: None,
            host_pid: std::process::id(),
        },
    }
}

fn slots(count: i64) -> toml::Value {
    let mut table = toml::Table::new();
    table.insert("pool".to_string(), toml::Value::String("test-slots".into()));
    table.insert("count".to_string(), toml::Value::Integer(count));
    toml::Value::Table(table)
}

#[test]
fn fake_pool_lease_records_bind_process_and_releases_its_slot_once() {
    let pool = FakePool::new("test-slots", 1, FakeClock::manual());
    let mut wait = pool.acquire(request("a", 1, "test-slots".into()));
    assert!(wait.ready().is_ready());
    let lease = block_on(wait.ready()).unwrap();
    lease.bind_process(4242, 99);
    assert_eq!(pool.state().binds, [(1, 4242, 99)]);
    assert_eq!(pool.holders(), ["a/tests part 1"]);
    drop(wait);
    assert_eq!(
        pool.holders(),
        ["a/tests part 1"],
        "the lease outlives its wait"
    );
    drop(lease);
    assert!(pool.holders().is_empty());
    pool.assert_all_released();
}

#[test]
fn fake_pool_status_reports_place_holders_and_why_it_is_blocked() {
    let clock = FakeClock::manual();
    let pool = FakePool::new("test-slots", 1, clock.clone());
    let started = clock.now();
    let held = pool.hold("other run", 1);
    let mut first = pool.acquire(request("a", 1, "test-slots".into()));
    clock.advance(Duration::from_secs(60));
    let second = pool.acquire(request("b", 1, "test-slots".into()));
    assert!(!first.ready().is_ready());
    assert_eq!(
        first.status(),
        WaitStatus {
            pool: "test-slots".to_string(),
            position: 1,
            since: started,
            holders: vec!["other run".to_string()],
            blocked: Some("all 1 slots held".to_string()),
        }
    );
    let status = second.status();
    assert_eq!(status.position, 2);
    assert_eq!(status.since, started + Duration::from_secs(60));
    assert_eq!(status.blocked.as_deref(), Some("1 waiting ahead"));

    // The slot goes to the first in line; dropping it hands it to the second.
    drop(held);
    assert!(first.ready().is_ready());
    let lease = block_on(first.ready()).unwrap();
    assert_eq!(pool.holders(), ["a/tests part 1"]);
    assert_eq!(second.status().position, 1);
    drop(second);
    assert_eq!(pool.queued(), 0, "a dropped wait leaves the line");
    drop(lease);
    drop(first);
    assert!(pool.holders().is_empty());
}

#[test]
fn fake_pool_refuses_try_acquire_now_while_a_waiter_is_queued() {
    let pool = FakePool::new("test-slots", 3, FakeClock::manual());
    let held = pool.hold("other run", 2);
    // One slot is free, but a request for two waits in line.
    let mut waiting = pool.acquire(request("a", 1, slots(2)));
    assert!(!waiting.ready().is_ready());
    assert!(
        pool.try_acquire_now(request("b", 1, "test-slots".into()))
            .is_none()
    );
    assert_eq!(pool.state().refused_now, 1);

    // With the line empty, a free slot is granted now; with none free, it is refused.
    drop(held);
    let lease = block_on(waiting.ready()).unwrap();
    let now = pool
        .try_acquire_now(request("b", 1, "test-slots".into()))
        .expect("a slot is free and nobody waits");
    assert!(
        pool.try_acquire_now(request("c", 1, "test-slots".into()))
            .is_none()
    );
    drop((lease, now, waiting));
    assert!(pool.holders().is_empty());
}
