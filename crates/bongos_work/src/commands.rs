//! The handlers of the `task` and `todo` actions that name a task or todo (D17), and hiding those
//! actions from the command palette while `work.enabled` is false (D18).
//!
//! Every handler first checks that the store is on and loaded, and otherwise shows a toast and
//! opens nothing. A handler then chains pickers and prompts to learn which task and todo, and
//! queues the model command on the store's serial writer. A refusal, from the model or the
//! writer, reaches a toast with the reason as it is.

use std::any::TypeId;
use std::sync::Arc;

use bongos_work_model::commands::{
    self as model, Context as WorkContext, NewTask, NewTodo, Placement, TaskEdit,
};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::graph::state_label;
use bongos_work_model::rules::{self, Place};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Kind, Status, TodoRole, TodoState};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::actions::{artifact, project, task, todo, work};
use crate::modals::{self, Then};
use crate::repositories::default_kind;
use crate::{WorkSettings, WorkStore, WriteError};
use crate::{
    artifacts, focus, links, modes, open, panel, projects, rebase, repositories, structure,
    worktrees,
};

/// The activity a task made with `task: new` goes under: it names no parent, and the Work view
/// that would supply "the group at point" does not exist yet.
pub const DEFAULT_ACTIVITY: &str = "general";

/// The id of the toast every refusal uses, so a newer one replaces an older.
pub struct WorkToast;

/// Every action type this module registers.
pub fn action_types() -> Vec<TypeId> {
    let mut types = vec![
        TypeId::of::<task::New>(),
        TypeId::of::<task::NewChild>(),
        TypeId::of::<task::Advance>(),
        TypeId::of::<task::AdvanceOverride>(),
        TypeId::of::<task::Close>(),
        TypeId::of::<task::DropTask>(),
        TypeId::of::<task::Reopen>(),
        TypeId::of::<task::Edit>(),
        TypeId::of::<todo::Add>(),
        TypeId::of::<todo::Remove>(),
        TypeId::of::<todo::Skip>(),
        TypeId::of::<todo::Reopen>(),
        TypeId::of::<todo::Move>(),
        TypeId::of::<todo::SetWorkflow>(),
        TypeId::of::<todo::SetRole>(),
        TypeId::of::<todo::Start>(),
        TypeId::of::<task::NewOneOff>(),
        TypeId::of::<task::AddEdge>(),
        TypeId::of::<task::RemoveEdge>(),
        TypeId::of::<task::ShowBlockers>(),
        TypeId::of::<task::SetUnblockPoint>(),
        TypeId::of::<task::SetMergeShip>(),
        TypeId::of::<task::Retarget>(),
        TypeId::of::<task::ClearRebase>(),
        TypeId::of::<task::SetResult>(),
        TypeId::of::<task::ClearResult>(),
        TypeId::of::<task::Split>(),
        TypeId::of::<task::Move>(),
        TypeId::of::<task::MoveToProject>(),
        TypeId::of::<task::MakeOneOff>(),
        TypeId::of::<task::Start>(),
        TypeId::of::<task::Focus>(),
        TypeId::of::<task::Unfocus>(),
        TypeId::of::<task::OpenWorktree>(),
        TypeId::of::<task::OpenPr>(),
        TypeId::of::<task::CopyPath>(),
        TypeId::of::<project::New>(),
        TypeId::of::<project::Archive>(),
        TypeId::of::<project::Unarchive>(),
        TypeId::of::<project::OpenFolder>(),
        TypeId::of::<project::Focus>(),
        TypeId::of::<work::AddRepository>(),
        TypeId::of::<task::SetDefaultVisibility>(),
        TypeId::of::<task::Think>(),
        TypeId::of::<task::Mode>(),
        TypeId::of::<artifact::Hide>(),
        TypeId::of::<artifact::Share>(),
        TypeId::of::<artifact::HideAll>(),
        TypeId::of::<artifact::ShareAll>(),
        TypeId::of::<artifact::Link>(),
        TypeId::of::<work::TogglePanel>(),
        TypeId::of::<work::Up>(),
        TypeId::of::<work::ToggleRow>(),
    ];
    #[cfg(debug_assertions)]
    {
        types.push(TypeId::of::<work::DumpSessionContext>());
        types.push(TypeId::of::<work::DumpView>());
    }
    types
}

