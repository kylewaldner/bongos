//! Workflow nodes that wait for their pools (51 D9): feature 20's [`NodeAdmission`] served by the
//! pool brokers, registered with the engine once `pools.enabled` is on.
//!
//! - A node's `needs` is asked of the broker of its run's project as one request of the holder
//!   `Step { key }` of owner `<run id>`. The first start of the key answers `Wait`; the broker's
//!   grant marks it granted and wakes the engine, whose next start of the key goes ahead
//!   (`Go`), so nothing of the try (its process, its time cap) begins before the grant. A refusal
//!   (undeclared pool, kind clash, deadlock) answers `Refused` and the try fails `pool request
//!   refused`. A node with no `needs` always goes.
//! - The runtime is locked while it asks, so [`PoolsNodeAdmission`] only reads and writes the
//!   slots it shares with [`PoolsNodes`] and sends it a message; [`PoolsNodes`] owns the waits and
//!   the leases on the foreground and handles the messages in order.
//! - Holds: the lease is bound to the node's process when it starts (a built-in, pid 0, keeps its
//!   holds with the host) and released at the try's end, its stop, or the run's wait for a person.
//!   The holds of a `hold_while_waiting` pool are kept apart from the process; when the node
//!   finishes they become the run's (`Level::Run`), which satisfy later nodes of the run asking
//!   for that pool, and go when the run ends.
//! - Waiters: a key's wait is dropped (its rows leave every pool's line) when it is no longer
//!   offered, when its run waits for a person and when its run ends.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bongos_pools_model::{
    Holder, HolderLabel, HolderPath, HostRef, Level, Need, OwnerKey, parse_need,
};
use bongos_workflow::process::ProcessRecord;
use bongos_workflow::{
    Admission, NodeAdmission, NodeNeeds, NodeStatus, NodeStatusSource, NodeWait,
};
use bongos_workflow_model::definition::Node;
use bongos_workflow_model::{Definition, Diagnostic, NodeKey, Severity};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, ForegroundExecutor, Task};

use crate::broker::{Lease, PoolBroker, Refused, Request, StatusSource, WaitStatus51};
use crate::checks::PoolsForChecks;

/// The needs of `node`: each entry of its `needs`, an array of entries or one entry.
pub fn node_needs(node: &Node) -> Result<Vec<Need>, String> {
    let Some(needs) = &node.needs else {
        return Ok(Vec::new());
    };
    match &needs.value {
        toml::Value::Array(entries) => entries.iter().map(parse_need).collect(),
        entry => parse_need(entry).map(|need| vec![need]),
    }
}

/// The validator of a node's `needs` ([`NodeNeeds::set`]): while `pools.enabled` is off it is
/// not available; otherwise each entry must parse. Whether each pool is declared is the
/// `pool-references` check's.
pub fn validate_node_needs(node: &Node) -> Vec<Diagnostic> {
    if !crate::enabled() {
        return NodeNeeds::not_available(node).into_iter().collect();
    }
    let Some(needs) = &node.needs else {
        return Vec::new();
    };
    match node_needs(node) {
        Ok(_) => Vec::new(),
        Err(message) => vec![Diagnostic {
            node: Some(node.name.clone()),
            line: needs.line,
            severity: Severity::Error,
            message,
        }],
    }
}

/// Where the request of one key stands, as the runtime sees it.
#[derive(Debug, Clone, PartialEq)]
enum SlotState {
    /// Asked of the broker, not granted yet.
    Asked,
    /// Granted: the next start goes.
    Granted,
    /// Never granted, with why.
    Refused(String),
    /// The key's try runs.
    Started,
}

struct Slot {
    id: u64,
    state: SlotState,
    wait: NodeWait,
}

#[derive(Default)]
struct Slots {
    next: u64,
    by_key: BTreeMap<(String, NodeKey), Slot>,
}

impl Slots {
    /// Sets the state of the slot of request `id`, when it is still that request's.
    fn set(&mut self, id: u64, state: SlotState) {
        if let Some(slot) = self.by_key.values_mut().find(|slot| slot.id == id) {
            slot.state = state;
        }
    }

    /// Drops the slot of request `id`, when it is still that request's.
    fn remove(&mut self, id: u64) {
        self.by_key.retain(|_, slot| slot.id != id);
    }
}

enum Message {
    Ask {
        id: u64,
        run: String,
        key: NodeKey,
        needs: Vec<Need>,
    },
    Started {
        id: u64,
        process: ProcessRecord,
    },
    Finished {
        id: u64,
        ran: bool,
    },
    Drop {
        ids: Vec<u64>,
    },
    RunEnded {
        run: String,
    },
}

