//! Live status lines of running program nodes another crate supplies: a source registered for a
//! `use` name answers, for a running key of such a node, the line the run view shows under it
//! ("waiting: test-slots, place 2, 4m", "running part 5 of 8, 4 kept") and an optional longer
//! detail. With no source registered a node shows no line.

use std::collections::BTreeMap;
use std::rc::Rc;

use bongos_workflow_model::definition::NodeKind;
use bongos_workflow_model::{Definition, NodeKey};
use gpui::{App, Global};

/// What a running node is doing now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    /// One short line, shown under the node.
    pub line: String,
    pub detail: Option<String>,
}

/// Answers the live status of running keys of the nodes it is registered for.
pub trait NodeStatusSource: 'static {
    fn status(&self, run_id: &str, key: &NodeKey, cx: &App) -> Option<NodeStatus>;
}

/// The registered [`NodeStatusSource`]s, by the `use` name of their program nodes.
#[derive(Default, Clone)]
pub struct NodeStatusSources(BTreeMap<String, Rc<dyn NodeStatusSource>>);

impl Global for NodeStatusSources {}

impl NodeStatusSources {
    /// Registers `source` for program nodes with `use = use_name`, for the life of the app.
    pub fn register(use_name: impl Into<String>, source: Rc<dyn NodeStatusSource>, cx: &mut App) {
        cx.default_global::<Self>().0.insert(use_name.into(), source);
    }

    /// The status of `key` of run `run_id`, when its node has a source and the source answers.
    pub fn status(
        definition: &Definition,
        run_id: &str,
        key: &NodeKey,
        cx: &App,
    ) -> Option<NodeStatus> {
        let node = definition.node(&key.node)?;
        if node.kind.value != NodeKind::Program {
            return None;
        }
        let use_name = &node.use_node.as_ref()?.value;
        let source = cx.try_global::<Self>()?.0.get(use_name)?.clone();
        source.status(run_id, key, cx)
    }

    /// The status of each of `keys` (the run's keys in flight) that has one.
    pub fn running<'a>(
        definition: &Definition,
        run_id: &str,
        keys: impl IntoIterator<Item = &'a NodeKey>,
        cx: &App,
    ) -> BTreeMap<NodeKey, NodeStatus> {
        keys.into_iter()
            .filter_map(|key| {
                Self::status(definition, run_id, key, cx).map(|status| (key.clone(), status))
            })
            .collect()
    }

    /// Tells the open run views that a status changed, so they read it again.
    pub fn changed(cx: &mut App) {
        crate::run_view::refresh_views(cx);
    }
}