/// Hides the actions from the command palette while `work.enabled` is false and shows them
/// while it is true. Call after `command_palette_hooks::init`.
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = WorkSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// Registers the handlers on a workspace.
pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &task::New, window, cx| new_task(w, window, cx))
        .register_action(|w, _: &task::NewChild, window, cx| new_child(w, window, cx))
        .register_action(|w, _: &task::Advance, window, cx| advance(w, false, window, cx))
        .register_action(|w, _: &task::AdvanceOverride, window, cx| advance(w, true, window, cx))
        .register_action(|w, _: &task::Close, window, cx| close(w, window, cx))
        .register_action(|w, _: &task::DropTask, window, cx| drop_task(w, window, cx))
        .register_action(|w, _: &task::Reopen, window, cx| reopen(w, window, cx))
        .register_action(|w, _: &task::Edit, window, cx| edit(w, window, cx))
        .register_action(|w, _: &todo::Add, window, cx| todo_add(w, window, cx))
        .register_action(|w, _: &todo::Remove, window, cx| todo_remove(w, window, cx))
        .register_action(|w, _: &todo::Skip, window, cx| todo_skip(w, window, cx))
        .register_action(|w, _: &todo::Reopen, window, cx| todo_reopen(w, window, cx))
        .register_action(|w, _: &todo::Move, window, cx| todo_move(w, window, cx))
        .register_action(|w, _: &todo::SetWorkflow, window, cx| todo_set_workflow(w, window, cx))
        .register_action(|w, _: &todo::SetRole, window, cx| todo_set_role(w, window, cx))
        .register_action(|w, _: &todo::Start, window, cx| todo_start(w, window, cx));
    links::register(workspace);
    structure::register(workspace);
    projects::register(workspace);
    repositories::register(workspace);
    worktrees::register(workspace);
    rebase::register(workspace);
    focus::register(workspace);
    open::register(workspace);
    artifacts::register(workspace);
    modes::register(workspace);
    panel::register(workspace);
}

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<WorkToast>(), message),
        cx,
    );
}

/// The tree, when the store is on and has loaded it; otherwise a toast says why.
pub(crate) fn ready(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Arc<WorkTree>> {
    let state = WorkStore::global(cx).map(|store| {
        let store = store.read(cx);
        (store.is_enabled(), store.snapshot())
    });
    match state {
        Some((true, Some(tree))) => Some(tree),
        Some((true, None)) => {
            show_toast(
                workspace,
                "The Work tree is still loading; try again in a moment".to_string(),
                cx,
            );
            None
        }
        _ => {
            show_toast(workspace, WriteError::Off.to_string(), cx);
            None
        }
    }
}

/// Queues `command` on the store's writer. `None`, after a toast, when there is no store.
pub(crate) fn queue(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    command: impl Fn(&WorkTree, &WorkContext) -> model::Outcome + Send + 'static,
) -> Option<gpui::Task<Result<model::Change, WriteError>>> {
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return None;
    };
    Some(store.update(cx, |store, cx| {
        store.write(WorkContext::kyle(), command, cx)
    }))
}

/// Queues `command` on the store's writer and shows why it did not happen, if it did not.
pub(crate) fn submit(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    command: impl Fn(&WorkTree, &WorkContext) -> model::Outcome + Send + 'static,
) {
    let Some(write) = queue(workspace, cx, command) else {
        return;
    };
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = write.await {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, error.to_string(), cx)
                })
                .ok();
        }
    })
    .detach();
}

