//! The plan of moving a task subtree from one project to another, pure over the two masters and
//! the source's live facts: which tasks move and to which folders, the crossing edges that are
//! dropped, the code repositories the target lacks, a slug renamed when the target already has
//! it, and every reason the move is refused.
//!
//! Everything a plan names becomes a path: a task id is a files folder (`files/<id>/`) and a slug
//! a definition folder. An id or a folder that is not one plain name is refused, whether it came
//! from a definition, a request a peer sent, or a stored plan read back.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::checks::LiveFacts;
use crate::definitions::{DefinitionNode, DefinitionTree, RelPath};
use crate::layout::TASKS_DIR;
use crate::summary::EdgeKind;

/// The longest task id or slug a move accepts, in bytes.
const MAX_NAME_BYTES: usize = 200;

/// What a person (or a peer) asks to move: the task, and the task of the target project it goes
/// under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRequest {
    pub task_id: String,
    /// The task of the target project the subtree goes under (its top group for the top).
    pub parent_id: String,
}

/// A project by id and title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub title: String,
}

/// One moved task: its definition folder in the source and in the target, relative to
/// `definitions/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedTask {
    pub id: String,
    pub from: String,
    pub to: String,
}

/// An edge between the moved subtree and the rest of the source, dropped by the move.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DroppedEdge {
    /// The task holding the edge, by id and by slug.
    pub task: String,
    pub slug: String,
    pub kind: EdgeKind,
    /// The sibling it names, by slug.
    pub target: String,
    /// Whether the holder is the moved task (its edge is dropped in the target's copy) or a
    /// sibling staying in the source (dropped in the source's copy).
    pub holder_moves: bool,
}

impl fmt::Display for DroppedEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self.kind {
            EdgeKind::After => "after",
            EdgeKind::StackedOn => "stacked on",
        };
        write!(f, "{} {word} {}", self.slug, self.target)
    }
}

/// What a move does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePlan {
    pub source: ProjectRef,
    pub target: ProjectRef,
    /// The root of the moved subtree.
    pub task_id: String,
    /// The root's slug in the target.
    pub slug: String,
    /// The root's slug in the source, when the target already had it and it was renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// The root first, then its descendants depth first.
    pub tasks: Vec<MovedTask>,
    #[serde(default)]
    pub dropped_edges: Vec<DroppedEdge>,
    /// Code repositories the moved tasks name that the target's `project.toml` lacks.
    #[serde(default)]
    pub missing_repositories: Vec<String>,
}

/// Why a move cannot be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRefusal {
    pub reasons: Vec<String>,
}

impl fmt::Display for MoveRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reasons.join("; "))
    }
}

impl std::error::Error for MoveRefusal {}

impl MovePlan {
    /// The moved task ids, root first.
    pub fn task_ids(&self) -> Vec<String> {
        self.tasks.iter().map(|task| task.id.clone()).collect()
    }

    /// The root's folder in the source.
    pub fn root_from(&self) -> &str {
        self.tasks.first().map_or("", |task| task.from.as_str())
    }

    /// The root's folder in the target.
    pub fn root_to(&self) -> &str {
        self.tasks.first().map_or("", |task| task.to.as_str())
    }

    /// The line both messages carry about the dropped edges; `None` when none is dropped.
    pub fn dropped_edges_line(&self) -> Option<String> {
        if self.dropped_edges.is_empty() {
            return None;
        }
        let edges: Vec<String> = self.dropped_edges.iter().map(ToString::to_string).collect();
        Some(format!("Dropped edges: {}", edges.join(", ")))
    }

