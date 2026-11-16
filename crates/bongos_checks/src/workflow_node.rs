//! The workflow step "run checks": the built-in behind a program node with `use = "checks"`, and
//! its live status line in the run view.
//!
//! The workflow runtime starts, polls and stops the node through [`ChecksNode`], which hands each
//! request to a worker on the app's main thread; the worker runs the [`ChecksEngine`] with the
//! node's parameters (`with`: `checks`, `items` or `items_from`, `item`, `repeat`, `tree`) and
//! reports the fixed output (`verdict`, `failing`, `scope`, `evidence`, `summary`), on which the
//! workflow routes. The node has no process of its own (pid 0 in the run's log); the processes are
//! the check's parts, which the engine records and stops.
//!
//! - A check runs as long as it needs: no workflow time or output cap applies, and the key holds
//!   no session slot (pools govern checks).
//! - A stop (halt, cancel, quit, a join `any`) cancels the node's checks; the parts that finished
//!   stay in the unfinished record, so the next try of the key resumes from them, and the stopped
//!   try is never reported.
//! - When the task already has checks running (a person's run, or this key's stopped try still
//!   ending) the node waits for them to end, then starts.
//! - A drain stops the node's checks and reports nothing for it: whoever drained takes the key over
//!   (the engine's [`ChecksEngine::drain`] names it). When the drain is abandoned, the key's checks
//!   start again at the worker's next wake, from the kept parts.
//! - After a relaunch the runtime calls [`BuiltinNode::recover`] for each interrupted key, and the
//!   processes an earlier bongos left running for its parts are stopped.
//! - While `task_checks.enabled` is false the node gives no verdict ("task checks are off"), which
//!   holds the run unless the definition routes it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use bongos_checks_model::evidence::{EvidenceRecord, Params, Scope, StartedBy, TreeKind};
use bongos_checks_model::verdict::Verdict;
use bongos_workflow::runtime::Finish;
use bongos_workflow::{
    BuiltinNode, BuiltinNodes, BuiltinStart, NodeStatus, NodeStatusSource, NodeStatusSources, RunId,
};
use bongos_workflow_model::definition::CHECKS_PROGRAM;
use bongos_workflow_model::{Failure, FailureReason, NodeKey, RunState};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AsyncApp, Entity, WeakEntity};
use serde_json::{Value, json};
use settings::Settings as _;

use crate::engine::{CheckResult, ChecksEngine, ChecksEvent, Refusal, RunChecksRequest, RunReport};
use crate::settings::TaskChecksSettings;

/// A try of a key: `(run, key, try number)`.
type TryId = (RunId, NodeKey, u32);

/// What a checks node asks for, read from its `with` when its try starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeJob {
    pub run: RunId,
    pub key: NodeKey,
    pub try_number: u32,
    /// The checks to run; `None` runs the task's whole set.
    pub checks: Option<Vec<String>>,
    /// The items the checks are scoped to, from `items`, `items_from` or `item`.
    pub items: Option<Vec<String>>,
    /// How many times the checks run; the verdict combines the repeats.
    pub repeat: u32,
    pub tree: TreeKind,
}

