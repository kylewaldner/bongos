//! Feature 47's seam: the pools a check's parts wait for, served by the broker of the part's
//! project ([`PoolsForChecks`], registered with `CheckPoolsRegistry::set` when `pools.enabled`).
//!
//! - `acquire` parses the part's `needs`; an unknown pool, a bad mode or a bad count resolves
//!   `ready()` to "pool `<name>` is not declared in pools/pools.toml", which 47 records as no
//!   verdict. Otherwise the broker queues it for the holder `Part { key, part }` of the run (or
//!   `check/<task>/<check>` when no workflow run started the check).
//! - `try_acquire_now` is the broker's `try_now`: never while any waiter is queued on a named
//!   pool, so a parallel part never jumps a queued waiter.
//! - The lease's `bind_process` binds the hold to the part's process; dropping it releases.
//! - `PoolWait::status` maps the broker's [`WaitStatus51`] onto 47's `WaitStatus`, with the
//!   holders and the blocked reason.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bongos_checks::{
    CheckPools, Holder as CheckHolder, PoolLease, PoolLeaseHandle, PoolRequest, PoolWait,
    WaitStatus,
};
use bongos_pools_model::{
    Holder, HolderLabel, HolderPath, HostRef, Level, Need, OwnerKey, parse_need,
};
use bongos_workflow::process::ProcessRecord;
use gpui::{BackgroundExecutor, Task};

use crate::StoreContext;
use crate::broker::{Lease, PoolBroker, Refused, Request, StatusSource, WaitStatus51};

/// The brokers of the projects this process holds the writer of, as 47's `CheckPools`.
#[derive(Default)]
pub struct PoolsForChecks {
    brokers: RefCell<BTreeMap<String, Rc<PoolBroker>>>,
    /// The goal of a task by its id, for the holder labels; none names the task by id.
    goals: RefCell<Option<Box<dyn Fn(&str) -> Option<String>>>>,
    /// Called with every broker inserted.
    inserted: RefCell<Vec<Rc<dyn Fn(&Rc<PoolBroker>)>>>,
}

impl PoolsForChecks {
    pub fn new() -> Rc<PoolsForChecks> {
        Rc::new(PoolsForChecks::default())
    }

    /// Serves `broker`'s project from now on, in place of any broker it had.
    pub fn insert(&self, broker: Rc<PoolBroker>) {
        let id = broker.project().id.clone();
        self.brokers.borrow_mut().insert(id, broker.clone());
        let listeners: Vec<_> = self.inserted.borrow().clone();
        for listener in listeners {
            listener(&broker);
        }
    }

    /// Calls `listener` with each broker open now and every one inserted from now on.
    pub fn on_insert(&self, listener: impl Fn(&Rc<PoolBroker>) + 'static) {
        let brokers: Vec<_> = self.brokers.borrow().values().cloned().collect();
        for broker in &brokers {
            listener(broker);
        }
        self.inserted.borrow_mut().push(Rc::new(listener));
    }

    pub fn remove(&self, project: &str) -> Option<Rc<PoolBroker>> {
        self.brokers.borrow_mut().remove(project)
    }

    pub fn broker(&self, project: &str) -> Option<Rc<PoolBroker>> {
        self.brokers.borrow().get(project).cloned()
    }

    /// The broker, when exactly one project's is open.
    pub fn only(&self) -> Option<Rc<PoolBroker>> {
        let brokers = self.brokers.borrow();
        if brokers.len() == 1 {
            brokers.values().next().cloned()
        } else {
            None
        }
    }

    /// How a task's goal is found for the labels ("check build of Invoice export, part 2").
    pub fn set_task_goals(&self, goals: impl Fn(&str) -> Option<String> + 'static) {
        *self.goals.borrow_mut() = Some(Box::new(goals));
    }

    fn holder(&self, holder: &CheckHolder) -> Holder {
        let goal = self
            .goals
            .borrow()
            .as_ref()
            .and_then(|goals| goals(&holder.task_id))
            .unwrap_or_else(|| holder.task_id.clone());
        let (owner, key) = match &holder.run {
            Some((run, key)) => (run.clone(), key.to_string()),
            None => (
                format!("check/{}/{}", holder.task_id, holder.check),
                String::new(),
            ),
        };
        Holder {
            // The broker sets its own project and host.
            project: String::new(),
            path: HolderPath {
                owner: OwnerKey(owner),
                level: Level::Part {
                    key,
                    part: holder.part.to_string(),
                },
            },
            label: HolderLabel(format!(
                "check {} of {}, part {}",
                holder.check, goal, holder.part
            )),
            host: HostRef {
                pid: holder.host_pid,
                start: 0,
            },
            process: None,
        }
    }
}

