//! Which tasks a workflow's graph edits may touch (D3), decided on the in-memory tree the batch
//! has built so far, so a task an earlier op created counts by its parent's membership.
//!
//! - `none`: nothing.
//! - `own-subtree`: the run's task and its descendants ([`is_ancestor`]).
//! - `own-project`: every task of the run's project. The tree is that project's, so every task in
//!   it is in scope.
//! - `projects`: the orchestrator resolves the listed project names and hands `apply` the tree of
//!   the project the batch edits, with `ctx.project_id` set to it; a project it accepted is in
//!   scope, so here `projects` behaves like `own-project` on that tree.

use bongos_project_repo_model::{DefinitionNode, DefinitionTree};
use bongos_workflow_model::definition::Scope;

use crate::apply::ApplyCtx;

/// Whether the task `task` of `tree` may be edited under `ctx.scope`. False for an unknown task.
pub fn contains(tree: &DefinitionTree, ctx: &ApplyCtx, task: &str) -> bool {
    match ctx.scope {
        Scope::None => false,
        Scope::OwnSubtree => is_ancestor(tree, &ctx.run_task, task),
        Scope::OwnProject | Scope::Projects => tree.node(task).is_some(),
    }
}

/// Whether a batch naming `project` (its `project` field, when it has one) may edit it.
pub fn project_allowed(ctx: &ApplyCtx, project: &str) -> bool {
    ctx.scope != Scope::None && project == ctx.project_id
}

/// Whether `ancestor_id` is `id` or one of its ancestors in `tree`; false when either is unknown.
/// The semantics of `WorkTree::is_ancestor`, on parsed definitions.
pub fn is_ancestor(tree: &DefinitionTree, ancestor_id: &str, id: &str) -> bool {
    fn path_to<'a>(node: &'a DefinitionNode, id: &str, path: &mut Vec<&'a DefinitionNode>) -> bool {
        path.push(node);
        if node.definition.id == id || node.children.iter().any(|child| path_to(child, id, path)) {
            return true;
        }
        path.pop();
        false
    }

    let mut path = Vec::new();
    tree.top
        .as_ref()
        .is_some_and(|top| path_to(top, id, &mut path))
        && path.iter().any(|node| node.definition.id == ancestor_id)
}