impl NodeJob {
    /// Reads the node's `with` against the run's state: `items_from` names an earlier node's
    /// list field, and `item = "{{item}}"` is the item of the fan-out branch the key runs in.
    pub fn read(start: &BuiltinStart<'_>) -> Result<Self, Failure> {
        let key = start.key;
        let failure = |detail: String| {
            Failure::new(
                FailureReason::ProgramResultInvalid,
                Some(key.clone()),
                detail,
            )
        };
        let node = start
            .definition
            .node(&key.node)
            .ok_or_else(|| failure(format!("no node '{}' in the definition", key.node)))?;
        let with = &node.with;
        let checks = with
            .get("checks")
            .map(text_list)
            .transpose()
            .map_err(|()| failure("`with.checks` must be a list of check names".to_string()))?;
        let mut items = with
            .get("items")
            .map(text_list)
            .transpose()
            .map_err(|()| failure("`with.items` must be a list of text".to_string()))?;
        if let Some(reference) = with.get("items_from").and_then(toml::Value::as_str) {
            items = Some(items_from(start.state, key, reference).map_err(failure)?);
        }
        if let Some(item) = with.get("item").and_then(toml::Value::as_str) {
            let item = if item.contains("{{item}}") {
                let value = branch_item(start, key).ok_or_else(|| {
                    failure("`with.item` names the fan-out item, but this key has none".to_string())
                })?;
                item.replace("{{item}}", &value)
            } else {
                item.to_string()
            };
            items.get_or_insert_with(Vec::new).push(item);
        }
        let repeat = match with.get("repeat") {
            None => 1,
            Some(value) => value
                .as_integer()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count >= 1)
                .ok_or_else(|| failure("`with.repeat` must be a positive integer".to_string()))?,
        };
        let tree = match with.get("tree").and_then(toml::Value::as_str) {
            None | Some("tip") => TreeKind::Tip,
            Some("base") => TreeKind::Base,
            Some(other) => {
                return Err(failure(format!(
                    "`with.tree` must be \"tip\" or \"base\", not \"{other}\""
                )));
            }
        };
        Ok(Self {
            run: start.run.to_string(),
            key: key.clone(),
            try_number: start.try_number,
            checks,
            items,
            repeat,
            tree,
        })
    }

    /// Whether the run has parameters, so its records satisfy nothing.
    pub fn parameterized(&self) -> bool {
        self.items.is_some() || self.repeat > 1 || self.tree == TreeKind::Base
    }

    /// The engine's request for repeat `repeat_index` (1 first) on `task_id`.
    pub fn request(&self, task_id: &str, repeat_index: u32) -> RunChecksRequest {
        let params = self.parameterized().then(|| Params {
            items: self.items.clone().unwrap_or_default(),
            repeat_index: (self.repeat > 1).then_some(repeat_index),
            tree: (self.tree == TreeKind::Base).then_some(TreeKind::Base),
        });
        RunChecksRequest {
            task_id: task_id.to_string(),
            checks: self.checks.clone(),
            clean: false,
            started_by: StartedBy::Run {
                run_id: self.run.clone(),
                node_key: self.key.to_string(),
            },
            node: Some((self.run.clone(), self.key.clone())),
            params,
        }
    }

    fn try_id(&self) -> TryId {
        (self.run.clone(), self.key.clone(), self.try_number)
    }
}

fn text_list(value: &toml::Value) -> Result<Vec<String>, ()> {
    value
        .as_array()
        .ok_or(())?
        .iter()
        .map(|item| item.as_str().map(str::to_string).ok_or(()))
        .collect()
}

/// The text of a JSON value an item names: a string as it is, anything else as JSON.
fn item_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The last result of node `node` in `branch` or an enclosing branch.
fn last_result<'a>(state: &'a RunState, node: &str, branch: &[(String, u32)]) -> Option<&'a Value> {
    state
        .finished
        .iter()
        .rev()
        .filter(|finished| finished.key.node == node && branch.starts_with(&finished.key.branch))
        .find_map(|finished| finished.result.as_ref())
}

/// `<node>.<field>`: the list field of the last result of an earlier node.
fn items_from(state: &RunState, key: &NodeKey, reference: &str) -> Result<Vec<String>, String> {
    let Some((node, field)) = reference.split_once('.') else {
        return Err(format!(
            "`with.items_from` must be \"<node>.<list field>\", not \"{reference}\""
        ));
    };
    let result = last_result(state, node, &key.branch).ok_or_else(|| {
        format!("`with.items_from` names {reference}, but '{node}' has no result")
    })?;
    match result.get(field) {
        Some(Value::Array(items)) => Ok(items.iter().map(item_text).collect()),
        _ => Err(format!(
            "`with.items_from` names {reference}, which is not a list in '{node}''s result"
        )),
    }
}

/// The item of the fan-out branch `key` runs in: the fan node's list at the branch's index.
fn branch_item(start: &BuiltinStart<'_>, key: &NodeKey) -> Option<String> {
    let ((fan, index), outer) = key.branch.split_last()?;
    let result = last_result(start.state, fan, outer)?;
    let node = start.definition.node(fan)?;
    node.routes
        .iter()
        .filter_map(|route| route.fan_out.as_ref())
        .find_map(|fan_out| match result.get(&fan_out.over) {
            Some(Value::Array(items)) => items.get(*index as usize).map(item_text),
            _ => None,
        })
}

