//! A split inside an amendment (D12): an unstarted task the author's part adds may hand its todos
//! to new child folders under it, or to one new sibling folder. Nothing is lost or invented: the
//! todo ids and each todo's text are preserved exactly. Each new task takes `kind`, `work_kind`,
//! `repository`, `repositories`, `merge_is_ship` and `checks` from the task it splits (`areas` and
//! `size` may differ, as for any task in the bounds), its goal is [`split_goal`] of its todos, and
//! its edges name only tasks of the same split (or, for the sibling, the split task).

use std::collections::{BTreeMap, BTreeSet};

use super::amendment::{AmendmentInput, changed_fields, split_goal, task_file};
use crate::definitions::DefinitionNode;
use crate::split::TodoDefinition;
use crate::summary::{EdgeKind, edges_of, parent_folder};

/// The ids of the tasks the amendment split.
pub(super) type SplitSources = BTreeSet<String>;

/// The fields a new task of a split takes from the task it splits.
const INHERITED_FIELDS: [&str; 6] = [
    "kind",
    "work_kind",
    "repository",
    "repositories",
    "merge_is_ship",
    "checks",
];
type Nodes<'a> = BTreeMap<&'a str, &'a DefinitionNode>;

pub(super) fn check_splits(
    input: &AmendmentInput,
    author: &Nodes,
    writable: &Nodes,
) -> Result<SplitSources, String> {
    // Source id -> the new tasks it produced.
    let mut products: BTreeMap<&str, Vec<&DefinitionNode>> = BTreeMap::new();
    for (id, node) in writable {
        if author.contains_key(id) {
            continue;
        }
        let source = source_of(node, author, writable).ok_or_else(|| {
            format!(
                "{}: a new task that is not a split",
                task_file(&node.folder)
            )
        })?;
        products.entry(source).or_default().push(node);
    }

    for (source, made) in &products {
        let before = author[source];
        let file = task_file(&before.folder);
        if !input.author_part.tasks_added.iter().any(|id| id == source)
            || input.started.contains(*source)
        {
            return Err(format!("{file}: split of a task the change did not add"));
        }
        let siblings = made
            .iter()
            .filter(|node| parent_folder(&node.folder) == parent_folder(&before.folder))
            .count();
        if siblings > 1 {
            return Err(format!("{file}: split into more than one sibling"));
        }
        check_todos(before, writable[source], made)?;
        let slugs: BTreeSet<&str> = made.iter().map(|node| node.slug.as_str()).collect();
        for node in made {
            check_product(before, node, &slugs)?;
        }
    }
    Ok(products.keys().map(|id| id.to_string()).collect())
}

/// The task a new task was split from: its parent (a child product) or a sibling (the sibling
/// product) that the author's tree has and whose todos the new task holds.
fn source_of<'a>(
    node: &DefinitionNode,
    author: &Nodes<'a>,
    writable: &Nodes<'a>,
) -> Option<&'a str> {
    let parent = parent_folder(&node.folder);
    let holds_its_todos = |candidate: &DefinitionNode| {
        !node.definition.todos.is_empty()
            && node.definition.todos.iter().all(|todo| {
                todo.id.is_some()
                    && candidate
                        .definition
                        .todos
                        .iter()
                        .any(|theirs| theirs.id == todo.id)
            })
    };
    let parent_node = writable
        .values()
        .find(|candidate| candidate.folder == parent && !node.folder.is_empty());
    let siblings = writable.values().filter(|candidate| {
        !candidate.folder.is_empty()
            && candidate.folder != node.folder
            && parent_folder(&candidate.folder) == parent
    });
    parent_node
        .into_iter()
        .chain(siblings)
        .filter_map(|candidate| author.get_key_value(candidate.definition.id.as_str()))
        .find(|(_, before)| holds_its_todos(before))
        .map(|(id, _)| *id)
}

/// The todos of the split task before equal, as a set by id with each todo's text, the todos it
/// keeps and the todos its new tasks hold.
fn check_todos(
    before: &DefinitionNode,
    after: &DefinitionNode,
    made: &[&DefinitionNode],
) -> Result<(), String> {
    let file = task_file(&before.folder);
    let mut original: BTreeMap<&str, &TodoDefinition> = BTreeMap::new();
    for todo in &before.definition.todos {
        let id = todo
            .id
            .as_deref()
            .ok_or_else(|| format!("{file}: a todo without an id cannot be split"))?;
        original.insert(id, todo);
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for node in std::iter::once(&after).chain(made) {
        let file = task_file(&node.folder);
        for todo in &node.definition.todos {
            let Some(id) = todo.id.as_deref() else {
                return Err(format!("{file}: todo {} was invented", todo.name));
            };
            match original.get(id) {
                None => return Err(format!("{file}: todo {id} was invented")),
                Some(theirs) if *theirs != todo => {
                    return Err(format!("{file}: todo {id} was altered"));
                }
                Some(_) => {}
            }
            if !seen.insert(id) {
                return Err(format!("{file}: todo {id} was copied"));
            }
        }
    }
    if let Some(dropped) = original.keys().find(|id| !seen.contains(*id)) {
        return Err(format!("{file}: todo {dropped} was dropped"));
    }
    Ok(())
}

/// A new task of a split, against the task it splits (as the author's tree has it).
fn check_product(
    source: &DefinitionNode,
    node: &DefinitionNode,
    slugs: &BTreeSet<&str>,
) -> Result<(), String> {
    let file = task_file(&node.folder);
    let (a, b) = (&source.definition, &node.definition);
    // The inherited fields, and `schema` and `created`, equal the split task's.
    let differing = changed_fields(a, b);
    let inherited = INHERITED_FIELDS
        .into_iter()
        .chain(["schema", "created"])
        .find(|field| differing.iter().any(|name| name.as_str() == *field));
    if let Some(field) = inherited {
        return Err(format!("{file}: {field}"));
    }
    // Any other field is absent.
    if b.activity.is_some() {
        return Err(format!("{file}: activity"));
    }
    if b.planned_result.is_some() {
        return Err(format!("{file}: planned_result"));
    }
    if let Some(key) = b.extra.keys().find(|key| key.as_str() != "checks") {
        return Err(format!("{file}: {key}"));
    }
    let names = node.definition.todos.iter().map(|todo| todo.name.as_str());
    if node.definition.goal != split_goal(names) {
        return Err(format!("{file}: goal"));
    }
    let sibling_of_source = parent_folder(&node.folder) == parent_folder(&source.folder);
    for edge in edges_of(node) {
        let to_split = slugs.contains(edge.target.as_str()) && edge.target != node.slug;
        let to_source = sibling_of_source && edge.target == source.slug;
        if !to_split && !to_source {
            let field = match edge.kind {
                EdgeKind::After => "after",
                EdgeKind::StackedOn => "stacked_on",
            };
            return Err(format!("{file}: {field}"));
        }
    }
    Ok(())
}
