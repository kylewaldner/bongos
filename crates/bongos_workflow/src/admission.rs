//! The seam through which another crate makes a node's try wait for what it `needs` before it
//! starts (the pools feature), and the check that keeps a node's `needs` from being silently
//! ignored while nothing reads it.
//!
//! - [`NodeAdmission`] is asked before every start of a key ([`crate::runtime::Runtime`]): `Go`
//!   starts it as before, `Wait` appends nothing (the key holds no session slot, and the run is
//!   parked when nothing else of it can go on), `Refused` fails the try like a runner's start
//!   failure, with [`FailureReason::PoolRefused`](bongos_workflow_model::FailureReason). With no
//!   admission set every start goes.
//! - [`NodeNeeds`] is the validator of a node's `needs`. With none set, validation (`workflow:
//!   validate`, the landing check, the start path) reports every node that has `needs` as not
//!   available.

use std::sync::Mutex;

use bongos_workflow_model::definition::Node;
use bongos_workflow_model::{Definition, Diagnostic, NodeKey, Severity};
use gpui::{App, Global};

use crate::WorkflowEngine;
use crate::process::ProcessRecord;

/// Why a key does not start yet: what the run view and the stall watcher show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeWait {
    /// One short line ("waiting for pool gpu").
    pub text: String,
}

/// The answer to a start of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Start it now.
    Go,
    /// Not yet: nothing is appended, and the key is asked again at its next start command.
    Wait(NodeWait),
    /// Never: the try fails with this text.
    Refused(String),
}

/// Decides whether a key may start now, and hears of what became of the keys it let go. Called
/// with the runtime locked, so no method may wait or lock the workflow engine.
///
/// The runtime calls, per key: [`NodeAdmission::admit`] before each start (again at each start
/// command while it answers `Wait`); [`NodeAdmission::started`] once the try's process runs;
/// [`NodeAdmission::finished`] on every end of a try it let go (its `NodeFinished`, a failed start,
/// a stop by a halt, a cancel, a quit or a join) and when a waiting key is no longer offered. Per
/// run: [`NodeAdmission::run_waiting`] when the run is held for a person (halted, interrupted,
/// held on a question) and [`NodeAdmission::run_ended`] when it ends, is cancelled or is dropped.
pub trait NodeAdmission: Send {
    /// Whether `key` of `run` may start now.
    fn admit(&mut self, run: &str, definition: &Definition, key: &NodeKey) -> Admission;

    /// The key's try runs as `process` (pid 0 for a built-in, which has no process of its own).
    fn started(&mut self, run: &str, key: &NodeKey, process: ProcessRecord);

    /// The key's try ended or was stopped, or the key no longer waits: what it holds or asked for
    /// goes. Called again for a key with nothing is a no-op.
    fn finished(&mut self, run: &str, key: &NodeKey);

    /// The run waits for a person: every key of it that waits or was let go and never started
    /// drops its request. A key whose process runs keeps what it holds until it finishes.
    fn run_waiting(&mut self, run: &str);

    /// The run is over (ended, cancelled, or dropped from this process): everything it asked for
    /// or holds goes.
    fn run_ended(&mut self, run: &str);
}

/// The validator of a node's `needs`, set by the crate that reads it.
pub type NeedsValidator = fn(&Node) -> Vec<Diagnostic>;

static NEEDS_VALIDATOR: Mutex<Option<NeedsValidator>> = Mutex::new(None);

/// The message for a node with `needs` while nothing reads it.
pub const NEEDS_NOT_AVAILABLE: &str = "`needs` is not available (pools are off: pools.enabled)";

/// The validation of the nodes' `needs`.
pub struct NodeNeeds;

impl NodeNeeds {
    /// Judges every node's `needs` with `validator` from now on.
    pub fn set(validator: NeedsValidator) {
        *NEEDS_VALIDATOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(validator);
    }

    /// The error that refuses `node`'s `needs` while nothing reads it.
    pub fn not_available(node: &Node) -> Option<Diagnostic> {
        let needs = node.needs.as_ref()?;
        Some(Diagnostic {
            node: Some(node.name.clone()),
            line: needs.line,
            severity: Severity::Error,
            message: NEEDS_NOT_AVAILABLE.to_string(),
        })
    }

    /// The diagnostics of every node of `definition` that has `needs`: the validator's, or "not
    /// available" when none is set.
    pub fn check(definition: &Definition) -> Vec<Diagnostic> {
        let validator = *NEEDS_VALIDATOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        definition
            .nodes
            .iter()
            .filter(|node| node.needs.is_some())
            .flat_map(|node| match validator {
                Some(validator) => validator(node),
                None => Self::not_available(node).into_iter().collect(),
            })
            .collect()
    }
}

/// An admission registered before the engine was made, set on its runtime when it is.
#[derive(Default)]
struct PendingAdmission(Option<Box<dyn NodeAdmission>>);

impl Global for PendingAdmission {}

/// Sets `admission` on the app's engine's runtime, now when the engine exists, else when it is
/// made ([`WorkflowEngine::ensure`]). A later registration replaces an earlier one.
pub fn register_admission(admission: Box<dyn NodeAdmission>, cx: &mut App) {
    match cx.try_global::<WorkflowEngine>().cloned() {
        Some(engine) => engine.lifecycle().runtime_mut().set_admission(admission),
        None => cx.set_global(PendingAdmission(Some(admission))),
    }
}

/// Sets the admission registered before `engine` was made on its runtime.
pub(crate) fn use_registered(engine: &WorkflowEngine, cx: &mut App) {
    let pending = cx
        .try_global::<PendingAdmission>()
        .is_some_and(|pending| pending.0.is_some());
    if !pending {
        return;
    }
    if let Some(admission) = cx.default_global::<PendingAdmission>().0.take() {
        engine.lifecycle().runtime_mut().set_admission(admission);
    }
}
