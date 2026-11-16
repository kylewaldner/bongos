//! Built-in program nodes another crate supplies: a program node whose `use` names a registered
//! [`BuiltinNode`] is started, polled and stopped through it instead of through the [`Runner`]
//! (crate::runtime::Runner), so it launches no command, agent or interpreter of its own.
//!
//! A built-in has no process of its own: its key is recorded with pid 0, which the orphan kill at
//! load leaves alone ([`crate::process::kill_orphans`] treats it as unknown). Whatever processes it
//! runs, it stops itself, and it cleans up after a crash in [`BuiltinNode::recover`], which the
//! runtime calls at load for each of its keys a crash or a quit interrupted.
//!
//! The registry is shared: the global one is handed to the engine's runtime when the engine is
//! created, and a node registered later is seen by that runtime too. With nothing registered for a
//! `use`, its nodes go to the runner as before.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use bongos_workflow_model::definition::NodeKind;
use bongos_workflow_model::{Definition, Failure, NodeKey, RunState};
use gpui::{App, Global};

use crate::runtime::Finish;

/// What a built-in is told when one of its keys starts.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinStart<'a> {
    pub run: &'a str,
    pub definition: &'a Definition,
    /// The run's state before the start: the earlier results its parameters may name.
    pub state: &'a RunState,
    pub key: &'a NodeKey,
    pub try_number: u32,
}

/// A program node the app supplies in code. Its methods are called with the runtime locked, so
/// none may wait or lock the workflow engine: each hands the work on and returns.
pub trait BuiltinNode: Send + Sync + 'static {
    /// Starts a try of `start.key`, or says why it cannot start (the try then fails with it).
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure>;

    /// The tries that ended since the last call. A stopped try is never reported.
    fn poll(&self) -> Vec<Finish>;

    /// Stops the tries of these keys, as `(run, key)`. From this call on, none of them is
    /// reported by [`BuiltinNode::poll`].
    fn stop(&self, tries: &[(String, NodeKey)]);

    /// After a relaunch: these keys of `run` were started by an earlier process and never
    /// finished. Stops whatever that process left running for them; their next try resumes.
    fn recover(&self, _run: &str, _keys: &[NodeKey]) {}
}

/// The registered built-ins, by the `use` name of their program nodes.
#[derive(Clone, Default)]
pub struct BuiltinNodes(Arc<Mutex<BTreeMap<String, Arc<dyn BuiltinNode>>>>);

impl Global for BuiltinNodes {}

impl std::fmt::Debug for BuiltinNodes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_list()
            .entries(self.lock().keys())
            .finish()
    }
}

impl BuiltinNodes {
    /// Registers `node` for program nodes with `use = name` in the global registry.
    pub fn register(name: impl Into<String>, node: Arc<dyn BuiltinNode>, cx: &mut App) {
        cx.default_global::<Self>().insert(name, node);
    }

    /// The global registry (shared: a later registration is seen through it).
    pub fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    pub fn insert(&self, name: impl Into<String>, node: Arc<dyn BuiltinNode>) {
        self.lock().insert(name.into(), node);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn BuiltinNode>> {
        self.lock().get(name).cloned()
    }

    /// The built-in that runs `key` of `definition`: a program node whose `use` is registered.
    pub fn for_key(&self, definition: &Definition, key: &NodeKey) -> Option<Arc<dyn BuiltinNode>> {
        let node = definition.node(&key.node)?;
        if node.kind.value != NodeKind::Program {
            return None;
        }
        self.get(&node.use_node.as_ref()?.value)
    }

    /// Every registered built-in, in name order.
    pub fn all(&self) -> Vec<Arc<dyn BuiltinNode>> {
        self.lock().values().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<dyn BuiltinNode>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
