//! `task: new from linear`: the plan for importing a ticket and its subtickets as a tree of
//! tasks, the preview text, and the one [`Change`] that creates them. Nothing exists before the
//! change is applied; the change is a journaled multi-file write, so a crash leaves the old tree
//! or the new one.

use std::fmt;

use bongos_work_model::commands::{Change, Context, Outcome, Write, free_slug};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::slug::slugify;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, LinearState, TaskFile, TaskId, layout};

use crate::api::Issue;
use crate::graphql::CHILDREN_CAP;
use crate::sync::{entry, linked_task, plan_todo, task_path};

/// How many levels of subtickets below the ticket are imported. A ticket at the last level that
/// has subtickets imports as a leaf and its subtickets are listed as not imported.
pub const MAX_DEPTH: usize = 4;

/// The most subtickets one ticket may have to be imported (the cap of the API client).
pub const MAX_SUBTICKETS: usize = CHILDREN_CAP;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A ticket with imported subtickets.
    Group,
    /// A ticket without, with one `plan` todo.
    Leaf,
}

/// One task to create.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportNode {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: String,
    pub url: String,
    pub team_id: String,
    pub kind: NodeKind,
    pub children: Vec<ImportNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// A task already links it; `path` says where.
    AlreadyLinked { path: String },
    /// Deeper than [`MAX_DEPTH`] levels.
    TooDeep,
    /// The ticket has more than [`MAX_SUBTICKETS`] subtickets; it imports as a leaf.
    TooManySubtickets,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub identifier: String,
    pub reason: SkipReason,
}

/// A `blocks` relation, shown as a hint and never added as an edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub blocker: String,
    pub blocked: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportPlan {
    /// The group or project the tree goes under.
    pub placement: TaskId,
    pub placement_title: String,
    pub root: ImportNode,
    pub skipped: Vec<Skipped>,
    pub hints: Vec<Hint>,
}

/// Why there is no plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// A task already links the ticket: focus it, create nothing.
    AlreadyImported {
        identifier: String,
        task_id: Option<TaskId>,
        path: String,
    },
    /// The placement is missing, not writable or not a group.
    Placement(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::AlreadyImported {
                identifier, path, ..
            } => write!(f, "{identifier} is already imported as {path}"),
            ImportError::Placement(reason) => f.write_str(reason),
        }
    }
}

/// The plan for importing `issue` (with its subtickets read level by level into `children`)
/// under the group or project `placement`. A ticket with imported subtickets is a group, one
/// without a leaf. Subtickets that a task already links are skipped with the path; a ticket
/// deeper than [`MAX_DEPTH`] levels, or with more than [`MAX_SUBTICKETS`] subtickets, is listed
/// as not imported.
pub fn plan(issue: &Issue, tree: &WorkTree, placement: &str) -> Result<ImportPlan, ImportError> {
    let place = Place::locate(tree, placement).map_err(ImportError::Placement)?;
    place
        .task
        .check_writable()
        .map_err(|refused| ImportError::Placement(refused.to_string()))?;
    if derive::shape(place.task, tree) != Shape::Group {
        return Err(ImportError::Placement(format!(
            "{} is a leaf; import under a group or a project",
            place.slug()
        )));
    }
    if let Some(existing) = linked_task(tree, &issue.id, &issue.identifier, None) {
        return Err(ImportError::AlreadyImported {
            identifier: issue.identifier.clone(),
            task_id: existing.id().map(str::to_string),
            path: task_path(tree, existing),
        });
    }
    let mut skipped = Vec::new();
    let root = node(issue, 0, tree, &mut skipped);
    let mut hints = Vec::new();
    let mut nodes = Vec::new();
    flatten(&root, &mut nodes);
    collect_hints(issue, &nodes, &mut hints);
    Ok(ImportPlan {
        placement: placement.to_string(),
        placement_title: place.task.goal().to_string(),
        root,
        skipped,
        hints,
    })
}

