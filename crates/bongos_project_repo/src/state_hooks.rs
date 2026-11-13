//! State apply hooks: code of other features that runs at the end of every writer transaction
//! that writes task or todo rows, before it commits, and may write `kv` namespaces in the same
//! transaction.

use std::sync::Arc;

use anyhow::Result;
use bongos_project_repo_model::{Author, TaskDefinition, TaskStateRecord};
use bongos_work_model::TaskId;
use parking_lot::Mutex;

use crate::state::StateTx;
use crate::state_ops::StateOp;

/// One task whose state changed in a transaction.
pub struct TaskTouch<'a> {
    pub id: TaskId,
    /// The whole record, todo states included, before the transaction's first write to it.
    pub before: Option<TaskStateRecord>,
    /// The whole record after the transaction's own writes.
    pub after: Option<TaskStateRecord>,
    /// The definition at the old verified commit; `None` until definitions are loaded.
    pub definition_before: Option<&'a TaskDefinition>,
    /// The definition at the new verified commit; `None` until definitions are loaded.
    pub definition_after: Option<&'a TaskDefinition>,
}

/// What a transaction changed, as a hook sees it.
pub struct StateApplyChange<'a> {
    /// The ops applied, empty for a closure transaction.
    pub ops: &'a [StateOp],
    /// Every task whose state rows were written.
    pub touched: &'a [TaskTouch<'a>],
    /// The `kv` namespaces written.
    pub namespaces: &'a [String],
    pub origin: &'a Author,
}

pub trait StateApplyHook: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn wants(&self, change: &StateApplyChange) -> bool {
        !change.touched.is_empty()
    }

    /// Writes only `kv` namespaces through `tx`; an error aborts the whole transaction. Must be
    /// fast and deterministic, and never start a run, process or agent.
    fn apply(&self, tx: &mut StateTx, change: &StateApplyChange) -> Result<()>;
}

/// The registered hooks, run in registration order. Sealed when the first writer opens; a later
/// registration is a bug.
pub struct StateApplyHooks {
    inner: Mutex<HooksInner>,
}

struct HooksInner {
    hooks: Vec<Arc<dyn StateApplyHook>>,
    sealed: bool,
}

static GLOBAL_HOOKS: StateApplyHooks = StateApplyHooks::new();

impl Default for StateApplyHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl StateApplyHooks {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(HooksInner {
                hooks: Vec::new(),
                sealed: false,
            }),
        }
    }

    /// The hooks every project's writer runs. Features register here from their `init`.
    pub fn global() -> &'static Self {
        &GLOBAL_HOOKS
    }

    /// Adds `hook`. After the first writer opened, this panics in debug builds and is logged and
    /// ignored in release builds.
    pub fn register(&self, hook: Arc<dyn StateApplyHook>) {
        let mut inner = self.inner.lock();
        if inner.sealed {
            drop(inner);
            if cfg!(debug_assertions) {
                panic!(
                    "state hook {} registered after a project was opened",
                    hook.name()
                );
            }
            log::error!(
                "state hook {} registered after a project was opened; ignored",
                hook.name()
            );
            return;
        }
        inner.hooks.push(hook);
    }

    pub(crate) fn seal(&self) -> Arc<[Arc<dyn StateApplyHook>]> {
        let mut inner = self.inner.lock();
        inner.sealed = true;
        inner.hooks.iter().cloned().collect()
    }
}
