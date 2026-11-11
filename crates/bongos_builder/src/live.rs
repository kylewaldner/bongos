//! The running work the builder's diffs and prints are told about: every task of the tree with an
//! open chat or an active run, as feature 03's live-modes sources report them now. It is collected
//! on the foreground just before it is used and never stored.

use std::collections::BTreeSet;
use std::sync::Arc;

use bongos_builder_model::LiveSet;
use bongos_work::LiveModesRegistry;
use bongos_work::model::store::WorkTree;
use gpui::{App, Global};

/// Where the workflow names come from, once a registry exists.
struct WorkflowNames(Box<dyn Fn(&App) -> Option<BTreeSet<String>>>);

impl Global for WorkflowNames {}

/// Tells the builder where the known workflow names come from. `names` answers `None` while
/// there is no registry (a workflow name in a proposal is then a warning) and the set of known
/// names once there is one (an unknown name is then an error). Until this is called the names
/// are unknown.
pub fn set_workflow_names(
    names: impl Fn(&App) -> Option<BTreeSet<String>> + 'static,
    cx: &mut App,
) {
    cx.set_global(WorkflowNames(Box::new(names)));
}

/// The known workflow names now, shared so a background task may hold them.
pub(crate) fn workflow_names(cx: &App) -> Option<Arc<BTreeSet<String>>> {
    let names = cx.try_global::<WorkflowNames>()?;
    (names.0)(cx).map(Arc::new)
}

/// The tasks of `tree` with an open chat or an active run.
pub(crate) fn collect(tree: &WorkTree, cx: &App) -> LiveSet {
    let mut live = LiveSet::default();
    for task in tree.tasks() {
        let Some(id) = task.id() else {
            continue;
        };
        let modes = LiveModesRegistry::live_modes(id, cx);
        if modes.open_chats > 0 {
            live.add_chat(id);
        }
        if modes.run.is_some() {
            live.add_run(id);
        }
    }
    live
}