/// What the runtime and the worker share.
#[derive(Default)]
struct Shared {
    /// The tries that ended and are not polled yet.
    finishes: Vec<Finish>,
    /// The try in flight of each key.
    current: BTreeMap<(RunId, NodeKey), u32>,
    /// Tries stopped whose run has not ended yet; nothing of theirs is reported.
    stopped: BTreeSet<TryId>,
}

impl Shared {
    /// Reports `finish` unless its try was stopped or replaced.
    fn finish(&mut self, finish: Finish) {
        let id = (finish.run.clone(), finish.key.clone(), finish.try_number);
        if self.stopped.remove(&id) {
            return;
        }
        let slot = (finish.run.clone(), finish.key.clone());
        if self.current.get(&slot) == Some(&finish.try_number) {
            self.current.remove(&slot);
            self.finishes.push(finish);
        }
    }
}

enum Message {
    Start(NodeJob),
    Stop(Vec<TryId>),
    Recover {
        run: RunId,
        keys: Vec<NodeKey>,
    },
    /// The engine changed for `task_id`.
    Wake {
        task_id: String,
    },
    Ended {
        id: TryId,
        report: RunReport,
    },
}

/// The [`BuiltinNode`] of `use = "checks"`.
pub struct ChecksNode {
    messages: mpsc::UnboundedSender<Message>,
    shared: Arc<Mutex<Shared>>,
}

impl ChecksNode {
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The built-in over `engine`, with its worker on the main thread; nothing is registered.
    pub fn new(engine: &Entity<ChecksEngine>, cx: &mut App) -> (Arc<Self>, Rc<ChecksNodeStatus>) {
        let (messages, receiver) = mpsc::unbounded();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker = Rc::new(RefCell::new(Worker {
            engine: engine.downgrade(),
            shared: shared.clone(),
            messages: messages.clone(),
            pending: Vec::new(),
            launched: BTreeMap::new(),
        }));
        let wake = messages.clone();
        cx.subscribe(engine, move |_, event: &ChecksEvent, _| {
            let task_id = match event {
                ChecksEvent::Changed { task_id }
                | ChecksEvent::Waiting { task_id, .. }
                | ChecksEvent::Finished { task_id, .. } => task_id.clone(),
            };
            wake.unbounded_send(Message::Wake { task_id }).ok();
        })
        .detach();
        let looping = worker.clone();
        cx.spawn(async move |cx| {
            let mut receiver = receiver;
            while let Some(message) = receiver.next().await {
                let refresh = looping.borrow_mut().handle(message, cx);
                if refresh {
                    cx.update(NodeStatusSources::changed);
                }
            }
        })
        .detach();
        let status = Rc::new(ChecksNodeStatus {
            engine: engine.downgrade(),
            worker,
        });
        (Arc::new(Self { messages, shared }), status)
    }

    /// Registers the built-in and its status source for `use = "checks"`.
    pub fn register(engine: &Entity<ChecksEngine>, cx: &mut App) -> Arc<Self> {
        let (node, status) = Self::new(engine, cx);
        BuiltinNodes::register(CHECKS_PROGRAM, node.clone(), cx);
        NodeStatusSources::register(CHECKS_PROGRAM, status, cx);
        node
    }
}

