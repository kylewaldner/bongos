//! What a change does to the definitions, computed purely from two parsed trees
//! ([`summarize`]): the summary every landing check reads, and the text of the change's row in the
//! landing view and of a subject the author did not write.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::Author;
use crate::definitions::{DefinitionNode, DefinitionTree};
use crate::layout::{PROJECT_FILE, TASK_FILE, WORKFLOWS_DIR};

/// How one file differs between master and the change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileChange {
    Added,
    Modified,
    Removed,
}

/// Which list of a task an edge sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeKind {
    After,
    StackedOn,
}

/// One edge: the task holding it, by id, and the sibling it names, by slug.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct EdgeChange {
    pub task: String,
    pub kind: EdgeKind,
    pub target: String,
}

/// What a change does, task ids sorted. `files` are relative to `definitions/`; [`summarize`]
/// fills them with the definition files the parsed trees hold, and [`ChangeSummary::with_files`]
/// replaces them with every changed path of the git diff.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ChangeSummary {
    pub files: Vec<(String, FileChange)>,
    pub tasks_added: Vec<String>,
    pub tasks_removed: Vec<String>,
    /// Tasks in both trees whose definition or folder differs.
    pub tasks_redefined: Vec<String>,
    pub edges_added: Vec<EdgeChange>,
    pub edges_removed: Vec<EdgeChange>,
    /// Tasks in both trees whose `areas` differ.
    pub retagged: Vec<String>,
    /// Tasks in both trees whose parent folder differs.
    pub reparented: Vec<String>,
    /// Tasks in both trees whose todos differ.
    pub todos_changed: Vec<String>,
    /// The workflow folders (`workflows/<dir>`) a changed file is in, by `<dir>`.
    pub workflows_touched: Vec<String>,
    /// Set by the landing engine; [`summarize`] leaves it empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
}

impl ChangeSummary {
    /// The summary with `files` (relative to `definitions/`) as its changed files, and the
    /// workflows they touch.
    pub fn with_files(mut self, files: Vec<(String, FileChange)>) -> Self {
        self.workflows_touched = workflows_touched(&files);
        self.files = files;
        self
    }

    pub fn with_author(mut self, author: Author) -> Self {
        self.author = Some(author);
        self
    }

    /// Whether the change touches no task.
    pub fn touches_no_task(&self) -> bool {
        self.tasks_added.is_empty()
            && self.tasks_removed.is_empty()
            && self.tasks_redefined.is_empty()
    }

    /// The text of the change's row in the landing view, such as "adds 5 tasks, 2 edges; retags
    /// 1".
    pub fn row_text(&self) -> String {
        let groups = self.groups(false);
        if groups.is_empty() {
            "no changes".to_string()
        } else {
            groups.join("; ")
        }
    }

