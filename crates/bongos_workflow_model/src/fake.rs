//! A scripted runner for tests: it drives [`scheduler::step`] to the end of a run with no
//! processes, playing a list of results per key or per node, and appends the events a real engine
//! would, so [`replay`] of its log is the run's state.
//!
//! Each round it applies every command [`scheduler::step`] gives; when there are none it finishes
//! the oldest running key with its next scripted result; when nothing runs either it wakes the
//! sleeping polls. Gates are approved as soon as they open. It records how many keys and branches
//! ran at once, so tests can check the limits.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;

use crate::definition::Definition;
use crate::failure::{Failure, FailureReason};
use crate::record::{EndState, Event, NodeKey, RunState, replay};
use crate::scheduler::{self, Command, Limits};

/// One scripted try.
#[derive(Debug, Clone, PartialEq)]
pub enum Scripted {
    /// The try finishes with this result (checked by the scheduler against the node's output).
    Result(Value),
    /// The try fails with this reason.
    Fail(FailureReason),
}

impl Scripted {
    /// A result from a JSON value.
    pub fn ok(value: Value) -> Self {
        Self::Result(value)
    }
}

/// The scripted runner. Scripts are queues: each try takes the front entry, and the last entry
/// stays, so one entry serves every try. A script for the exact key wins over one for its node.
pub struct FakeRunner {
    pub definition: Definition,
    pub limits: Limits,
    /// The log so far.
    pub events: Vec<Event>,
    /// Every key started, in order (a key per try).
    pub started: Vec<NodeKey>,
    /// Every command applied, in order.
    pub commands: Vec<Command>,
    /// The most keys running at once.
    pub max_running: usize,
    /// The most fan-out branches with a running key at once.
    pub max_branches: usize,
    /// Free session slots taken by other runs, subtracted from the session limit.
    pub other_sessions: usize,
    /// Joins whose merges conflict: the merge records a failed try of the join's key, as the
    /// engine does, and the rest of that step is dropped.
    pub conflicting_joins: BTreeSet<String>,
    by_key: BTreeMap<NodeKey, VecDeque<Scripted>>,
    by_node: BTreeMap<String, VecDeque<Scripted>>,
    /// Running keys in start order.
    running: Vec<NodeKey>,
}

impl FakeRunner {
    pub fn new(definition: Definition, limits: Limits) -> Self {
        Self::with_events(definition, limits, vec![Event::Created])
    }

    /// A runner continuing an existing log, as after a relaunch.
    pub fn with_events(definition: Definition, limits: Limits, events: Vec<Event>) -> Self {
        let running = replay(&events).running.keys().cloned().collect();
        Self {
            definition,
            limits,
            events,
            started: Vec::new(),
            commands: Vec::new(),
            max_running: 0,
            max_branches: 0,
            other_sessions: 0,
            conflicting_joins: BTreeSet::new(),
            by_key: BTreeMap::new(),
            by_node: BTreeMap::new(),
            running,
        }
    }

    /// Scripts every try of `node`, in finish order.
    pub fn script(&mut self, node: &str, tries: impl IntoIterator<Item = Scripted>) -> &mut Self {
        self.by_node
            .insert(node.to_string(), tries.into_iter().collect());
        self
    }

    /// Scripts the tries of one key; wins over the node's script.
    pub fn script_key(
        &mut self,
        key: NodeKey,
        tries: impl IntoIterator<Item = Scripted>,
    ) -> &mut Self {
        self.by_key.insert(key, tries.into_iter().collect());
        self
    }

    pub fn state(&self) -> RunState {
        replay(&self.events)
    }

    /// How the run ended, if it did.
    pub fn ended(&self) -> Option<(EndState, Option<Failure>)> {
        self.state().ended
    }

    /// How many tries of `key` started.
    pub fn tries(&self, key: &NodeKey) -> usize {
        self.started
            .iter()
            .filter(|started| *started == key)
            .count()
    }

    /// How many tries of any key of `node` started.
    pub fn starts_of(&self, node: &str) -> usize {
        self.started.iter().filter(|key| key.node == node).count()
    }

    /// Runs until the run ends or waits for Kyle (an interruption, a halt, a failed end).
    pub fn run(&mut self) -> RunState {
        for _ in 0..10_000 {
            if self.apply_step() || self.finish_oldest() || self.wake_sleeping() {
                continue;
            }
            return self.state();
        }
        panic!("the fake run did not settle in 10000 rounds");
    }

    /// Applies one [`scheduler::step`]; whether it gave any command.
    pub fn apply_step(&mut self) -> bool {
        let state = self.state();
        let running = state.running.len() + self.other_sessions;
        let free = self.limits.free_sessions(running);
        let commands = scheduler::step(&self.definition, &state, &self.limits, free);
        if commands.is_empty() {
            return false;
        }
        for command in commands {
            let carry_on = self.apply(&command);
            self.commands.push(command);
            if !carry_on {
                break;
            }
        }
        true
    }