/// Feature 20's admission over the pools, handed to the engine's runtime.
pub struct PoolsNodeAdmission {
    slots: Arc<Mutex<Slots>>,
    messages: mpsc::UnboundedSender<Message>,
}

impl PoolsNodeAdmission {
    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, message: Message) {
        // The worker is gone only when its app is: nothing is held for anyone then.
        self.messages.unbounded_send(message).ok();
    }
}

impl NodeAdmission for PoolsNodeAdmission {
    fn admit(&mut self, run: &str, definition: &Definition, key: &NodeKey) -> Admission {
        let Some(node) = definition.node(&key.node) else {
            return Admission::Go;
        };
        let needs = match node_needs(node) {
            Ok(needs) => needs,
            Err(message) => return Admission::Refused(message),
        };
        if needs.is_empty() {
            return Admission::Go;
        }
        let mut slots = self.slots();
        if let Some(slot) = slots.by_key.get(&(run.to_string(), key.clone())) {
            return match &slot.state {
                SlotState::Asked => Admission::Wait(slot.wait.clone()),
                SlotState::Granted | SlotState::Started => Admission::Go,
                SlotState::Refused(text) => Admission::Refused(text.clone()),
            };
        }
        if self.messages.is_closed() {
            return Admission::Refused(Refused::Closed.to_string());
        }
        slots.next += 1;
        let id = slots.next;
        let wait = NodeWait {
            text: format!("waiting for pool {}", needs[0].pool),
        };
        slots.by_key.insert(
            (run.to_string(), key.clone()),
            Slot {
                id,
                state: SlotState::Asked,
                wait: wait.clone(),
            },
        );
        drop(slots);
        self.send(Message::Ask {
            id,
            run: run.to_string(),
            key: key.clone(),
            needs,
        });
        Admission::Wait(wait)
    }

    fn started(&mut self, run: &str, key: &NodeKey, process: ProcessRecord) {
        let id = {
            let mut slots = self.slots();
            let Some(slot) = slots.by_key.get_mut(&(run.to_string(), key.clone())) else {
                return;
            };
            slot.state = SlotState::Started;
            slot.id
        };
        self.send(Message::Started { id, process });
    }

    fn finished(&mut self, run: &str, key: &NodeKey) {
        let Some(slot) = self.slots().by_key.remove(&(run.to_string(), key.clone())) else {
            return;
        };
        self.send(Message::Finished {
            id: slot.id,
            ran: slot.state == SlotState::Started,
        });
    }

    fn run_waiting(&mut self, run: &str) {
        let mut ids = Vec::new();
        self.slots().by_key.retain(|(slot_run, _), slot| {
            let drop = slot_run == run && slot.state != SlotState::Started;
            if drop {
                ids.push(slot.id);
            }
            !drop
        });
        if !ids.is_empty() {
            self.send(Message::Drop { ids });
        }
    }

    fn run_ended(&mut self, run: &str) {
        let mut ids = Vec::new();
        self.slots().by_key.retain(|(slot_run, _), slot| {
            if slot_run == run {
                ids.push(slot.id);
            }
            slot_run != run
        });
        if !ids.is_empty() {
            self.send(Message::Drop { ids });
        }
        self.send(Message::RunEnded {
            run: run.to_string(),
        });
    }
}

/// One key's request on the foreground.
struct Entry {
    run: String,
    key: NodeKey,
    broker: Rc<PoolBroker>,
    /// Where the wait stands, until it is granted.
    status: Option<StatusSource>,
    /// Awaits the grant; dropping it drops the wait.
    waiting: Option<Task<()>>,
    /// The granted holds the node's process holds.
    lease: Option<Lease>,
    /// The granted holds of `hold_while_waiting` pools, which go to the run at the finish.
    keep: Option<Lease>,
}

/// Finds the project of a run: the id of the broker its nodes ask.
pub type ProjectOf = Box<dyn Fn(&str) -> Option<String>>;

struct Worker {
    brokers: Rc<PoolsForChecks>,
    project_of: ProjectOf,
    wake: Option<Rc<dyn Fn()>>,
    slots: Arc<Mutex<Slots>>,
    foreground: ForegroundExecutor,
    entries: BTreeMap<u64, Entry>,
    /// The holds each run keeps until it ends (`hold_while_waiting`).
    run_holds: BTreeMap<String, Vec<Lease>>,
}

/// The waits and leases of the workflow nodes, on the foreground; also the status source of the
/// keys waiting for their pools.
pub struct PoolsNodes {
    worker: Rc<RefCell<Worker>>,
    _messages: Task<()>,
}