    /// Every name the plan turns into a path is a plain name, every folder a task folder of
    /// `definitions/`, the root the first task and no id twice. A plan read back from storage
    /// (or received) is checked again before anything is written from it.
    pub fn validate(&self) -> Result<(), String> {
        for id in [&self.source.id, &self.target.id, &self.task_id] {
            check_name(id)?;
        }
        check_name(&self.slug)?;
        if self.source.id == self.target.id {
            return Err("a move needs two projects".to_string());
        }
        let Some(root) = self.tasks.first() else {
            return Err("a move moves at least one task".to_string());
        };
        if root.id != self.task_id {
            return Err("the plan's first task is not its root".to_string());
        }
        if root.to.rsplit('/').next() != Some(self.slug.as_str()) {
            return Err("the plan's root folder does not end in its slug".to_string());
        }
        let mut seen = BTreeSet::new();
        for task in &self.tasks {
            check_name(&task.id)?;
            if !seen.insert(task.id.as_str()) {
                return Err(format!("task {} is in the plan twice", task.id));
            }
            for folder in [&task.from, &task.to] {
                if !is_task_folder(folder) || folder.is_empty() {
                    return Err(format!("{folder} is not a task folder"));
                }
            }
            if task.from != root.from && !task.from.starts_with(&format!("{}/", root.from)) {
                return Err(format!("{} is outside the moved subtree", task.from));
            }
            if task.to != root.to && !task.to.starts_with(&format!("{}/", root.to)) {
                return Err(format!("{} is outside the moved subtree", task.to));
            }
        }
        for edge in &self.dropped_edges {
            check_name(&edge.task)?;
            check_name(&edge.slug)?;
            check_name(&edge.target)?;
        }
        Ok(())
    }
}

/// Whether `name` is one plain path part: not empty, not `.` or `..`, no separator, no NUL, no
/// leading dot and not too long. Task ids and slugs become folder names.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c == '\0' || c.is_control())
}

fn check_name(name: &str) -> Result<(), String> {
    if is_plain_name(name) {
        Ok(())
    } else {
        Err(format!("{name:?} is not a plain name"))
    }
}

/// Whether `folder` is the top group (`""`) or `tasks/<slug>` at any depth, every part plain.
fn is_task_folder(folder: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    if RelPath::new(folder).is_none() {
        return false;
    }
    let parts: Vec<&str> = folder.split('/').collect();
    parts.len().is_multiple_of(2)
        && parts
            .chunks(2)
            .all(|pair| pair[0] == TASKS_DIR && is_plain_name(pair[1]))
}

/// The folder of the child `slug` of the task folder `parent`.
pub fn child_folder(parent: &str, slug: &str) -> String {
    if parent.is_empty() {
        format!("{TASKS_DIR}/{slug}")
    } else {
        format!("{parent}/{TASKS_DIR}/{slug}")
    }
}

fn project_ref(tree: &DefinitionTree, side: &str) -> Result<ProjectRef, String> {
    let project = tree
        .project
        .as_ref()
        .ok_or_else(|| format!("the {side} project has no project.toml"))?;
    let id = project
        .id
        .clone()
        .ok_or_else(|| format!("the {side} project has no id"))?;
    Ok(ProjectRef {
        id,
        title: project.title.clone(),
    })
}

fn walk<'a>(node: &'a DefinitionNode, nodes: &mut Vec<&'a DefinitionNode>) {
    nodes.push(node);
    for child in &node.children {
        walk(child, nodes);
    }
}

fn parent_of<'a>(tree: &'a DefinitionTree, folder: &str) -> Option<&'a DefinitionNode> {
    tree.nodes()
        .into_iter()
        .find(|node| node.children.iter().any(|child| child.folder == folder))
}

fn edges(node: &DefinitionNode) -> Vec<(EdgeKind, &str)> {
    node.definition
        .after
        .iter()
        .map(|edge| (EdgeKind::After, edge.task.as_str()))
        .chain(
            node.definition
                .stacked_on
                .iter()
                .map(|edge| (EdgeKind::StackedOn, edge.task.as_str())),
        )
        .collect()
}

