//! The running work the builder's diffs and prints are told about: every task of the tree with an
//! open chat or an active run, as feature 03's live-modes sources report them now. It is collected
//! on the foreground just before it is used and never stored.

use bongos_builder_model::LiveSet;
use bongos_work::LiveModesRegistry;
use bongos_work::model::store::WorkTree;
use gpui::App;

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