impl BuiltinNode for ChecksNode {
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let job = NodeJob::read(&start)?;
        self.shared()
            .current
            .insert((job.run.clone(), job.key.clone()), job.try_number);
        if self.messages.unbounded_send(Message::Start(job)).is_err() {
            self.shared()
                .current
                .remove(&(start.run.to_string(), start.key.clone()));
            return Err(Failure::new(
                FailureReason::WorktreeUnavailable,
                Some(start.key.clone()),
                "the checks engine is not running",
            ));
        }
        Ok(())
    }

    fn poll(&self) -> Vec<Finish> {
        std::mem::take(&mut self.shared().finishes)
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        let mut stopped = Vec::new();
        {
            let mut shared = self.shared();
            for (run, key) in tries {
                shared
                    .finishes
                    .retain(|finish| !(finish.run == *run && finish.key == *key));
                if let Some(try_number) = shared.current.remove(&(run.clone(), key.clone())) {
                    let id = (run.clone(), key.clone(), try_number);
                    shared.stopped.insert(id.clone());
                    stopped.push(id);
                }
            }
        }
        if !stopped.is_empty() {
            self.messages.unbounded_send(Message::Stop(stopped)).ok();
        }
    }

    fn recover(&self, run: &str, keys: &[NodeKey]) {
        self.messages
            .unbounded_send(Message::Recover {
                run: run.to_string(),
                keys: keys.to_vec(),
            })
            .ok();
    }
}

/// A try waiting to start its next repeat, or running one.
struct Job {
    job: NodeJob,
    task_id: Option<String>,
    /// The repeat that runs or starts next, 1 first.
    repeat_index: u32,
    /// The results of the repeats that ended.
    results: Vec<CheckResult>,
}

/// The main-thread side of [`ChecksNode`].
struct Worker {
    engine: WeakEntity<ChecksEngine>,
    shared: Arc<Mutex<Shared>>,
    messages: mpsc::UnboundedSender<Message>,
    /// Tries waiting to start, oldest first: their task's checks are running, or the engine is
    /// drained.
    pending: Vec<Job>,
    /// The tries whose checks run now, by `(run, key)`.
    launched: BTreeMap<(RunId, NodeKey), Job>,
}

impl Worker {
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_stopped(&self, id: &TryId) -> bool {
        self.shared().stopped.contains(id)
    }

    /// Handles one message; whether an open run view should read its status lines again.
    fn handle(&mut self, message: Message, cx: &mut AsyncApp) -> bool {
        match message {
            Message::Start(job) => {
                if !self.is_stopped(&job.try_id()) {
                    self.pending.push(Job {
                        job,
                        task_id: None,
                        repeat_index: 1,
                        results: Vec::new(),
                    });
                }
                self.pump(cx);
                true
            }
            Message::Stop(ids) => {
                for id in ids {
                    self.stop(&id, cx);
                }
                self.pump(cx);
                true
            }
            Message::Recover { run, keys } => {
                self.recover(&run, &keys, cx);
                false
            }
            Message::Wake { task_id } => {
                self.pump(cx);
                self.launched
                    .values()
                    .chain(&self.pending)
                    .any(|job| job.task_id.as_deref() == Some(task_id.as_str()))
            }
            Message::Ended { id, report } => {
                self.ended(id, report, cx);
                self.pump(cx);
                true
            }
        }
    }

    fn stop(&mut self, id: &TryId, cx: &mut AsyncApp) {
        let before = self.pending.len();
        self.pending.retain(|job| job.job.try_id() != *id);
        if self.pending.len() != before {
            // It never started, so nothing of it will end.
            self.shared().stopped.remove(id);
            return;
        }
        let slot = (id.0.clone(), id.1.clone());
        let Some(job) = self.launched.get(&slot) else {
            self.shared().stopped.remove(id);
            return;
        };
        if job.job.try_number != id.2 {
            self.shared().stopped.remove(id);
            return;
        }
        let Some(task_id) = job.task_id.clone() else {
            return;
        };
        self.engine
            .update(cx, |engine, _| {
                if engine.node_of(&task_id) == Some(&slot) {
                    engine.cancel(&task_id);
                }
            })
            .ok();
    }

    fn recover(&self, run: &str, keys: &[NodeKey], cx: &mut AsyncApp) {
        let tasks = self
            .engine
            .update(cx, |engine, cx| {
                let task_id = engine.task_of_run(run, cx)?;
                Some(
                    keys.iter()
                        .filter(|key| {
                            !self
                                .launched
                                .contains_key(&(run.to_string(), (*key).clone()))
                        })
                        .map(|key| engine.kill_node_orphans(&task_id, run, key, cx))
                        .collect::<Vec<_>>(),
                )
            })
            .ok()
            .flatten()
            .unwrap_or_default();
        for task in tasks {
            task.detach();
        }
    }

