//! The `checks` host command, and the window side that reaches it.
//!
//! The [`ChecksEngine`] runs only in the process that holds the project's state writer. A feature
//! that moves the writer into a separate host process registers [`handle`] there as the host
//! command of [`CHECKS_NAMESPACE`], and in each window process that gave the writer up registers
//! a forwarder with [`ChecksForwarderRegistry::set`] that sends a command payload to that host.
//!
//! Which process holds the writer: this process holds it exactly while no forwarder is
//! registered. Until a forwarder is set, every process is the writer and [`checks_access`] is
//! local; [`ChecksForwarderRegistry::clear`] (when the window takes the writer back) makes it
//! local again.

use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::engine::{ChecksEngine, RunChecksRequest};

/// The namespace of the host command.
pub const CHECKS_NAMESPACE: &str = "checks";

/// A writing command of task checks, as it travels to the process that holds the writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum ChecksCommand {
    /// Runs the checks of a task: `checks` names the ones to run, `None` every judged one;
    /// `clean` starts over and keeps no earlier part.
    RunChecks {
        task_id: String,
        checks: Option<Vec<String>>,
        clean: bool,
    },
    /// Stops the run of the task's checks.
    CancelChecks { task_id: String },
    /// Asks whether the done rule would hold the task if it were finished now.
    WouldHold { task_id: String },
}

/// The answer to a [`ChecksCommand`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case")]
pub enum ChecksReply {
    /// The run started; its progress is in the state store.
    Started,
    /// The run was not started, for `reason`.
    Refused { reason: String },
    /// `stopped` is false when no run of the task was live.
    Cancelled { stopped: bool },
    /// The reason the task would be held, `None` when it would not be.
    Hold { reason: Option<String> },
}

impl ChecksReply {
    pub fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).context("serializing a checks reply")
    }

    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        serde_json::from_value(value).context("reading a checks reply")
    }
}

/// Handles a [`ChecksCommand`] in the process that holds the writer, on its global engine.
///
/// `person` says whether the peer that sent the command may speak for the person at the
/// keyboard. Running and stopping checks need no such right, so it is not used yet; it is
/// carried for the commands that will (completing a task in spite of a hold).
pub fn handle(
    payload: serde_json::Value,
    person: bool,
    cx: &mut App,
) -> Task<Result<serde_json::Value>> {
    let _ = person;
    let command: ChecksCommand = match serde_json::from_value(payload) {
        Ok(command) => command,
        Err(error) => return Task::ready(Err(anyhow!("not a checks command: {error}"))),
    };
    let Some(engine) = ChecksEngine::global(cx) else {
        return Task::ready(Err(anyhow!("task checks are not running in this process")));
    };
    match command {
        ChecksCommand::RunChecks {
            task_id,
            checks,
            clean,
        } => {
            let request = RunChecksRequest {
                checks,
                clean,
                ..RunChecksRequest::person(task_id)
            };
            let reply = match engine.update(cx, |engine, cx| engine.start(request, cx)) {
                Ok(report) => {
                    // The run goes on without its report; its progress is in the store.
                    report.detach();
                    ChecksReply::Started
                }
                Err(refusal) => ChecksReply::Refused {
                    reason: refusal.to_string(),
                },
            };
            Task::ready(reply.to_value())
        }
        ChecksCommand::CancelChecks { task_id } => {
            let stopped = engine.update(cx, |engine, _| engine.cancel(&task_id));
            Task::ready(ChecksReply::Cancelled { stopped }.to_value())
        }
        ChecksCommand::WouldHold { task_id } => {
            let hold = would_hold(&task_id, cx);
            cx.background_spawn(async move { ChecksReply::Hold { reason: hold.await }.to_value() })
        }
    }
}

/// The reason the done rule would hold `task_id` if it were finished now, as the done decider
/// of this process (the one holding the writer) judges it; `None` when it would not be held.
pub fn would_hold(task_id: &str, cx: &mut App) -> Task<Option<String>> {
    match ChecksEngine::global(cx) {
        Some(engine) => engine.update(cx, |engine, cx| engine.would_hold(task_id, cx)),
        None => Task::ready(None),
    }
}

/// Sends a command payload to the process that holds the writer and resolves with its reply.
pub type ChecksForwarder =
    Rc<dyn Fn(serde_json::Value, &mut App) -> Task<Result<serde_json::Value>>>;

/// The forwarder of a window process that does not hold the writer; none in the process that
/// does.
#[derive(Default)]
pub struct ChecksForwarderRegistry {
    forwarder: Option<ChecksForwarder>,
}

impl Global for ChecksForwarderRegistry {}

impl ChecksForwarderRegistry {
    /// From now on this process does not hold the writer: commands go through `forwarder`.
    pub fn set(
        forwarder: Rc<dyn Fn(serde_json::Value, &mut App) -> Task<Result<serde_json::Value>>>,
        cx: &mut App,
    ) {
        cx.default_global::<Self>().forwarder = Some(forwarder);
    }

    /// This process holds the writer again: commands run in its own engine.
    pub fn clear(cx: &mut App) {
        cx.default_global::<Self>().forwarder = None;
    }

    pub fn get(cx: &App) -> Option<ChecksForwarder> {
        cx.try_global::<Self>()
            .and_then(|registry| registry.forwarder.clone())
    }
}

/// Where a window's writing command of task checks goes.
#[derive(Clone)]
pub enum ChecksAccess {
    /// This process holds the writer: its own engine.
    Local(Entity<ChecksEngine>),
    /// Another process holds it: the command travels there.
    Forwarded(ChecksForwarder),
}

impl ChecksAccess {
    /// Sends `command` through the forwarder, or handles it in this process's engine.
    pub fn send(&self, command: &ChecksCommand, cx: &mut App) -> Task<Result<ChecksReply>> {
        let payload = match serde_json::to_value(command) {
            Ok(payload) => payload,
            Err(error) => {
                return Task::ready(Err(anyhow!("serializing a checks command: {error}")));
            }
        };
        let reply = match self {
            Self::Local(_) => handle(payload, true, cx),
            Self::Forwarded(forward) => forward(payload, cx),
        };
        cx.background_spawn(async move { ChecksReply::from_value(reply.await?) })
    }
}

/// The way to the engine for a writing command: local while this process holds the writer (no
/// forwarder registered), else forwarded. `None` when neither exists (task checks not set up).
pub fn checks_access(cx: &App) -> Option<ChecksAccess> {
    match ChecksForwarderRegistry::get(cx) {
        Some(forward) => Some(ChecksAccess::Forwarded(forward)),
        None => ChecksEngine::global(cx).map(ChecksAccess::Local),
    }
}