/// Asks which project, among those that pass `keep`, then calls `then` with its slug. With none,
/// a toast says `none`.
pub(crate) fn with_project(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    placeholder: &'static str,
    none: &'static str,
    keep: impl Fn(&bongos_work_model::store::Project) -> bool,
    then: Then<String>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let (slugs, labels): (Vec<String>, Vec<String>) = tree
        .projects
        .iter()
        .filter(|project| keep(project))
        .map(|project| {
            let title = project
                .file
                .as_ref()
                .map_or(project.slug.as_str(), |file| file.title.as_str());
            let archived = project.file.as_ref().is_some_and(|file| file.archived);
            let label = format!(
                "{title} ({}){}",
                project.slug,
                if archived { ", archived" } else { "" }
            );
            (project.slug.clone(), label)
        })
        .unzip();
    if slugs.is_empty() {
        show_toast(workspace, none.to_string(), cx);
        return;
    }
    modals::pick(
        workspace,
        placeholder,
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            then(slugs[index].clone(), workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// One line per task in the tree that passes `keep`: its goal under its groups' goals, and its
/// state.
pub(crate) fn task_choices(
    tree: &WorkTree,
    keep: impl Fn(&Task, &WorkTree) -> bool,
) -> Vec<(String, String)> {
    tree.tasks()
        .into_iter()
        .filter(|task| keep(task, tree))
        .filter_map(|task| {
            let id = task.id()?.to_string();
            let mut path: Vec<&str> = Place::locate(tree, &id)
                .map(|place| {
                    place
                        .ancestors
                        .iter()
                        .rev()
                        .map(|task| task.goal())
                        .collect()
                })
                .unwrap_or_default();
            path.push(task.goal());
            let label = format!(
                "{}  [{}]",
                path.join(" / "),
                state_label(derive::state(task, tree))
            );
            Some((id, label))
        })
        .collect()
}

/// Asks which task, among those that pass `keep`, then calls `then` with its id. With none, a
/// toast says `none`.
pub(crate) fn with_task(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    placeholder: &'static str,
    none: &'static str,
    keep: impl Fn(&Task, &WorkTree) -> bool,
    then: Then<String>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let (ids, labels): (Vec<String>, Vec<String>) = task_choices(&tree, keep).into_iter().unzip();
    if ids.is_empty() {
        show_toast(workspace, none.to_string(), cx);
        return;
    }
    modals::pick(
        workspace,
        placeholder,
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            then(ids[index].clone(), workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// Asks which todo of the task `id`, among those that pass `keep`, then calls `then` with its
/// index in the list.
fn with_todo(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    id: &str,
    placeholder: &'static str,
    keep: impl Fn(usize, &bongos_work_model::Todo) -> bool,
    then: Then<usize>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let Some(file) = tree.task_by_id(id).and_then(|task| task.file.as_ref()) else {
        show_toast(workspace, format!("no task has the id {id}"), cx);
        return;
    };
    let (indexes, labels): (Vec<usize>, Vec<String>) = file
        .todos
        .iter()
        .enumerate()
        .filter(|(index, todo)| keep(*index, todo))
        .map(|(index, todo)| (index, todo_label(index, todo)))
        .unzip();
    if indexes.is_empty() {
        show_toast(workspace, "That task has no such todo".to_string(), cx);
        return;
    }
    modals::pick(
        workspace,
        placeholder,
        labels,
        0,
        Box::new(move |choice, workspace, window, cx| then(indexes[choice], workspace, window, cx)),
        window,
        cx,
    );
}

fn todo_label(index: usize, todo: &bongos_work_model::Todo) -> String {
    let state = match todo.state {
        TodoState::Open if todo.started.is_some() => "started",
        TodoState::Open => "open",
        TodoState::Done => "done",
        TodoState::Skipped => "skipped",
        TodoState::Cancelled => "cancelled",
    };
    let role = todo.role.map_or(String::new(), |role| {
        format!(", {}", rules::role_name(role))
    });
    let workflow = todo
        .workflow
        .as_ref()
        .map_or(String::new(), |workflow| format!(", workflow {workflow}"));
    format!("{}. {} ({state}{role}{workflow})", index + 1, todo.name)
}

pub(crate) fn is_open_leaf(task: &Task, tree: &WorkTree) -> bool {
    task.file
        .as_ref()
        .is_some_and(|file| file.status == Status::Open)
        && derive::shape(task, tree) == Shape::Leaf
}

fn is_leaf_with(
    task: &Task,
    tree: &WorkTree,
    keep: impl Fn(&bongos_work_model::Todo) -> bool,
) -> bool {
    derive::shape(task, tree) == Shape::Leaf
        && task
            .file
            .as_ref()
            .is_some_and(|file| file.todos.iter().any(keep))
}

pub(crate) fn has_status(task: &Task, keep: impl Fn(Status) -> bool) -> bool {
    task.file.as_ref().is_some_and(|file| keep(file.status))
}

// Tasks.

/// The request for a new task with `goal` at `placement`. Its kind is the model's default,
/// except `pr-merge` in a repository without deployments (see
/// [`default_kind`](crate::repositories::default_kind)).
pub(crate) fn new_task_with_default_kind(placement: &Placement, goal: String, cx: &App) -> NewTask {
    let mut new = NewTask {
        goal,
        ..NewTask::default()
    };
    if let Some(tree) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot()) {
        let repositories = &WorkSettings::get_global(cx).repositories;
        new.kind = default_kind(&tree, placement, &new, repositories);
    }
    new
}

fn new_task(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    modals::ask(
        workspace,
        "Goal of the new task, in one sentence",
        "",
        false,
        Box::new(|goal, workspace, _, cx| {
            let placement = Placement::OneOff {
                activity: DEFAULT_ACTIVITY.to_string(),
            };
            let new = new_task_with_default_kind(&placement, goal, cx);
            submit(workspace, cx, move |tree, ctx| {
                model::new_task(tree, ctx, placement.clone(), new.clone())
            })
        }),
        window,
        cx,
    );
}

fn new_child(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "New task under which task?",
        "There is no open task to add a child to",
        |task, _| has_status(task, |status| status == Status::Open),
        Box::new(|parent, workspace, window, cx| {
            modals::ask(
                workspace,
                "Goal of the new task, in one sentence",
                "",
                false,
                Box::new(move |goal, workspace, _, cx| {
                    let new =
                        new_task_with_default_kind(&Placement::Under(parent.clone()), goal, cx);
                    submit(workspace, cx, move |tree, ctx| {
                        model::new_child(tree, ctx, &parent, new.clone())
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

fn advance(
    workspace: &mut Workspace,
    override_ship: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (placeholder, none) = if override_ship {
        (
            "Ship which task before the task it is stacked on is Done?",
            "No task is waiting on the task it is stacked on",
        )
    } else {
        (
            "Advance which task?",
            "There is no open task with an open todo",
        )
    };
    with_task(
        workspace,
        window,
        cx,
        placeholder,
        none,
        move |task, tree| {
            if !is_open_leaf(task, tree) || derive::current_todo(task).is_none() {
                return false;
            }
            !override_ship || ship_waits(task, tree)
        },
        Box::new(move |id, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::advance(tree, ctx, &id, override_ship)
            })
        }),
    );
}

/// Whether the current todo of `task` is a ships todo that waits for the task it is stacked on.
fn ship_waits(task: &Task, tree: &WorkTree) -> bool {
    let (Some(id), Some(file)) = (task.id(), task.file.as_ref()) else {
        return false;
    };
    let Some(index) = file
        .todos
        .iter()
        .position(|todo| todo.state == TodoState::Open)
    else {
        return false;
    };
    Place::locate(tree, id)
        .is_ok_and(|place| rules::ship_waits_on(&place, tree, file, index).is_some())
}

fn close(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Close which task?",
        "There is no open task to close",
        |task, _| has_status(task, |status| status == Status::Open),
        Box::new(|id, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| model::close(tree, ctx, &id))
        }),
    );
}

fn drop_task(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Drop which task?",
        "There is no task to drop",
        |task, _| has_status(task, |status| status != Status::Cancelled),
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let open: Vec<String> = tree
                .task_by_id(&id)
                .map(|task| {
                    rules::open_descendants(task, &tree)
                        .iter()
                        .map(|task| task.slug.clone())
                        .collect()
                })
                .unwrap_or_default();
            if open.is_empty() {
                submit(workspace, cx, move |tree, ctx| {
                    model::drop_task(tree, ctx, &id, false)
                });
                return;
            }
            let slug = tree
                .task_by_id(&id)
                .map_or_else(String::new, |task| task.slug.clone());
            modals::pick(
                workspace,
                format!("Dropping {slug} also cancels {} open tasks", open.len()),
                vec![
                    format!("Drop {slug} and cancel {}", open.join(", ")),
                    "Keep them".to_string(),
                ],
                1,
                Box::new(move |choice, workspace, _, cx| {
                    if choice == 0 {
                        submit(workspace, cx, move |tree, ctx| {
                            model::drop_task(tree, ctx, &id, true)
                        });
                    }
                }),
                window,
                cx,
            );
        }),
    );
}

fn reopen(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Reopen which task?",
        "There is no closed or cancelled task to reopen",
        |task, _| has_status(task, |status| status != Status::Open),
        Box::new(|id, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::reopen(tree, ctx, &id)
            })
        }),
    );
}