    /// Starts every pending try that can start now, oldest first.
    fn pump(&mut self, cx: &mut AsyncApp) {
        let pending = std::mem::take(&mut self.pending);
        for mut job in pending {
            if self.is_stopped(&job.job.try_id()) {
                self.shared().stopped.remove(&job.job.try_id());
                continue;
            }
            match self.launch(&mut job, cx) {
                Launch::Started => {
                    let slot = (job.job.run.clone(), job.job.key.clone());
                    self.launched.insert(slot, job);
                }
                Launch::Wait => self.pending.push(job),
                Launch::Done(outcome) => self.report(&job, outcome),
            }
        }
    }

    fn launch(&self, job: &mut Job, cx: &mut AsyncApp) -> Launch {
        let messages = self.messages.clone();
        let id = job.job.try_id();
        let launched = self.engine.update(cx, |engine, cx| {
            if !TaskChecksSettings::get_global(cx).enabled {
                return Launch::Done(Err(no_verdict("task checks are off")));
            }
            if job.task_id.is_none() {
                job.task_id = engine.task_of_run(&job.job.run, cx);
            }
            let Some(task_id) = job.task_id.clone() else {
                return Launch::Done(Err(no_verdict("the run's task is not known")));
            };
            if engine.is_drained() || engine.is_running(&task_id) {
                return Launch::Wait;
            }
            match engine.start(job.job.request(&task_id, job.repeat_index), cx) {
                Ok(report) => {
                    cx.spawn(async move |_, _| {
                        let report = report.await;
                        messages.unbounded_send(Message::Ended { id, report }).ok();
                    })
                    .detach();
                    Launch::Started
                }
                Err(Refusal::Running { .. } | Refusal::Draining { .. }) => Launch::Wait,
                Err(Refusal::UnknownCheck { check, .. }) => Launch::Done(Err(Outcome::Failed(
                    format!("check {check} is not in the task's set"),
                ))),
                Err(refusal) => Launch::Done(Err(no_verdict(&refusal.to_string()))),
            }
        });
        launched.unwrap_or_else(|_| Launch::Done(Err(no_verdict("the checks engine is gone"))))
    }

    fn ended(&mut self, id: TryId, report: RunReport, cx: &mut AsyncApp) {
        let slot = (id.0.clone(), id.1.clone());
        if self
            .launched
            .get(&slot)
            .is_none_or(|job| job.job.try_number != id.2)
        {
            // A try no longer launched (stopped, then replaced): nothing of it is reported.
            self.shared().stopped.remove(&id);
            return;
        }
        let Some(mut job) = self.launched.remove(&slot) else {
            return;
        };
        if self.is_stopped(&id) {
            self.shared().stopped.remove(&id);
            return;
        }
        if report.cancelled() {
            let drained = self
                .engine
                .read_with(cx, |engine, _| engine.is_drained())
                .unwrap_or(false);
            if drained {
                // The drain hands the key over: nothing is reported, and the repeat starts again
                // from its kept parts if the engine takes runs again here.
                self.pending.push(job);
                return;
            }
        }
        job.results.extend(report.results().iter().cloned());
        if let Some(refusal) = report.refusal() {
            self.report(&job, Err(no_verdict(&refusal.to_string())));
            return;
        }
        if !report.cancelled() && job.repeat_index < job.job.repeat {
            job.repeat_index += 1;
            self.pending.push(job);
            return;
        }
        let records = job
            .task_id
            .as_deref()
            .and_then(|task_id| {
                self.engine
                    .read_with(cx, |engine, cx| engine.records(task_id, cx))
                    .ok()
            })
            .unwrap_or_default();
        let result = node_result(&job.results, &records);
        self.report(&job, Ok(result));
    }

    fn report(&self, job: &Job, outcome: Result<Value, Outcome>) {
        let job = &job.job;
        let outcome = match outcome {
            Ok(result) => Ok(result),
            Err(Outcome::NoVerdict(summary)) => Ok(json!({
                "verdict": "no_verdict",
                "failing": [],
                "scope": "",
                "evidence": [],
                "summary": summary,
            })),
            Err(Outcome::Failed(detail)) => Err(Failure::new(
                FailureReason::ProgramResultInvalid,
                Some(job.key.clone()),
                detail,
            )),
        };
        let finish = Finish {
            outcome,
            ..Finish::ok(
                job.run.clone(),
                job.key.clone(),
                job.try_number,
                Value::Null,
            )
        };
        self.shared().finish(finish);
    }
}

