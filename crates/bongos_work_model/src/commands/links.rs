//! Results, edges and the stacked-branch commands: set and clear result, add and remove edge,
//! set unblock point, set merge ship, retarget and clear rebase.

use super::{Context, Outcome, edit_task, require_leaf};
use crate::rules::{self, EdgeKind, Place, Refusal};
use crate::store::WorkTree;
use crate::types::{
    Edge, Extra, Kind, ResultKind, ResultState, StackedOn, TaskFile, TaskId, TaskResult,
    UnblockPoint,
};

/// Where `task: retarget` points a task whose stacked-on parent was cancelled.
#[derive(Clone, Debug, PartialEq)]
pub enum Retarget {
    /// Stacked on another sibling, by id.
    StackedOn(TaskId),
    /// After another sibling, by id.
    After(TaskId),
    /// Branched from main: no edge.
    Main,
}

pub fn unblock_name(point: UnblockPoint) -> &'static str {
    match point {
        UnblockPoint::Implemented => "implemented",
        UnblockPoint::Review => "review",
        UnblockPoint::Merged => "merged",
    }
}

/// Review for `stacked on`, Merged for `after`.
pub fn default_unblock(kind: EdgeKind) -> UnblockPoint {
    match kind {
        EdgeKind::After => UnblockPoint::Merged,
        EdgeKind::StackedOn => UnblockPoint::Review,
    }
}

fn result_kind_name(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::Pr => "pr",
        ResultKind::Prs => "prs",
        ResultKind::Document => "document",
        ResultKind::Link => "link",
        ResultKind::Findings => "findings",
        ResultKind::Branch => "branch",
    }
}

fn result_state_name(state: ResultState) -> &'static str {
    match state {
        ResultState::Open => "open",
        ResultState::Approved => "approved",
        ResultState::Merged => "merged",
        ResultState::Closed => "closed",
    }
}

fn describe(result: &TaskResult) -> String {
    let mut parts = vec![result_kind_name(result.kind).to_string()];
    parts.extend(result.url.clone());
    parts.extend(result.urls.iter().cloned());
    parts.extend(
        result
            .state
            .map(|state| result_state_name(state).to_string()),
    );
    parts.join(" ")
}

/// `task: set result`. A leaf holds one result: setting the same kind and URL again updates it
/// (its state, say), anything else is refused until `task: clear result`. A `babysit` leaf holds
/// a set of PRs; each set adds to it.
pub fn set_result(tree: &WorkTree, ctx: &Context, id: &str, result: TaskResult) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "result")?;
        rules::check_set_result(place.slug(), file, &result)?;
        let mut extra = file
            .result
            .as_ref()
            .map(|existing| existing.extra.clone())
            .unwrap_or_default();
        extra.extend(result.extra.clone());
        let new = if file.kind == Kind::Babysit {
            let mut urls: Vec<String> = Vec::new();
            let existing = file.result.as_ref();
            let all = existing
                .into_iter()
                .flat_map(|existing| existing.url.iter().chain(&existing.urls))
                .chain(result.url.iter().chain(&result.urls));
            for url in all {
                if !urls.contains(url) {
                    urls.push(url.clone());
                }
            }
            TaskResult {
                kind: ResultKind::Prs,
                url: None,
                state: result
                    .state
                    .or_else(|| existing.and_then(|existing| existing.state)),
                urls,
                extra,
            }
        } else {
            TaskResult { extra, ..result }
        };
        let detail = describe(&new);
        file.result = Some(new);
        Ok(("result set", Some(detail)))
    })
}

/// `task: clear result`.
pub fn clear_result(tree: &WorkTree, ctx: &Context, id: &str) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let Some(old) = file.result.take() else {
            return Err(format!("{} has no result", place.slug()));
        };
        Ok(("result cleared", Some(describe(&old))))
    })
}

/// Adds the edge to `target` on `file`, after the rules.
fn add_edge_to(
    place: &Place,
    tree: &WorkTree,
    file: &mut TaskFile,
    target: &str,
    kind: EdgeKind,
    unblock: Option<UnblockPoint>,
) -> Result<String, Refusal> {
    let target = Place::locate(tree, target)?.task;
    rules::check_edge(place, file, target, kind)?;
    let unblock = unblock.unwrap_or_else(|| default_unblock(kind));
    let name = match kind {
        EdgeKind::After => {
            file.after.push(Edge {
                task: target.slug.clone(),
                unblock,
                extra: Extra::new(),
            });
            "after"
        }
        EdgeKind::StackedOn => {
            file.stacked_on = Some(StackedOn {
                task: target.slug.clone(),
                unblock,
                extra: Extra::new(),
            });
            "stacked on"
        }
    };
    Ok(format!(
        "{name} {} ({})",
        target.slug,
        unblock_name(unblock)
    ))
}