impl PoolsNodes {
    /// The nodes' side over `brokers` (the brokers of the projects this process writes), finding
    /// a run's project with `project_of` (the only broker when it answers none), and the admission
    /// to hand to the runtime.
    pub fn new(
        brokers: Rc<PoolsForChecks>,
        project_of: ProjectOf,
        foreground: ForegroundExecutor,
    ) -> (Rc<PoolsNodes>, PoolsNodeAdmission) {
        let slots = Arc::new(Mutex::new(Slots::default()));
        let (sender, mut receiver) = mpsc::unbounded();
        let worker = Rc::new(RefCell::new(Worker {
            brokers,
            project_of,
            wake: None,
            slots: slots.clone(),
            foreground: foreground.clone(),
            entries: BTreeMap::new(),
            run_holds: BTreeMap::new(),
        }));
        let weak = Rc::downgrade(&worker);
        let messages = foreground.spawn(async move {
            while let Some(message) = receiver.next().await {
                let Some(worker) = weak.upgrade() else {
                    return;
                };
                Worker::handle(&worker, message);
            }
        });
        let nodes = Rc::new(PoolsNodes {
            worker,
            _messages: messages,
        });
        let admission = PoolsNodeAdmission {
            slots,
            messages: sender,
        };
        (nodes, admission)
    }

    /// Calls `wake` after a grant or a refusal, so the engine starts or fails the key now.
    pub fn set_wake(&self, wake: impl Fn() + 'static) {
        self.worker.borrow_mut().wake = Some(Rc::new(wake));
    }

    /// Where the wait of `key` of run `run` stands, while it waits for its pools: what the run
    /// view shows and a stall watcher asks.
    pub fn waiting_node(&self, run: &str, key: &NodeKey) -> Option<WaitStatus51> {
        let worker = self.worker.borrow();
        let entry = worker
            .entries
            .values()
            .find(|entry| entry.run == run && &entry.key == key)?;
        entry.status.as_ref().map(StatusSource::status)
    }

    /// The pools run `run` keeps for itself (`hold_while_waiting`), by name.
    pub fn run_pools(&self, run: &str) -> Vec<String> {
        let worker = self.worker.borrow();
        let mut pools: Vec<String> = worker
            .run_holds
            .get(run)
            .into_iter()
            .flatten()
            .flat_map(|lease| lease.hold_ids().into_iter().map(|hold| hold.pool))
            .collect();
        pools.sort();
        pools.dedup();
        pools
    }

    /// The pools the try of `key` of run `run` holds now, by name.
    pub fn node_pools(&self, run: &str, key: &NodeKey) -> Vec<String> {
        let worker = self.worker.borrow();
        let mut pools: Vec<String> = worker
            .entries
            .values()
            .filter(|entry| entry.run == run && &entry.key == key)
            .flat_map(|entry| entry.lease.iter().chain(entry.keep.iter()))
            .flat_map(|lease| lease.hold_ids().into_iter().map(|hold| hold.pool))
            .collect();
        pools.sort();
        pools.dedup();
        pools
    }

    /// The line the run view shows under a waiting key.
    fn line(&self, run: &str, key: &NodeKey) -> Option<String> {
        let status = self.waiting_node(run, key)?;
        let now = {
            let worker = self.worker.borrow();
            let entry = worker
                .entries
                .values()
                .find(|entry| entry.run == run && &entry.key == key)?;
            entry.broker.now()
        };
        Some(wait_line(&status, now.0))
    }
}

/// "waiting: gpu, place 2, held by run 42, 4m".
pub fn wait_line(status: &WaitStatus51, now_ms: u64) -> String {
    let mut parts = vec![format!("waiting: {}", status.pool)];
    if status.position > 0 {
        parts.push(format!("place {}", status.position));
    }
    if !status.holders.is_empty() {
        let holders: Vec<&str> = status
            .holders
            .iter()
            .map(|holder| holder.0.as_str())
            .collect();
        parts.push(format!("held by {}", holders.join(", ")));
    }
    let waited = Duration::from_millis(now_ms.saturating_sub(status.since.0));
    parts.push(if waited.as_secs() >= 60 {
        format!("{}m", waited.as_secs() / 60)
    } else {
        format!("{}s", waited.as_secs())
    });
    parts.join(", ")
}

impl NodeStatusSource for PoolsNodes {
    fn status(&self, run_id: &str, key: &NodeKey, _cx: &App) -> Option<NodeStatus> {
        let status = self.waiting_node(run_id, key)?;
        Some(NodeStatus {
            line: self.line(run_id, key)?,
            detail: status.blocked_text(),
        })
    }
}