enum Launch {
    Started,
    Wait,
    Done(Result<Value, Outcome>),
}

/// How a try ends without a run of checks.
enum Outcome {
    /// The node gives no verdict, with this summary.
    NoVerdict(String),
    /// The node fails (a definition problem).
    Failed(String),
}

fn no_verdict(summary: &str) -> Outcome {
    Outcome::NoVerdict(summary.to_string())
}

/// The fixed output of a checks node from the results of its checks (over every repeat):
/// `fail` when any check failed, else `no_verdict` when any had none (or none ran), else `pass`.
/// `failing` names the checks that did not pass, `evidence` the records, and `summary` says why
/// there was no verdict, or how many checks passed.
pub fn node_result(results: &[CheckResult], records: &[EvidenceRecord]) -> Value {
    let failed = results.iter().any(|result| result.verdict == Verdict::Fail);
    let first_no_verdict = results.iter().find_map(|result| match &result.verdict {
        Verdict::NoVerdict(reason) => Some((result.check.as_str(), reason.text())),
        _ => None,
    });
    let verdict = if failed {
        "fail"
    } else if first_no_verdict.is_some() || results.is_empty() {
        "no_verdict"
    } else {
        "pass"
    };
    let mut failing: Vec<&str> = Vec::new();
    for result in results.iter().filter(|result| !result.verdict.is_pass()) {
        if !failing.contains(&result.check.as_str()) {
            failing.push(&result.check);
        }
    }
    let evidence: Vec<&str> = results
        .iter()
        .filter_map(|result| result.record_id.as_deref())
        .collect();
    let mut scopes: Vec<String> = Vec::new();
    for id in &evidence {
        if let Some(record) = records.iter().find(|record| record.id == *id) {
            let scope = match &record.variant.scope {
                Scope::All => "all".to_string(),
                Scope::Items { count, .. } => format!("{count} items"),
                Scope::None { reason } => format!("nothing in scope: {reason}"),
                Scope::Unscoped => "unscoped".to_string(),
            };
            let scope = format!("{}: {scope}", record.check);
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
    }
    let summary = match (verdict, first_no_verdict) {
        ("no_verdict", Some((_, reason))) => reason,
        ("no_verdict", None) => "no check ran".to_string(),
        _ => {
            let passed = results
                .iter()
                .filter(|result| result.verdict.is_pass())
                .count();
            format!("{passed} of {} checks passed", results.len())
        }
    };
    json!({
        "verdict": verdict,
        "failing": failing,
        "scope": scopes.join("; "),
        "evidence": evidence,
        "summary": summary,
    })
}

/// The run view's line for a running checks node: what each of its checks is doing.
pub struct ChecksNodeStatus {
    engine: WeakEntity<ChecksEngine>,
    worker: Rc<RefCell<Worker>>,
}

impl NodeStatusSource for ChecksNodeStatus {
    fn status(&self, run_id: &str, key: &NodeKey, cx: &App) -> Option<NodeStatus> {
        let engine = self.engine.upgrade()?;
        let engine = engine.read(cx);
        if let Some((_, live)) = engine.node_live(run_id, key) {
            let lines: Vec<String> = live
                .iter()
                .map(|(check, live)| format!("{check}: {}", live.text()))
                .collect();
            let line = if lines.is_empty() {
                "checks starting".to_string()
            } else {
                lines.join("; ")
            };
            return Some(NodeStatus { line, detail: None });
        }
        let worker = self.worker.try_borrow().ok()?;
        let waiting = worker
            .pending
            .iter()
            .any(|job| job.job.run == run_id && job.job.key == *key);
        waiting.then(|| NodeStatus {
            line: if engine.is_drained() {
                "waiting: checks are moving to another process".to_string()
            } else {
                "waiting: the task's checks are running elsewhere".to_string()
            },
            detail: None,
        })
    }
}
