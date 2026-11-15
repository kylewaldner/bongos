//! Check 2, `no-state`: a tree carries no state. No reserved state key in a `task.toml`, no path
//! under the state or files part of a project folder, no file named like the state store.

use crate::layout::{DEFINITIONS_DIR, FILES_DIR, STATE_DB_FILE, STATE_DIR};
use crate::split::RESERVED_STATE_KEYS;

use super::outcome::CheckOutcome;

/// Check 2 over a tree: `paths` are every path of the tree, relative to the repository root, and
/// `task_files` the `(path, text)` of its `task.toml` files.
pub fn no_state<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    task_files: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> CheckOutcome {
    let mut refusals = Vec::new();
    for path in paths {
        let in_definitions = path
            .strip_prefix(DEFINITIONS_DIR)
            .and_then(|rest| rest.strip_prefix('/'));
        let top = in_definitions.unwrap_or(path);
        let first = top.split('/').next().unwrap_or_default();
        if top.contains('/') && (first == STATE_DIR || first == FILES_DIR) {
            refusals.push(format!("{path}: the {first} part never lands"));
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == STATE_DB_FILE || name.starts_with(&format!("{STATE_DB_FILE}-")) {
            refusals.push(format!("{path}: a state store never lands"));
        }
    }
    for (path, text) in task_files {
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        let todos: Vec<&toml::Table> = table
            .get("todos")
            .and_then(|todos| todos.as_array())
            .map(|todos| todos.iter().filter_map(|todo| todo.as_table()).collect())
            .unwrap_or_default();
        let held: Vec<&str> = RESERVED_STATE_KEYS
            .into_iter()
            .filter(|key| match key.strip_prefix("todo.") {
                Some(todo_key) => todos.iter().any(|todo| todo.contains_key(todo_key)),
                None => table.contains_key(*key),
            })
            .collect();
        if !held.is_empty() {
            refusals.push(format!(
                "{path}: holds state, which belongs to the state store: {}",
                held.join(", ")
            ));
        }
    }
    CheckOutcome::from_refusals(refusals)
}