    /// The subject of a landing whose author wrote none, such as "Add 5 tasks, 2 edges; retag 1".
    pub fn generated_subject(&self) -> String {
        let groups = self.groups(true);
        if groups.is_empty() {
            return "Change definitions".to_string();
        }
        let text = groups.join("; ");
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => text,
        }
    }

    fn groups(&self, imperative: bool) -> Vec<String> {
        let verb = |third: &'static str, base: &'static str| if imperative { base } else { third };
        let mut groups = Vec::new();
        let mut push = |verb: &str, items: Vec<String>| {
            if !items.is_empty() {
                groups.push(format!("{verb} {}", items.join(", ")));
            }
        };
        push(
            verb("adds", "add"),
            [
                counted(self.tasks_added.len(), "task"),
                counted(self.edges_added.len(), "edge"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );
        push(
            verb("removes", "remove"),
            [
                counted(self.tasks_removed.len(), "task"),
                counted(self.edges_removed.len(), "edge"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );
        push(verb("retags", "retag"), number(self.retagged.len()));
        push(verb("moves", "move"), number(self.reparented.len()));
        push(
            verb("changes todos of", "change todos of"),
            number(self.todos_changed.len()),
        );
        push(
            verb("redefines", "redefine"),
            number(self.otherwise_redefined()),
        );
        push(
            verb("touches", "touch"),
            counted(self.workflows_touched.len(), "workflow")
                .into_iter()
                .collect(),
        );
        if groups.is_empty()
            && let Some(files) = counted(self.files.len(), "file")
        {
            groups.push(format!("{} {files}", verb("changes", "change")));
        }
        groups
    }

    /// Redefined tasks whose change no other group names.
    fn otherwise_redefined(&self) -> usize {
        let named: BTreeSet<&str> = self
            .retagged
            .iter()
            .chain(&self.reparented)
            .chain(&self.todos_changed)
            .map(String::as_str)
            .chain(
                self.edges_added
                    .iter()
                    .chain(&self.edges_removed)
                    .map(|edge| edge.task.as_str()),
            )
            .collect();
        self.tasks_redefined
            .iter()
            .filter(|id| !named.contains(id.as_str()))
            .count()
    }
}

fn counted(count: usize, noun: &str) -> Option<String> {
    match count {
        0 => None,
        1 => Some(format!("1 {noun}")),
        _ => Some(format!("{count} {noun}s")),
    }
}

fn number(count: usize) -> Vec<String> {
    if count == 0 {
        Vec::new()
    } else {
        vec![count.to_string()]
    }
}

fn workflows_touched(files: &[(String, FileChange)]) -> Vec<String> {
    let prefix = format!("{WORKFLOWS_DIR}/");
    let dirs: BTreeSet<String> = files
        .iter()
        .filter_map(|(path, _)| {
            let rest = path.strip_prefix(&prefix)?;
            let (dir, _) = rest.split_once('/')?;
            Some(dir.to_string())
        })
        .collect();
    dirs.into_iter().collect()
}

/// The parent folder of a task folder (`""` for a task directly under the top group, and for the
/// top group itself).
pub(crate) fn parent_folder(folder: &str) -> &str {
    folder
        .rsplit_once('/')
        .and_then(|(rest, _)| rest.rsplit_once('/'))
        .map_or("", |(parent, _)| parent)
}

pub(crate) fn nodes_by_id(tree: &DefinitionTree) -> BTreeMap<&str, &DefinitionNode> {
    tree.nodes()
        .into_iter()
        .map(|node| (node.definition.id.as_str(), node))
        .collect()
}

pub(crate) fn edges_of(node: &DefinitionNode) -> BTreeSet<EdgeChange> {
    let id = &node.definition.id;
    node.definition
        .after
        .iter()
        .map(|edge| (EdgeKind::After, &edge.task))
        .chain(
            node.definition
                .stacked_on
                .iter()
                .map(|edge| (EdgeKind::StackedOn, &edge.task)),
        )
        .map(|(kind, target)| EdgeChange {
            task: id.clone(),
            kind,
            target: target.clone(),
        })
        .collect()
}

fn task_file(folder: &str) -> String {
    if folder.is_empty() {
        TASK_FILE.to_string()
    } else {
        format!("{folder}/{TASK_FILE}")
    }
}

/// What the change from `master` to `new` does to the definitions.
pub fn summarize(master: &DefinitionTree, new: &DefinitionTree) -> ChangeSummary {
    let old = nodes_by_id(master);
    let now = nodes_by_id(new);
    let mut summary = ChangeSummary::default();

    let mut files: BTreeMap<String, FileChange> = BTreeMap::new();
    if master.project != new.project {
        let change = match (&master.project, &new.project) {
            (None, Some(_)) => FileChange::Added,
            (Some(_), None) => FileChange::Removed,
            _ => FileChange::Modified,
        };
        files.insert(PROJECT_FILE.to_string(), change);
    }
    let old_files: BTreeMap<&str, &DefinitionNode> = old
        .values()
        .map(|node| (node.folder.as_str(), *node))
        .collect();
    let new_files: BTreeMap<&str, &DefinitionNode> = now
        .values()
        .map(|node| (node.folder.as_str(), *node))
        .collect();
    for (folder, node) in &old_files {
        match new_files.get(folder) {
            None => {
                files.insert(task_file(folder), FileChange::Removed);
            }
            Some(other) if other.definition != node.definition => {
                files.insert(task_file(folder), FileChange::Modified);
            }
            Some(_) => {}
        }
    }
    for folder in new_files.keys() {
        if !old_files.contains_key(folder) {
            files.insert(task_file(folder), FileChange::Added);
        }
    }
    summary.files = files.into_iter().collect();

    let mut old_edges = BTreeSet::new();
    let mut new_edges = BTreeSet::new();
    for (id, node) in &old {
        old_edges.extend(edges_of(node));
        match now.get(id) {
            None => summary.tasks_removed.push(id.to_string()),
            Some(other) => {
                let (a, b) = (&node.definition, &other.definition);
                if a != b || node.folder != other.folder {
                    summary.tasks_redefined.push(id.to_string());
                }
                if a.areas != b.areas {
                    summary.retagged.push(id.to_string());
                }
                if parent_folder(&node.folder) != parent_folder(&other.folder) {
                    summary.reparented.push(id.to_string());
                }
                if a.todos != b.todos {
                    summary.todos_changed.push(id.to_string());
                }
            }
        }
    }
    for (id, node) in &now {
        new_edges.extend(edges_of(node));
        if !old.contains_key(id) {
            summary.tasks_added.push(id.to_string());
        }
    }
    summary.edges_added = new_edges.difference(&old_edges).cloned().collect();
    summary.edges_removed = old_edges.difference(&new_edges).cloned().collect();
    let files = std::mem::take(&mut summary.files);
    summary.with_files(files)
}
