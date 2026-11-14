//! The bounds of an amendment (D12): what an amending check may change in the working copy
//! before the landing engine commits it. Only `after`, `stacked_on`, `areas` and `size` in the
//! `task.toml` of an unstarted task the author's part adds or redefines, a bounded split of a
//! task the author's part adds ([`super::amendment_split`]), and the check's own stamp paths.
//! Everything else gives no verdict and nothing is committed.

use std::collections::BTreeSet;

use super::amendment_split::{SplitSources, check_splits};
use super::outcome::AmendOutcome;
use crate::definitions::{DefinitionTree, RelPath};
use crate::layout::{DEFINITIONS_DIR, PROJECT_FILE, TASK_FILE};
use crate::split::TaskDefinition;
use crate::summary::{ChangeSummary, nodes_by_id};

/// The start of the reason of an amendment that left its bounds.
pub const OUT_OF_BOUNDS: &str = "amendment outside its bounds";

/// The fields of an existing task an amendment may change.
const AMENDABLE_FIELDS: [&str; 4] = ["after", "stacked_on", "areas", "size"];

/// What the bounds are judged on.
pub struct AmendmentInput<'a> {
    /// The tree of the author's part (the last author commit).
    pub author: &'a DefinitionTree,
    /// The tree the amending check left in its writable checkout.
    pub writable: &'a DefinitionTree,
    /// The author's part: master's tree against `author`.
    pub author_part: &'a ChangeSummary,
    /// Every path (from the repository root) that differs between `author` and `writable`.
    pub changed_paths: &'a [String],
    /// The check's `stamp_paths()`, relative to `definitions/`.
    pub stamp_paths: &'a [RelPath],
    /// The tasks `LiveFacts::started` reports.
    pub started: &'a BTreeSet<String>,
}

/// `Ok` when the amendment stays inside its bounds; otherwise the reason, "amendment outside its
/// bounds: <path or field>".
pub fn amendment_bounds(input: &AmendmentInput) -> Result<(), String> {
    check(input).map_err(|what| format!("{OUT_OF_BOUNDS}: {what}"))
}

/// `outcome` when it is not `Amended` or the amendment is inside its bounds; otherwise the no
/// verdict that keeps the engine from committing it.
pub fn bound_amendment(outcome: AmendOutcome, input: &AmendmentInput) -> AmendOutcome {
    match outcome {
        AmendOutcome::Amended { .. } => match amendment_bounds(input) {
            Ok(()) => outcome,
            Err(reason) => AmendOutcome::no_verdict(reason),
        },
        other => other,
    }
}

/// The goal of a task a split produces: the names of the todos it holds, joined.
pub fn split_goal<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    names.into_iter().collect::<Vec<_>>().join("; ")
}

pub(super) fn task_file(folder: &str) -> String {
    if folder.is_empty() {
        TASK_FILE.to_string()
    } else {
        format!("{folder}/{TASK_FILE}")
    }
}

fn check(input: &AmendmentInput) -> Result<(), String> {
    let author = nodes_by_id(input.author);
    let writable = nodes_by_id(input.writable);

    let folders: BTreeSet<&str> = author
        .values()
        .chain(writable.values())
        .map(|node| node.folder.as_str())
        .collect();
    let prefix = format!("{DEFINITIONS_DIR}/");
    for path in input.changed_paths {
        let Some(relative) = path.strip_prefix(&prefix) else {
            return Err(path.clone());
        };
        if input
            .stamp_paths
            .iter()
            .any(|stamp| stamp.as_str() == relative)
        {
            continue;
        }
        let (folder, name) = relative.rsplit_once('/').unwrap_or(("", relative));
        if name == TASK_FILE && folders.contains(folder) {
            continue;
        }
        return Err(relative.to_string());
    }
    if input.author.project != input.writable.project {
        return Err(PROJECT_FILE.to_string());
    }
    for (id, node) in &author {
        if !writable.contains_key(id) {
            return Err(format!("{}: task removed", task_file(&node.folder)));
        }
    }

    let part: BTreeSet<&str> = input
        .author_part
        .tasks_added
        .iter()
        .chain(&input.author_part.tasks_redefined)
        .map(String::as_str)
        .collect();
    let editable = |id: &str| part.contains(id) && !input.started.contains(id);

    let sources: SplitSources = check_splits(input, &author, &writable)?;

    for (id, before) in &author {
        let after = writable[id];
        let file = task_file(&before.folder);
        if before.folder != after.folder {
            return Err(format!("{file}: folder"));
        }
        let changed = changed_fields(&before.definition, &after.definition);
        let Some(first) = changed.first() else {
            continue;
        };
        for field in &changed {
            let allowed = AMENDABLE_FIELDS.contains(&field.as_str())
                || (field == "todos" && sources.contains(*id));
            if !allowed {
                return Err(format!("{file}: {field}"));
            }
        }
        if !editable(id) {
            return Err(format!("{file}: {first}"));
        }
    }
    Ok(())
}

/// The names of the fields that differ between two definitions of a task; a key outside the
/// known fields is named by itself.
pub(super) fn changed_fields(a: &TaskDefinition, b: &TaskDefinition) -> Vec<String> {
    // Every field is named so that a new field fails to compile here until it is classified.
    let TaskDefinition {
        schema,
        id,
        goal,
        kind,
        created,
        repository,
        work_kind,
        areas,
        size,
        repositories,
        merge_is_ship,
        activity,
        after,
        stacked_on,
        todos,
        planned_result,
        extra,
    } = a;
    let mut changed = Vec::new();
    let mut note = |differs: bool, name: &str| {
        if differs {
            changed.push(name.to_string());
        }
    };
    note(*schema != b.schema, "schema");
    note(*id != b.id, "id");
    note(*goal != b.goal, "goal");
    note(*kind != b.kind, "kind");
    note(*created != b.created, "created");
    note(*repository != b.repository, "repository");
    note(*work_kind != b.work_kind, "work_kind");
    note(*areas != b.areas, "areas");
    note(*size != b.size, "size");
    note(*repositories != b.repositories, "repositories");
    note(*merge_is_ship != b.merge_is_ship, "merge_is_ship");
    note(*activity != b.activity, "activity");
    note(*after != b.after, "after");
    note(*stacked_on != b.stacked_on, "stacked_on");
    note(*todos != b.todos, "todos");
    note(*planned_result != b.planned_result, "planned_result");
    let keys: BTreeSet<&String> = extra.keys().chain(b.extra.keys()).collect();
    for key in keys {
        note(extra.get(key) != b.extra.get(key), key);
    }
    changed
}