const KINDS: [Kind; 6] = [
    Kind::PrShip,
    Kind::PrMerge,
    Kind::Document,
    Kind::Spike,
    Kind::Babysit,
    Kind::Other,
];

fn edit(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Edit which task?",
        "There is no task to edit",
        |_, _| true,
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let Some(file) = tree.task_by_id(&id).and_then(|task| task.file.clone()) else {
                return;
            };
            let current = file.kind;
            modals::ask(
                workspace,
                "Goal, in one sentence",
                &file.goal,
                false,
                Box::new(move |goal, workspace, window, cx| {
                    let labels = KINDS
                        .iter()
                        .map(|kind| {
                            if *kind == current {
                                format!("{} (current)", model::kind_name(*kind))
                            } else {
                                model::kind_name(*kind).to_string()
                            }
                        })
                        .collect();
                    let selected = KINDS.iter().position(|kind| *kind == current).unwrap_or(0);
                    modals::pick(
                        workspace,
                        "Kind",
                        labels,
                        selected,
                        Box::new(move |choice, workspace, _, cx| {
                            let kind = KINDS[choice];
                            submit(workspace, cx, move |tree, ctx| {
                                model::edit(
                                    tree,
                                    ctx,
                                    &id,
                                    TaskEdit {
                                        goal: Some(goal.clone()),
                                        kind: Some(kind),
                                        repository: None,
                                    },
                                )
                            })
                        }),
                        window,
                        cx,
                    )
                }),
                window,
                cx,
            )
        }),
    );
}