fn node(issue: &Issue, depth: usize, tree: &WorkTree, skipped: &mut Vec<Skipped>) -> ImportNode {
    let mut children = Vec::new();
    if issue.children_truncated || issue.children.len() > MAX_SUBTICKETS {
        skipped.push(Skipped {
            identifier: issue.identifier.clone(),
            reason: SkipReason::TooManySubtickets,
        });
    } else if depth >= MAX_DEPTH {
        for child in &issue.children {
            skipped.push(Skipped {
                identifier: child.identifier.clone(),
                reason: SkipReason::TooDeep,
            });
        }
    } else {
        for child in &issue.children {
            match linked_task(tree, &child.id, &child.identifier, None) {
                Some(existing) => skipped.push(Skipped {
                    identifier: child.identifier.clone(),
                    reason: SkipReason::AlreadyLinked {
                        path: task_path(tree, existing),
                    },
                }),
                None => children.push(node(child, depth + 1, tree, skipped)),
            }
        }
    }
    ImportNode {
        id: issue.id.clone(),
        identifier: issue.identifier.clone(),
        title: issue.title.trim().to_string(),
        description: issue.description.clone(),
        url: issue.url.clone(),
        team_id: issue.team_id.clone(),
        kind: if children.is_empty() {
            NodeKind::Leaf
        } else {
            NodeKind::Group
        },
        children,
    }
}

fn flatten<'a>(node: &'a ImportNode, out: &mut Vec<&'a ImportNode>) {
    out.push(node);
    for child in &node.children {
        flatten(child, out);
    }
}

/// A relation counts when both tickets are in the plan; it names the identifiers.
fn collect_hints(issue: &Issue, nodes: &[&ImportNode], hints: &mut Vec<Hint>) {
    let mut issues = Vec::new();
    flatten_issues(issue, &mut issues);
    for node in nodes {
        let Some(source) = issues.iter().find(|candidate| candidate.id == node.id) else {
            continue;
        };
        for blocked in &source.blocks {
            if let Some(target) = nodes.iter().find(|candidate| candidate.id == *blocked) {
                hints.push(Hint {
                    blocker: node.identifier.clone(),
                    blocked: target.identifier.clone(),
                });
            }
        }
    }
}