    /// Resumes after a halt or an interruption: `Resumed`, then a new try of every key of
    /// [`scheduler::resume_set`].
    pub fn resume(&mut self) {
        let keys = scheduler::resume_set(&self.state());
        self.events.push(Event::Resumed);
        for key in keys {
            self.running.retain(|running| running != &key);
            self.start(key);
        }
    }

    /// A retry step: a new try of `key`, which reopens a failed run.
    pub fn retry_step(&mut self, key: NodeKey) {
        self.start(key);
    }

    /// Applies one command; whether the rest of the step may still be applied.
    fn apply(&mut self, command: &Command) -> bool {
        match command {
            Command::Start(key) => self.start(key.clone()),
            Command::OpenGate(key) => {
                self.events.push(Event::GateOpened {
                    key: key.clone(),
                    question: "fake question".to_string(),
                });
                self.events.push(Event::GateClosed {
                    key: key.clone(),
                    by: "person".to_string(),
                    edit: None,
                    comment: None,
                });
            }
            Command::MergeBranch { join, key, .. } if self.conflicting_joins.contains(join) => {
                let failure = Failure::new(
                    FailureReason::MergeConflict,
                    Some(key.clone()),
                    "fake merge conflict",
                );
                self.start(key.clone());
                self.finish(key.clone(), Scripted::Fail(failure.reason), Some(failure));
                return false;
            }
            Command::MergeBranch { join, branch, .. } => self.events.push(Event::BranchMerged {
                join: join.clone(),
                branch: branch.clone(),
                sha: "0000000".to_string(),
            }),
            Command::CancelBranch { branch, .. } => self.cancel(branch),
            Command::End { state, failure } => self.events.push(Event::Ended {
                state: *state,
                failure: failure.clone(),
            }),
        }
        true
    }

    fn start(&mut self, key: NodeKey) {
        let try_number = self
            .events
            .iter()
            .filter(
                |event| matches!(event, Event::NodeStarted { key: started, .. } if *started == key),
            )
            .count() as u32
            + 1;
        self.events.push(Event::NodeStarted {
            key: key.clone(),
            try_number,
            pid: 1000 + self.started.len() as u32,
            pid_start_time: 0,
        });
        self.started.push(key.clone());
        self.running.push(key);
        self.max_running = self.max_running.max(self.running.len());
        let branches: BTreeSet<_> = self
            .running
            .iter()
            .filter(|key| !key.branch.is_empty())
            .map(|key| &key.branch)
            .collect();
        self.max_branches = self.max_branches.max(branches.len());
    }

    /// Stops the running keys of a cancelled branch; they record an interruption not resumed.
    fn cancel(&mut self, branch: &[(String, u32)]) {
        let stopped: Vec<NodeKey> = self
            .running
            .iter()
            .filter(|key| key.branch.starts_with(branch))
            .cloned()
            .collect();
        for key in stopped {
            let failure = Failure::new(
                FailureReason::InterruptedNotResumed,
                Some(key.clone()),
                "cancelled by a join any",
            );
            self.finish(key, Scripted::Fail(failure.reason), Some(failure));
        }
    }

    /// Finishes the oldest running key with its next scripted try.
    pub fn finish_oldest(&mut self) -> bool {
        let Some(key) = self.running.first().cloned() else {
            return false;
        };
        let scripted = self.next_script(&key);
        self.finish(key, scripted, None);
        true
    }

    fn finish(&mut self, key: NodeKey, scripted: Scripted, failure: Option<Failure>) {
        self.running.retain(|running| running != &key);
        let try_number = self
            .state()
            .running
            .get(&key)
            .map_or(1, |running| running.try_number);
        let (result, failure) = match scripted {
            Scripted::Result(value) => (Some(value), None),
            Scripted::Fail(reason) => (
                None,
                Some(failure.unwrap_or_else(|| {
                    Failure::new(reason, Some(key.clone()), "scripted failure")
                })),
            ),
        };
        self.events.push(Event::NodeFinished {
            key,
            try_number,
            result,
            failure,
            cost_usd: 0.0,
            turns: None,
            elapsed_ms: 0,
            session_id: None,
        });
    }

    fn next_script(&mut self, key: &NodeKey) -> Scripted {
        let queue = match self.by_key.get_mut(key) {
            Some(queue) => queue,
            None => self
                .by_node
                .get_mut(&key.node)
                .unwrap_or_else(|| panic!("no script for {key}")),
        };
        if queue.len() > 1 {
            queue.pop_front().expect("the queue has entries")
        } else {
            queue
                .front()
                .cloned()
                .unwrap_or_else(|| panic!("empty script for {key}"))
        }
    }

    /// Starts the sleeping polls again, as the engine's timer would.
    fn wake_sleeping(&mut self) -> bool {
        let state = self.state();
        if state.ended.is_some() || !state.running.is_empty() {
            return false;
        }
        let sleeping = scheduler::plan(&self.definition, &state, &self.limits).sleeping;
        if sleeping.is_empty() {
            return false;
        }
        for key in sleeping {
            self.start(key);
        }
        true
    }
}