// Todos.

fn todo_add(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Add a todo to which task?",
        "There is no leaf task to add a todo to",
        |task, tree| derive::shape(task, tree) == Shape::Leaf && task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            modals::ask(
                workspace,
                "Name of the new todo",
                "",
                false,
                Box::new(move |name, workspace, _, cx| {
                    submit(workspace, cx, move |tree, ctx| {
                        model::todo_add(
                            tree,
                            ctx,
                            &id,
                            NewTodo {
                                name: name.clone(),
                                ..NewTodo::default()
                            },
                            None,
                        )
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

/// Picks a task with a todo that passes `keep_todo`, then that todo, then calls `then`.
fn with_task_and_todo(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    (task_placeholder, todo_placeholder, none): (&'static str, &'static str, &'static str),
    keep_task: fn(&Task, &WorkTree) -> bool,
    keep_todo: fn(usize, &bongos_work_model::Todo) -> bool,
    then: impl FnOnce(String, usize, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    with_task(
        workspace,
        window,
        cx,
        task_placeholder,
        none,
        keep_task,
        Box::new(move |id, workspace, window, cx| {
            let task = id.clone();
            with_todo(
                workspace,
                window,
                cx,
                &task,
                todo_placeholder,
                keep_todo,
                Box::new(move |index, workspace, window, cx| {
                    then(id, index, workspace, window, cx)
                }),
            )
        }),
    );
}

fn todo_remove(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Remove a todo from which task?",
            "Remove which todo?",
            "There is no leaf task with a todo",
        ),
        |task, tree| is_leaf_with(task, tree, |_| true),
        |_, _| true,
        |id, index, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::todo_remove(tree, ctx, &id, index)
            })
        },
    );
}

fn todo_skip(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Skip a todo of which task?",
            "Skip which todo?",
            "There is no open leaf task with an open todo",
        ),
        |task, tree| is_open_leaf(task, tree) && is_leaf_with(task, tree, is_open),
        |_, todo| is_open(todo),
        |id, index, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::todo_skip(tree, ctx, &id, Some(index))
            })
        },
    );
}