fn flatten_issues<'a>(issue: &'a Issue, out: &mut Vec<&'a Issue>) {
    out.push(issue);
    for child in &issue.children {
        flatten_issues(child, out);
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

impl ImportPlan {
    pub fn groups(&self) -> usize {
        self.nodes()
            .iter()
            .filter(|node| node.kind == NodeKind::Group)
            .count()
    }

    pub fn leaves(&self) -> usize {
        self.nodes()
            .iter()
            .filter(|node| node.kind == NodeKind::Leaf)
            .count()
    }

    fn nodes(&self) -> Vec<&ImportNode> {
        let mut nodes = Vec::new();
        flatten(&self.root, &mut nodes);
        nodes
    }

    /// The read-only preview: what would be created, the hints, and what is not imported.
    pub fn preview(&self) -> String {
        let root = &self.root;
        let mut parts = Vec::new();
        if self.groups() > 0 {
            parts.push(plural(self.groups(), "group", "groups"));
        }
        parts.push(plural(self.leaves(), "leaf", "leaves"));
        let mut text = format!(
            "Import {} \"{}\" into {}\ncreate {}, each leaf with one todo: plan\n",
            root.identifier,
            root.title,
            self.placement_title,
            parts.join(" and ")
        );
        preview_node(root, 1, &mut text);
        for skipped in &self.skipped {
            text.push_str(&format!("  {} ", skipped.identifier));
            match &skipped.reason {
                SkipReason::AlreadyLinked { path } => {
                    text.push_str(&format!("already linked: {path}, skipped\n"))
                }
                SkipReason::TooDeep => {
                    text.push_str(&format!("not imported: deeper than {MAX_DEPTH} levels\n"))
                }
                SkipReason::TooManySubtickets => text.push_str(&format!(
                    "not imported: more than {MAX_SUBTICKETS} subtickets\n"
                )),
            }
        }
        if !self.hints.is_empty() {
            let hints: Vec<String> = self
                .hints
                .iter()
                .map(|hint| format!("{} blocks {}", hint.blocker, hint.blocked))
                .collect();
            text.push_str(&format!("Hints (not added): {}\n", hints.join(", ")));
        }
        text
    }
}

fn preview_node(node: &ImportNode, depth: usize, text: &mut String) {
    let kind = match node.kind {
        NodeKind::Group => "group  ",
        NodeKind::Leaf => "leaf ",
    };
    text.push_str(&format!(
        "{}{} {}{}\n",
        "  ".repeat(depth),
        node.identifier,
        kind,
        node.title
    ));
    for child in &node.children {
        preview_node(child, depth + 1, text);
    }
}

/// The one change that creates every task of `plan`: all new `task.toml` files and directories,
/// with their links, in one journal. `kind` is the placement's default kind for the leaves.
/// Refused when the placement is gone, not a writable group any more, or a ticket of the plan
/// became linked meanwhile. History on each task: `imported from SEQ-120`. The tasks are new, so
/// no applied proposal exists to seed a group's `offered` with.
pub fn change(tree: &WorkTree, ctx: &Context, plan: &ImportPlan, kind: Kind) -> Outcome {
    let place = Place::locate(tree, &plan.placement)?;
    place
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    if derive::shape(place.task, tree) != Shape::Group {
        return Err(format!(
            "{} is a leaf; import under a group or a project",
            place.slug()
        ));
    }
    for node in plan.nodes() {
        if let Some(existing) = linked_task(tree, &node.id, &node.identifier, None) {
            return Err(format!(
                "{} is linked to {}",
                node.identifier,
                task_path(tree, existing)
            ));
        }
    }
    let mut taken: Vec<String> = place
        .task
        .children
        .iter()
        .map(|child| child.slug.clone())
        .collect();
    let mut writes = Vec::new();
    let mut root_dir = None;
    emit(
        &plan.root,
        &place.task.dir,
        &mut taken,
        &plan.root.identifier,
        ctx,
        kind,
        &mut writes,
        &mut root_dir,
    );
    let root_dir = root_dir.ok_or_else(|| "the import creates nothing".to_string())?;
    Ok(Change {
        dir: root_dir,
        writes,
        entry: entry(
            ctx,
            &format!("imported from {}", plan.root.identifier),
            None,
        ),
        moves: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn emit(
    node: &ImportNode,
    parent_dir: &std::path::Path,
    taken: &mut Vec<String>,
    root_identifier: &str,
    ctx: &Context,
    kind: Kind,
    writes: &mut Vec<Write>,
    root_dir: &mut Option<std::path::PathBuf>,
) {
    let title = if node.title.is_empty() {
        node.identifier.as_str()
    } else {
        node.title.as_str()
    };
    let siblings: Vec<&str> = taken.iter().map(String::as_str).collect();
    let slug = free_slug(&slugify(title), &siblings);
    taken.push(slug.clone());
    let dir = layout::child_dir(parent_dir, &slug);
    root_dir.get_or_insert_with(|| dir.clone());

    let mut file = TaskFile::new(
        title,
        match node.kind {
            NodeKind::Group => Kind::Other,
            NodeKind::Leaf => kind,
        },
    );
    file.created = ctx.at;
    if node.kind == NodeKind::Leaf {
        file.todos = vec![plan_todo()];
    }
    file.linear = Some(node.identifier.clone());
    file.linear_state = Some(LinearState {
        issue_id: node.id.clone(),
        team_id: node.team_id.clone(),
        url: node.url.clone(),
        description: node.description.clone(),
        ..LinearState::default()
    });
    file.history.push(entry(
        ctx,
        &format!("imported from {root_identifier}"),
        None,
    ));
    writes.push(Write::Task {
        dir: dir.clone(),
        file,
    });
    let mut child_taken = Vec::new();
    for child in &node.children {
        emit(
            child,
            &dir,
            &mut child_taken,
            root_identifier,
            ctx,
            kind,
            writes,
            &mut None,
        );
    }
}
