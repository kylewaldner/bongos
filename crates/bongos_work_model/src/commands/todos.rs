//! `task: advance` and the `todo:` commands. A todo is named by its index in the list; `None`
//! means the current todo, the first one still open.

use super::{Context, Outcome, edit_task, require_leaf, require_open};
use crate::graph::state_label;
use crate::rules::{self, Place, Refusal};
use crate::store::WorkTree;
use crate::types::{Extra, ShipOverridden, TaskFile, Todo, TodoRole, TodoState};

/// A todo for `todo: add`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewTodo {
    pub name: String,
    pub role: Option<TodoRole>,
    pub workflow: Option<String>,
}

/// The index of the todo `index` names, or of the current todo when it is `None`.
fn todo_index(place: &Place, file: &TaskFile, index: Option<usize>) -> Result<usize, Refusal> {
    match index {
        Some(index) if index < file.todos.len() => Ok(index),
        Some(index) => Err(format!("{} has no todo {}", place.slug(), index + 1)),
        None => file
            .todos
            .iter()
            .position(|todo| todo.state == TodoState::Open)
            .ok_or_else(|| format!("{} has no open todo", place.slug())),
    }
}

fn require_todo_open(todo: &Todo) -> Result<(), Refusal> {
    if todo.state == TodoState::Open {
        Ok(())
    } else {
        Err(format!("todo {} is not open", todo.name))
    }
}

/// `task: advance`: complete the current todo. The ships todo of a task stacked on a sibling
/// that is not Done is refused unless `override_ship` (`task: advance override`), which completes
/// it and writes the `ship_overridden` marker with the parent's state.
pub fn advance(tree: &WorkTree, ctx: &Context, id: &str, override_ship: bool) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        require_open(place.slug(), file)?;
        let index = todo_index(place, file, None)?;
        let mut event = "todo done";
        if let Some((parent, state)) = rules::ship_waits_on(place, tree, file, index) {
            if !override_ship {
                return Err(rules::ship_refusal(place, parent, state));
            }
            file.ship_overridden = Some(ShipOverridden {
                at: ctx.at,
                parent_state: state_label(state).to_lowercase(),
                extra: Extra::new(),
            });
            event = "todo done, ship overridden";
        }
        let todo = &mut file.todos[index];
        todo.state = TodoState::Done;
        Ok((event, Some(todo.name.clone())))
    })
}

/// `todo: skip`: close an open todo without doing it. Skipping the ships todo of a task stacked
/// on a sibling that is not Done is refused like shipping it, since a skipped ships todo ends
/// the task just the same.
pub fn todo_skip(tree: &WorkTree, ctx: &Context, id: &str, index: Option<usize>) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let index = todo_index(place, file, index)?;
        require_todo_open(&file.todos[index])?;
        if let Some((parent, state)) = rules::ship_waits_on(place, tree, file, index) {
            return Err(rules::ship_refusal(place, parent, state));
        }
        let todo = &mut file.todos[index];
        todo.state = TodoState::Skipped;
        Ok(("todo skipped", Some(todo.name.clone())))
    })
}

/// `todo: reopen`: open a done, skipped or cancelled todo again.
pub fn todo_reopen(tree: &WorkTree, ctx: &Context, id: &str, index: usize) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let index = todo_index(place, file, Some(index))?;
        let todo = &mut file.todos[index];
        if todo.state == TodoState::Open {
            return Err(format!("todo {} is already open", todo.name));
        }
        todo.state = TodoState::Open;
        Ok(("todo reopened", Some(todo.name.clone())))
    })
}

/// `todo: add`: a new open todo at `position`, or at the end.
pub fn todo_add(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    new: NewTodo,
    position: Option<usize>,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let name = new.name.trim();
        if name.is_empty() {
            return Err("a todo needs a name".to_string());
        }
        if let Some(role) = new.role {
            rules::check_role_free(place.slug(), &file.todos, None, role)?;
        }
        let position = position.unwrap_or(file.todos.len());
        if position > file.todos.len() {
            return Err(format!(
                "{} has {} todos; a new one goes at 1 to {}",
                place.slug(),
                file.todos.len(),
                file.todos.len() + 1
            ));
        }
        file.todos.insert(
            position,
            Todo {
                name: name.to_string(),
                role: new.role,
                workflow: new.workflow.clone(),
                state: TodoState::Open,
                started: None,
                extra: Extra::new(),
            },
        );
        Ok(("todo added", Some(name.to_string())))
    })
}

/// `todo: remove`. A task in a stack keeps its produces-branch and ships todos.
pub fn todo_remove(tree: &WorkTree, ctx: &Context, id: &str, index: usize) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let index = todo_index(place, file, Some(index))?;
        let before = file.clone();
        let removed = file.todos.remove(index);
        rules::check_stays_stackable(place, &before, file)?;
        Ok(("todo removed", Some(removed.name)))
    })
}

/// `todo: move`: the todo at `from` goes to position `to`.
pub fn todo_move(tree: &WorkTree, ctx: &Context, id: &str, from: usize, to: usize) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let from = todo_index(place, file, Some(from))?;
        let to = todo_index(place, file, Some(to))?;
        if from == to {
            return Err(format!("todo {} is already there", file.todos[from].name));
        }
        let todo = file.todos.remove(from);
        let detail = format!("{} to {}", todo.name, to + 1);
        file.todos.insert(to, todo);
        Ok(("todo moved", Some(detail)))
    })
}

/// `todo: set workflow`: the workflow a run of this todo starts; `None` makes it manual.
pub fn todo_set_workflow(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    index: usize,
    workflow: Option<String>,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let index = todo_index(place, file, Some(index))?;
        let todo = &mut file.todos[index];
        if todo.workflow == workflow {
            return Err(format!("todo {} already has that workflow", todo.name));
        }
        todo.workflow = workflow.clone();
        let detail = format!("{}: {}", todo.name, workflow.as_deref().unwrap_or("manual"));
        Ok(("todo workflow set", Some(detail)))
    })
}

/// `todo: set role`: `produces-branch`, `ships` or none. A task has at most one todo of each role,
/// and a task in a stack keeps both.
pub fn todo_set_role(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    index: usize,
    role: Option<TodoRole>,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        let index = todo_index(place, file, Some(index))?;
        if file.todos[index].role == role {
            return Err(format!(
                "todo {} already has that role",
                file.todos[index].name
            ));
        }
        if let Some(role) = role {
            rules::check_role_free(place.slug(), &file.todos, Some(index), role)?;
        }
        let before = file.clone();
        let todo = &mut file.todos[index];
        todo.role = role;
        let detail = format!("{}: {}", todo.name, role.map_or("none", rules::role_name));
        rules::check_stays_stackable(place, &before, file)?;
        Ok(("todo role set", Some(detail)))
    })
}

/// `todo: start`: mark an open todo as begun, which is how review starts on a PR task.
pub fn todo_start(tree: &WorkTree, ctx: &Context, id: &str, index: Option<usize>) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        require_leaf(place, tree, "todos")?;
        require_open(place.slug(), file)?;
        let index = todo_index(place, file, index)?;
        let todo = &mut file.todos[index];
        require_todo_open(todo)?;
        if todo.started.is_some() {
            return Err(format!("todo {} has already started", todo.name));
        }
        todo.started = Some(ctx.at);
        Ok(("todo started", Some(todo.name.clone())))
    })
}
