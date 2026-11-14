//! The handlers of the commands that change the shape of the tree: `task: new one off`, `split`,
//! `move`, `move to project` and `make one off`. The last four rename directories, so each runs
//! the model command once on the current snapshot to learn whether it is refused and what it
//! would do, shows that, and writes only after a confirmation (the default answer cancels).

use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{self as model, Context as WorkContext, NewTask};
use bongos_work_model::rules::{self, Place};
use bongos_work_model::store::{Task, WorkTree};
use gpui::{Context, Window};
use workspace::Workspace;

use crate::DEFAULT_ACTIVITY;
use crate::actions::task;
use crate::commands::{new_task_todos, ready, show_toast, submit, with_project, with_task};
use crate::modals;

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &task::NewOneOff, window, cx| new_one_off(w, window, cx))
        .register_action(|w, _: &task::Split, window, cx| split(w, window, cx))
        .register_action(|w, _: &task::Move, window, cx| move_task(w, window, cx))
        .register_action(|w, _: &task::MoveToProject, window, cx| move_to_project(w, window, cx))
        .register_action(|w, _: &task::MakeOneOff, window, cx| make_one_off(w, window, cx));
}

fn new_one_off(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    modals::ask(
        workspace,
        "Activity of the one-off, such as general or research",
        DEFAULT_ACTIVITY,
        false,
        Box::new(|activity, workspace, window, cx| {
            modals::ask(
                workspace,
                "Goal of the new one-off, in one sentence",
                "",
                false,
                Box::new(move |goal, workspace, window, cx| {
                    let new = NewTask {
                        goal,
                        todos: new_task_todos(cx),
                        ..NewTask::default()
                    };
                    submit(workspace, window, cx, move |tree, ctx| {
                        model::new_oneoff(tree, ctx, &activity, new.clone())
                    })
                }),
                window,
                cx,
            )
        }),
        window,
        cx,
    );
}

/// Runs `command` once on the current snapshot. A refusal is shown as a toast. Otherwise a
/// confirmation opens, naming `verb` and what the command would do; the default answer cancels.
/// Confirming queues `command` on the writer, which checks it again on the tree as it then is.
fn confirm_then_submit(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    verb: String,
    command: impl Fn(&WorkTree, &WorkContext) -> model::Outcome + Send + 'static,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let change = match command(
        &tree,
        &WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx)),
    ) {
        Ok(change) => change,
        Err(refusal) => {
            show_toast(workspace, refusal, cx);
            return;
        }
    };
    let yes = match &change.entry.detail {
        Some(detail) => format!("{verb}: {detail}"),
        None => verb.clone(),
    };
    modals::pick(
        workspace,
        format!("{verb}?"),
        vec![yes, "Cancel".to_string()],
        1,
        Box::new(move |choice, workspace, window, cx| {
            if choice == 0 {
                submit(workspace, window, cx, command);
            }
        }),
        window,
        cx,
    );
}

/// A toast and `true` when the task `id` is a project's top task, which cannot move.
fn refuse_project_top(workspace: &mut Workspace, id: &str, cx: &mut Context<Workspace>) -> bool {
    let Some(tree) = ready(workspace, cx) else {
        return true;
    };
    match Place::locate(&tree, id) {
        Ok(place) if place.is_project() => {
            let message = format!("{} is a project; only its tasks can move", place.slug());
            show_toast(workspace, message, cx);
            true
        }
        Ok(_) => false,
        Err(refusal) => {
            show_toast(workspace, refusal, cx);
            true
        }
    }
}

fn split(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Split which task?",
        "There is no task to split",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let refused = Place::locate(&tree, &id)
                .and_then(|place| rules::check_split(&place, &tree).map(|_| ()));
            if let Err(refusal) = refused {
                show_toast(workspace, refusal, cx);
                return;
            }
            modals::ask(
                workspace,
                "Goal of the new child; leave empty to reuse the goal of this task",
                "",
                true,
                Box::new(move |goal, workspace, window, cx| {
                    let goal = (!goal.is_empty()).then_some(goal);
                    confirm_then_submit(
                        workspace,
                        window,
                        cx,
                        "Split".to_string(),
                        move |tree, ctx| model::split(tree, ctx, &id, goal.as_deref()),
                    )
                }),
                window,
                cx,
            )
        }),
    );
}

fn move_task(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Move which task?",
        "There is no task to move",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            if refuse_project_top(workspace, &id, cx) {
                return;
            }
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            if tree.task_by_id(&id).is_none() {
                return;
            }
            let moved = id.clone();
            with_task(
                workspace,
                window,
                cx,
                "Move it under which task?",
                "There is no task to move it under",
                // Not under itself or one of its own tasks.
                move |task, tree| can_move_under(task, tree, &moved),
                Box::new(move |parent, workspace, window, cx| {
                    confirm_then_submit(
                        workspace,
                        window,
                        cx,
                        "Move".to_string(),
                        move |tree, ctx| model::move_task(tree, ctx, &id, &parent),
                    )
                }),
            )
        }),
    );
}

/// Whether the task `moved` may move under `task`: not under itself or one of its own tasks.
/// Decided through the tree, so it holds for a folder layout that does not nest tasks.
pub(crate) fn can_move_under(task: &Task, tree: &WorkTree, moved: &str) -> bool {
    task.file.is_some()
        && task
            .id()
            .is_some_and(|task_id| !tree.is_ancestor(moved, task_id))
}

fn move_to_project(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Move which task to a project?",
        "There is no task to move",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            if refuse_project_top(workspace, &id, cx) {
                return;
            }
            with_project(
                workspace,
                window,
                cx,
                "Move it to which project?",
                "There is no project",
                |_| true,
                Box::new(move |project, workspace, window, cx| {
                    confirm_then_submit(
                        workspace,
                        window,
                        cx,
                        "Move to project".to_string(),
                        move |tree, ctx| model::move_to_project(tree, ctx, &id, &project),
                    )
                }),
            )
        }),
    );
}

fn make_one_off(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Make which task a one-off?",
        "There is no task to make a one-off",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            if refuse_project_top(workspace, &id, cx) {
                return;
            }
            modals::ask(
                workspace,
                "Activity of the one-off, such as general or research",
                DEFAULT_ACTIVITY,
                false,
                Box::new(move |activity, workspace, window, cx| {
                    confirm_then_submit(
                        workspace,
                        window,
                        cx,
                        "Make one-off".to_string(),
                        move |tree, ctx| model::make_one_off(tree, ctx, &id, &activity),
                    )
                }),
                window,
                cx,
            )
        }),
    );
}
