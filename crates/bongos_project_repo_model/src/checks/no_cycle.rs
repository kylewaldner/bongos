//! Check 3, `no-cycle`: no sibling group's `after` and `stacked_on` edges close a cycle. Runs
//! the work model's own [`cycle`] over every sibling group.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use bongos_work_model::rules::{Place, cycle};
use bongos_work_model::store::Task;

use super::outcome::CheckOutcome;
use crate::definitions::{DefinitionNode, DefinitionTree};
use crate::split::{TaskStateRecord, join};
use crate::summary::{ChangeSummary, EdgeChange, EdgeKind, edges_of};

/// Check 3 over `tree`. Each cycle is reported once, named by the edge that closes it: an edge
/// the change adds (from `summary`) when one does, such as "cycle: tests after deploy closes build
/// -> deploy -> tests" (each arrow pointing from a task to one that comes after it).
pub fn no_cycle(tree: &DefinitionTree, summary: &ChangeSummary) -> CheckOutcome {
    let added: BTreeSet<&EdgeChange> = summary.edges_added.iter().collect();
    let mut refusals = Vec::new();
    for node in tree.nodes() {
        if node.children.is_empty() {
            continue;
        }
        let siblings: Vec<Task> = node.children.iter().map(task_of).collect();
        let by_id: BTreeMap<&str, &DefinitionNode> = node
            .children
            .iter()
            .map(|child| (child.definition.id.as_str(), child))
            .collect();
        let mut edges: Vec<(usize, EdgeChange)> = node
            .children
            .iter()
            .enumerate()
            .flat_map(|(index, child)| edges_of(child).into_iter().map(move |e| (index, e)))
            .collect();
        // Added edges first, so a cycle is named by the edge the change brings.
        edges.sort_by_key(|(_, edge)| !added.contains(edge));
        let mut reported: BTreeSet<BTreeSet<String>> = BTreeSet::new();
        for (index, edge) in edges {
            let task = &siblings[index];
            let Some(file) = task.file.as_ref() else {
                continue;
            };
            let place = Place {
                task,
                siblings: &siblings,
                ancestors: Vec::new(),
                project: None,
            };
            let Some(mut chain) = cycle(&place, file, &edge.target) else {
                continue;
            };
            if !reported.insert(chain.iter().cloned().collect()) {
                continue;
            }
            // `chain` runs from the task along its edges back to itself; shown in the order the
            // work flows, without the repeated task.
            chain.pop();
            chain.reverse();
            let from = by_id
                .get(edge.task.as_str())
                .map_or(edge.task.as_str(), |node| node.slug.as_str());
            let relation = match edge.kind {
                EdgeKind::After => "after",
                EdgeKind::StackedOn => "stacked on",
            };
            refusals.push(format!(
                "cycle: {from} {relation} {} closes {}",
                edge.target,
                chain.join(" -> ")
            ));
        }
    }
    CheckOutcome::from_refusals(refusals)
}

/// The work-model task of a definition node, with no state, for the edge walk.
fn task_of(node: &DefinitionNode) -> Task {
    Task {
        slug: node.slug.clone(),
        dir: PathBuf::from(&node.folder),
        definition: Some(PathBuf::from(&node.folder)),
        pending: None,
        needs_conversion: false,
        file: Some(join(node.definition.clone(), TaskStateRecord::default())),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    }
}
