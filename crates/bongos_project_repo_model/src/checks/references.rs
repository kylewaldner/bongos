//! Check 4, `references`: every edge names a sibling and every todo workflow resolves. Parents
//! exist by construction of a parsed [`DefinitionTree`] (a folder without its parent's
//! `task.toml` does not parse, which check 1 reports).

use std::collections::BTreeSet;

use super::outcome::CheckOutcome;
use crate::definitions::DefinitionTree;
use crate::summary::{EdgeKind, edges_of};

/// Check 4 over `tree`. `workflow_resolves` says whether a workflow name resolves in the three
/// layers (the project's `workflows/`, the user's and the built-ins).
pub fn references(tree: &DefinitionTree, workflow_resolves: &dyn Fn(&str) -> bool) -> CheckOutcome {
    let mut refusals = Vec::new();
    for parent in tree.nodes() {
        let slugs: BTreeSet<&str> = parent
            .children
            .iter()
            .map(|child| child.slug.as_str())
            .collect();
        for child in &parent.children {
            for edge in edges_of(child) {
                if edge.target == child.slug || !slugs.contains(edge.target.as_str()) {
                    let relation = match edge.kind {
                        EdgeKind::After => "after",
                        EdgeKind::StackedOn => "stacked_on",
                    };
                    refusals.push(format!(
                        "{}: {relation} names {}, which is not a sibling",
                        child.slug, edge.target
                    ));
                }
            }
        }
    }
    for node in tree.nodes() {
        for todo in &node.definition.todos {
            if let Some(workflow) = &todo.workflow
                && !workflow_resolves(workflow)
            {
                refusals.push(format!(
                    "{}: todo {} names workflow {workflow}, which does not exist",
                    if node.slug.is_empty() {
                        "the top group"
                    } else {
                        &node.slug
                    },
                    todo.name
                ));
            }
        }
    }
    CheckOutcome::from_refusals(refusals)
}