/// The text 47 records as no verdict for a pool it cannot ask for.
pub fn undeclared(pool: &str) -> String {
    format!("pool `{pool}` is not declared in pools/pools.toml")
}

/// The needs of a part; an entry that does not parse names its pool (or itself) as undeclared.
fn parse_needs(needs: &[toml::Value]) -> Result<Vec<Need>, String> {
    needs
        .iter()
        .map(|value| {
            parse_need(value).map_err(|_| {
                let name = match value {
                    toml::Value::String(name) => name.clone(),
                    toml::Value::Table(table) => table
                        .get("pool")
                        .and_then(toml::Value::as_str)
                        .map_or_else(|| value.to_string(), str::to_string),
                    other => other.to_string(),
                };
                undeclared(&name)
            })
        })
        .collect()
}

/// A refusal as 47 records it.
fn refusal_text(refused: &Refused) -> String {
    match refused {
        Refused::Undeclared { pool } | Refused::BadNeed { pool, .. } => undeclared(pool),
        other => other.to_string(),
    }
}

/// The broker's status as 47's, `since` on the executor's clock.
fn check_status(
    status: WaitStatus51,
    context: &StoreContext,
    executor: &BackgroundExecutor,
) -> WaitStatus {
    let now = executor.now();
    let waited = Duration::from_millis(context.now().0.saturating_sub(status.since.0));
    WaitStatus {
        blocked: status.blocked_text(),
        pool: status.pool,
        position: status.position,
        since: now.checked_sub(waited).unwrap_or(now),
        holders: status.holders.into_iter().map(|label| label.0).collect(),
    }
}

fn status_fn(
    source: Option<StatusSource>,
    pool: String,
    context: Option<StoreContext>,
    executor: Option<BackgroundExecutor>,
) -> Box<dyn Fn() -> WaitStatus> {
    let start = std::time::Instant::now();
    Box::new(move || match (&source, &context, &executor) {
        (Some(source), Some(context), Some(executor)) => {
            check_status(source.status(), context, executor)
        }
        _ => WaitStatus {
            pool: pool.clone(),
            position: 0,
            since: executor.as_ref().map_or(start, BackgroundExecutor::now),
            holders: Vec::new(),
            blocked: None,
        },
    })
}

/// 47's side of a broker lease.
struct CheckLease(Lease);

impl PoolLeaseHandle for CheckLease {
    fn bind_process(&self, pid: u32, start_time: u64) {
        self.0.bind_process(ProcessRecord { pid, start_time });
    }
}

impl CheckPools for PoolsForChecks {
    fn acquire(&self, request: PoolRequest) -> PoolWait {
        let first = request
            .needs
            .first()
            .map(|need| match need {
                toml::Value::String(name) => name.clone(),
                other => other
                    .get("pool")
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .unwrap_or_default();
        let refuse = |text: String| {
            PoolWait::new(
                status_fn(None, first.clone(), None, None),
                Task::ready(Err(text)),
            )
        };
        let needs = match parse_needs(&request.needs) {
            Ok(needs) => needs,
            Err(text) => return refuse(text),
        };
        let Some(broker) = self.broker(&request.project_id) else {
            if needs.is_empty() {
                // Nothing to wait for: 47 runs the part at once.
                return PoolWait::new(
                    status_fn(None, first.clone(), None, None),
                    Task::ready(Ok(PoolLease::new(Box::new(NoLease)))),
                );
            }
            return refuse(format!(
                "the pools of project {} are not open in this process",
                request.project_id
            ));
        };
        let waiting = broker.acquire(Request {
            needs,
            holder: self.holder(&request.holder),
        });
        let status = status_fn(
            Some(waiting.status_source()),
            first,
            Some(broker.context()),
            Some(broker.background().clone()),
        );
        let ready = broker.foreground().spawn(async move {
            waiting
                .ready()
                .await
                .map(|lease| PoolLease::new(Box::new(CheckLease(lease))))
                .map_err(|refused| refusal_text(&refused))
        });
        PoolWait::new(status, ready)
    }

    fn try_acquire_now(&self, request: PoolRequest) -> Option<PoolLease> {
        let needs = parse_needs(&request.needs).ok()?;
        let Some(broker) = self.broker(&request.project_id) else {
            return needs.is_empty().then(|| PoolLease::new(Box::new(NoLease)));
        };
        broker
            .try_now(Request {
                needs,
                holder: self.holder(&request.holder),
            })
            .map(|lease| PoolLease::new(Box::new(CheckLease(lease))))
    }
}

/// A lease of nothing.
struct NoLease;

impl PoolLeaseHandle for NoLease {
    fn bind_process(&self, _pid: u32, _start_time: u64) {}
}