/// The plan of moving the subtree of `request.task_id` from `source` (master's definitions) to
/// `target`, under `request.parent_id`. `live` holds the facts of the source's tasks: a started
/// task anywhere in the subtree, an active run or queue claim on one, or a hold (another move)
/// refuses, as do an id the target already has, a sibling that is started and holds an edge to
/// the moved task (dropping it would change a started task), and names that are not plain.
pub fn move_plan(
    source: &DefinitionTree,
    target: &DefinitionTree,
    request: &MoveRequest,
    live: &LiveFacts,
) -> Result<MovePlan, MoveRefusal> {
    let refuse = |reason: String| MoveRefusal {
        reasons: vec![reason],
    };
    let source_ref = project_ref(source, "source").map_err(refuse)?;
    let target_ref = project_ref(target, "target").map_err(refuse)?;
    for id in [&request.task_id, &request.parent_id] {
        if !is_plain_name(id) {
            return Err(refuse(format!("{id:?} is not a task id")));
        }
    }
    if source_ref.id == target_ref.id {
        return Err(refuse(format!(
            "the task is already in {}; pick another project",
            source_ref.title
        )));
    }
    let root = source.node(&request.task_id).ok_or_else(|| {
        refuse(format!(
            "there is no task {} in {}",
            request.task_id, source_ref.title
        ))
    })?;
    if root.folder.is_empty() {
        return Err(refuse(
            "the top group of a project cannot be moved".to_string(),
        ));
    }
    let parent = target.node(&request.parent_id).ok_or_else(|| {
        refuse(format!(
            "there is no task {} in {}",
            request.parent_id, target_ref.title
        ))
    })?;

    let mut subtree = Vec::new();
    walk(root, &mut subtree);
    let mut reasons = Vec::new();
    for node in &subtree {
        let id = &node.definition.id;
        if !is_plain_name(id) || !is_task_folder(&node.folder) {
            reasons.push(format!(
                "task {id:?} in {:?} cannot be moved: its id or folder is not a plain name",
                node.folder
            ));
            continue;
        }
        let which = if node.folder == root.folder {
            "task"
        } else {
            "child task"
        };
        if let Some(hold) = live.held(id) {
            reasons.push(format!("{which} {} is held: {hold}", node.slug));
        } else if live.started(id) {
            reasons.push(format!(
                "{which} {} is started; only tasks that have not started can move",
                node.slug
            ));
        }
        if target.node(id).is_some() {
            reasons.push(format!("task {id} is already in {}", target_ref.title));
        }
    }

    // Edges name siblings by slug, so only the root's edges and its siblings' edges to it cross.
    let mut dropped = Vec::new();
    for (kind, to) in edges(root) {
        dropped.push(DroppedEdge {
            task: root.definition.id.clone(),
            slug: root.slug.clone(),
            kind,
            target: to.to_string(),
            holder_moves: true,
        });
    }
    if let Some(source_parent) = parent_of(source, &root.folder) {
        for sibling in &source_parent.children {
            if sibling.folder == root.folder {
                continue;
            }
            for (kind, to) in edges(sibling) {
                if to != root.slug {
                    continue;
                }
                if live.started(&sibling.definition.id) {
                    reasons.push(format!(
                        "task {} is started and has an edge to {}; a started task's edges \
                         cannot be dropped",
                        sibling.slug, root.slug
                    ));
                }
                dropped.push(DroppedEdge {
                    task: sibling.definition.id.clone(),
                    slug: sibling.slug.clone(),
                    kind,
                    target: to.to_string(),
                    holder_moves: false,
                });
            }
        }
    }
    dropped.sort();
    dropped.dedup();
    if !reasons.is_empty() {
        return Err(MoveRefusal { reasons });
    }

    let taken: BTreeSet<&str> = parent
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    let mut slug = root.slug.clone();
    let mut number = 2;
    while taken.contains(slug.as_str()) {
        slug = format!("{}-{number}", root.slug);
        number += 1;
    }
    let renamed_from = (slug != root.slug).then(|| root.slug.clone());
    let root_to = child_folder(&parent.folder, &slug);
    let tasks = subtree
        .iter()
        .map(|node| MovedTask {
            id: node.definition.id.clone(),
            from: node.folder.clone(),
            to: format!("{root_to}{}", &node.folder[root.folder.len()..]),
        })
        .collect();

    let have: BTreeSet<&str> = target
        .project
        .as_ref()
        .map(|project| project.repositories.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut missing = BTreeSet::new();
    for node in &subtree {
        let definition = &node.definition;
        for repository in definition.repository.iter().chain(&definition.repositories) {
            if !have.contains(repository.as_str()) {
                missing.insert(repository.clone());
            }
        }
    }

    let plan = MovePlan {
        source: source_ref,
        target: target_ref,
        task_id: root.definition.id.clone(),
        slug,
        renamed_from,
        tasks,
        dropped_edges: dropped,
        missing_repositories: missing.into_iter().collect(),
    };
    plan.validate().map_err(refuse)?;
    Ok(plan)
}