impl Worker {
    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The broker of run `run`'s project, or the only one open.
    fn broker_of(&self, run: &str) -> Option<Rc<PoolBroker>> {
        if let Some(project) = (self.project_of)(run) {
            return self.brokers.broker(&project);
        }
        self.brokers.only()
    }

    fn handle(this: &Rc<RefCell<Worker>>, message: Message) {
        match message {
            Message::Ask {
                id,
                run,
                key,
                needs,
            } => Self::ask(this, id, run, key, needs),
            Message::Started { id, process } => {
                let worker = this.borrow();
                // A built-in has no process of its own: its holds stay with the host.
                if process.pid != 0
                    && let Some(lease) = worker
                        .entries
                        .get(&id)
                        .and_then(|entry| entry.lease.as_ref())
                {
                    lease.bind_process(process);
                }
            }
            Message::Finished { id, ran } => {
                let mut worker = this.borrow_mut();
                let Some(entry) = worker.entries.remove(&id) else {
                    return;
                };
                let Entry {
                    run, keep, lease, ..
                } = entry;
                drop(lease);
                if let Some(keep) = keep
                    && ran
                {
                    let label = HolderLabel(format!("run {run}"));
                    let held = keep.into_level(Level::Run, label);
                    worker.run_holds.entry(run).or_default().push(held);
                }
            }
            Message::Drop { ids } => {
                let dropped: Vec<Entry> = {
                    let mut worker = this.borrow_mut();
                    ids.iter()
                        .filter_map(|id| worker.entries.remove(id))
                        .collect()
                };
                drop(dropped);
            }
            Message::RunEnded { run } => {
                let held = this.borrow_mut().run_holds.remove(&run);
                drop(held);
            }
        }
    }

    fn ask(this: &Rc<RefCell<Worker>>, id: u64, run: String, key: NodeKey, needs: Vec<Need>) {
        let mut worker = this.borrow_mut();
        let Some(broker) = worker.broker_of(&run) else {
            worker.slots().set(
                id,
                SlotState::Refused(format!(
                    "the pools of run {run}'s project are not open in this process"
                )),
            );
            let wake = worker.wake.clone();
            drop(worker);
            if let Some(wake) = wake {
                wake();
            }
            return;
        };
        let holder = Holder {
            // The broker sets its own project and host.
            project: String::new(),
            path: HolderPath {
                owner: OwnerKey(run.clone()),
                level: Level::Step {
                    key: key.to_string(),
                },
            },
            label: HolderLabel(format!("node {key} of run {run}")),
            host: HostRef { pid: 0, start: 0 },
            process: None,
        };
        let waiting = broker.acquire(Request { needs, holder });
        let status = waiting.status_source();
        let weak = Rc::downgrade(this);
        let task = worker.foreground.spawn(async move {
            let answer = waiting.ready().await;
            if let Some(worker) = weak.upgrade() {
                Worker::resolved(&worker, id, answer);
            }
        });
        worker.entries.insert(
            id,
            Entry {
                run,
                key,
                broker,
                status: Some(status),
                waiting: Some(task),
                lease: None,
                keep: None,
            },
        );
    }

    fn resolved(this: &Rc<RefCell<Worker>>, id: u64, answer: Result<Lease, Refused>) {
        let wake = {
            let mut guard = this.borrow_mut();
            let worker = &mut *guard;
            let Some(entry) = worker.entries.get_mut(&id) else {
                // Dropped meanwhile: a lease is released as it drops here.
                return;
            };
            if let Some(task) = entry.waiting.take() {
                task.detach();
            }
            entry.status = None;
            let mut slots = worker.slots.lock().unwrap_or_else(PoisonError::into_inner);
            match answer {
                Ok(lease) => {
                    let keep: BTreeSet<String> = lease
                        .hold_ids()
                        .into_iter()
                        .map(|hold| hold.pool)
                        .filter(|pool| entry.broker.holds_while_waiting(pool))
                        .collect();
                    entry.keep = lease.split_off(&keep);
                    entry.lease = Some(lease);
                    slots.set(id, SlotState::Granted);
                }
                // The writer moves or closes: asked again at the key's next start.
                Err(Refused::Draining | Refused::Closed) => {
                    worker.entries.remove(&id);
                    slots.remove(id);
                }
                Err(refused) => {
                    worker.entries.remove(&id);
                    slots.set(id, SlotState::Refused(refused.to_string()));
                }
            }
            drop(slots);
            worker.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }
}