/// `task: add edge`: the task with `id` goes `after` or `stacked on` the sibling with `target`.
/// `unblock` defaults to Review for `stacked on` and Merged for `after`.
pub fn add_edge(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    target: &str,
    kind: EdgeKind,
    unblock: Option<UnblockPoint>,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let detail = add_edge_to(place, tree, file, target, kind, unblock)?;
        Ok(("edge added", Some(detail)))
    })
}

/// `task: remove edge`: removes the edge naming the sibling `target_slug`, which may be a broken
/// edge naming no sibling at all.
pub fn remove_edge(tree: &WorkTree, ctx: &Context, id: &str, target_slug: &str) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let mut removed = Vec::new();
        let before = file.after.len();
        file.after.retain(|edge| edge.task != target_slug);
        if file.after.len() != before {
            removed.push(format!("after {target_slug}"));
        }
        if file
            .stacked_on
            .as_ref()
            .is_some_and(|edge| edge.task == target_slug)
        {
            file.stacked_on = None;
            removed.push(format!("stacked on {target_slug}"));
        }
        if removed.is_empty() {
            return Err(format!("{} has no edge to {target_slug}", place.slug()));
        }
        Ok(("edge removed", Some(removed.join(", "))))
    })
}

/// `task: set unblock point` on the edge naming `target_slug`.
pub fn set_unblock_point(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    target_slug: &str,
    point: UnblockPoint,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let mut found = false;
        let mut changed = false;
        for edge in file
            .after
            .iter_mut()
            .filter(|edge| edge.task == target_slug)
        {
            found = true;
            changed |= edge.unblock != point;
            edge.unblock = point;
        }
        if let Some(edge) = file
            .stacked_on
            .as_mut()
            .filter(|edge| edge.task == target_slug)
        {
            found = true;
            changed |= edge.unblock != point;
            edge.unblock = point;
        }
        if !found {
            return Err(format!("{} has no edge to {target_slug}", place.slug()));
        }
        if !changed {
            return Err(format!(
                "the edge to {target_slug} already unblocks at {}",
                unblock_name(point)
            ));
        }
        Ok((
            "unblock point set",
            Some(format!("{target_slug}: {}", unblock_name(point))),
        ))
    })
}

/// `task: set merge ship`: whether a merged PR completes the ships todo of this task; `None`
/// follows the repository setting again.
pub fn set_merge_ship(tree: &WorkTree, ctx: &Context, id: &str, value: Option<bool>) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        if file.merge_is_ship == value {
            return Err(format!("{} already has that setting", place.slug()));
        }
        file.merge_is_ship = value;
        let detail = match value {
            Some(true) => "on",
            Some(false) => "off",
            None => "repository default",
        };
        Ok(("merge ship set", Some(detail.to_string())))
    })
}

/// `task: retarget`: replace the `stacked on` edge (usually to a cancelled parent) with a new
/// `stacked on` or `after` edge, or none (main). The new edge follows the edge rules. The base
/// recorded for rebase detection no longer applies and is cleared.
pub fn retarget(tree: &WorkTree, ctx: &Context, id: &str, to: Retarget) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let Some(old) = file.stacked_on.take() else {
            return Err(format!("{} is not stacked on anything", place.slug()));
        };
        file.base_sha = None;
        file.rebase_ack_sha = None;
        let new = match &to {
            Retarget::StackedOn(target) => {
                add_edge_to(place, tree, file, target, EdgeKind::StackedOn, None)?
            }
            Retarget::After(target) => {
                add_edge_to(place, tree, file, target, EdgeKind::After, None)?
            }
            Retarget::Main => "main".to_string(),
        };
        Ok(("retargeted", Some(format!("from {} to {new}", old.task))))
    })
}

/// `task: clear rebase`: the owner has seen that the parent's branch moved to `parent_head`, so
/// "rebase needed" clears until it moves again.
pub fn clear_rebase(tree: &WorkTree, ctx: &Context, id: &str, parent_head: &str) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        if file.stacked_on.is_none() {
            return Err(format!("{} is not stacked on anything", place.slug()));
        }
        if file.rebase_ack_sha.as_deref() == Some(parent_head) {
            return Err(format!("{} already cleared that rebase", place.slug()));
        }
        file.rebase_ack_sha = Some(parent_head.to_string());
        Ok(("rebase cleared", Some(parent_head.to_string())))
    })
}
