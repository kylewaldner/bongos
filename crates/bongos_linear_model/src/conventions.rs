//! Branch names, PR titles and the context text for a task with a synced link (D15). The text
//! names identifiers, goals and branch names only, never an artifact.

use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::{Place, linear_branch_segment};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{ResultKind, TaskFile};

use crate::sync::{link_of, linked_identifier};

/// The PR title of a task with a synced link: `SEQ-121 <goal>`. `None` for any other task: an
/// unlinked task, a hand-set link and a pending create have no rule.
pub fn pr_title(file: &TaskFile) -> Option<String> {
    let identifier = linked_identifier(file)?;
    Some(format!("{identifier} {}", file.goal.trim()))
}

/// The branch segment for a synced link: `rules::linear_branch_segment`, the one implementation
/// (it guards an empty template, a template ending in `/` and an empty slug).
pub fn branch_segment(identifier: &str, template: &str, slug: &str) -> String {
    linear_branch_segment(identifier, template, slug)
}

/// [`branch_segment`] for a task: `None` unless it has a synced link, which keeps today's rule.
pub fn branch_name(file: &TaskFile, template: &str, slug: &str) -> Option<String> {
    Some(branch_segment(linked_identifier(file)?, template, slug))
}

/// The identifiers of the linked leaves stacked on `task_id`, directly or through a chain, in
/// sibling order.
pub fn stacked_identifiers(tree: &WorkTree, task_id: &str) -> Vec<String> {
    let Ok(place) = Place::locate(tree, task_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_stacked(&place, place.task, &mut out, 0);
    out
}

fn collect_stacked(place: &Place, task: &Task, out: &mut Vec<String>, depth: usize) {
    // A chain longer than the siblings is a cycle.
    if depth > place.siblings.len() {
        return;
    }
    for sibling in place.siblings {
        let stacked_here = sibling
            .file
            .as_ref()
            .and_then(|file| file.stacked_on.as_ref())
            .is_some_and(|edge| edge.task == task.slug);
        if !stacked_here || sibling.dir == place.task.dir {
            continue;
        }
        if let Some((identifier, _)) = link_of(sibling)
            && !out.iter().any(|seen| seen == identifier)
        {
            out.push(identifier.to_string());
        }
        collect_stacked(place, sibling, out, depth + 1);
    }
}

/// What the context extension adds to a chat's `context.md` on a linked leaf, or `None` for any
/// other task.
pub fn context_text(tree: &WorkTree, task_id: &str) -> Option<String> {
    let place = Place::locate(tree, task_id).ok()?;
    let file = place.task.file.as_ref()?;
    let identifier = linked_identifier(file)?;
    if derive::shape(place.task, tree) != Shape::Leaf {
        return None;
    }
    let title = pr_title(file)?;
    let mut text = format!(
        "This task is Linear {identifier}. Its branch name carries {}. Start the PR title with \
         `{identifier} ` (for example `{title}`). Do not put other ticket ids or closing words in \
         the title.",
        identifier.to_lowercase()
    );
    let stacked = stacked_identifiers(tree, task_id);
    if let Some(first) = stacked.first() {
        text.push_str(&format!(
            "\n\nLeaves stacked on this one merge into its branch: {}. Add one line per ticket \
             to the PR description: `Closes {first}`.",
            stacked.join(", ")
        ));
    }
    Some(text)
}

/// The toast for a linked leaf stacked onto a linked leaf whose PR already exists: bongos has no
/// GitHub write path, so Kyle adds the line himself.
pub fn open_pr_warning(tree: &WorkTree, task_id: &str) -> Option<String> {
    let place = Place::locate(tree, task_id).ok()?;
    let (child, _) = link_of(place.task)?;
    let edge = place.task.file.as_ref()?.stacked_on.as_ref()?;
    let parent = place.sibling(&edge.task)?;
    let (parent_identifier, _) = link_of(parent)?;
    let result = parent.file.as_ref()?.result.as_ref()?;
    let url = match result.kind {
        ResultKind::Pr => result.url.as_deref()?,
        _ => return None,
    };
    Some(format!(
        "PR {url} for {parent_identifier} is already open; add `Closes {child}` to its description"
    ))
}