fn is_open(todo: &bongos_work_model::Todo) -> bool {
    todo.state == TodoState::Open
}

fn todo_reopen(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Reopen a todo of which task?",
            "Reopen which todo?",
            "No leaf task has a todo that is done or skipped",
        ),
        |task, tree| is_leaf_with(task, tree, |todo| !is_open(todo)),
        |_, todo| !is_open(todo),
        |id, index, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::todo_reopen(tree, ctx, &id, index)
            })
        },
    );
}

fn todo_move(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Move a todo of which task?",
            "Move which todo?",
            "There is no leaf task with a todo",
        ),
        |task, tree| is_leaf_with(task, tree, |_| true),
        |_, _| true,
        |id, from, workspace, window, cx| {
            with_todo(
                workspace,
                window,
                cx,
                &id.clone(),
                "Move it to the place of which todo?",
                |_, _| true,
                Box::new(move |to, workspace, _, cx| {
                    submit(workspace, cx, move |tree, ctx| {
                        model::todo_move(tree, ctx, &id, from, to)
                    })
                }),
            )
        },
    );
}

fn todo_set_workflow(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Set the workflow of a todo of which task?",
            "Set the workflow of which todo?",
            "There is no leaf task with a todo",
        ),
        |task, tree| is_leaf_with(task, tree, |_| true),
        |_, _| true,
        |id, index, workspace, window, cx| {
            let current = ready(workspace, cx)
                .and_then(|tree| {
                    tree.task_by_id(&id)?
                        .file
                        .as_ref()?
                        .todos
                        .get(index)?
                        .workflow
                        .clone()
                })
                .unwrap_or_default();
            modals::ask(
                workspace,
                "Workflow name; leave empty to make the todo manual",
                &current,
                true,
                Box::new(move |workflow, workspace, _, cx| {
                    let workflow = (!workflow.is_empty()).then_some(workflow);
                    submit(workspace, cx, move |tree, ctx| {
                        model::todo_set_workflow(tree, ctx, &id, index, workflow.clone())
                    })
                }),
                window,
                cx,
            )
        },
    );
}

fn todo_set_role(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    const ROLES: [Option<TodoRole>; 3] =
        [Some(TodoRole::ProducesBranch), Some(TodoRole::Ships), None];
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Set the role of a todo of which task?",
            "Set the role of which todo?",
            "There is no leaf task with a todo",
        ),
        |task, tree| is_leaf_with(task, tree, |_| true),
        |_, _| true,
        |id, index, workspace, window, cx| {
            let labels = ROLES
                .iter()
                .map(|role| role.map_or("none", rules::role_name).to_string())
                .collect();
            modals::pick(
                workspace,
                "Role",
                labels,
                0,
                Box::new(move |choice, workspace, _, cx| {
                    let role = ROLES[choice];
                    submit(workspace, cx, move |tree, ctx| {
                        model::todo_set_role(tree, ctx, &id, index, role)
                    })
                }),
                window,
                cx,
            )
        },
    );
}

fn todo_start(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task_and_todo(
        workspace,
        window,
        cx,
        (
            "Start a todo of which task?",
            "Start which todo?",
            "There is no open leaf task with a todo that has not started",
        ),
        |task, tree| {
            is_open_leaf(task, tree)
                && is_leaf_with(task, tree, |todo| is_open(todo) && todo.started.is_none())
        },
        |_, todo| is_open(todo) && todo.started.is_none(),
        |id, index, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::todo_start(tree, ctx, &id, Some(index))
            })
        },
    );
}
