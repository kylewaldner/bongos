//! The handlers of the edge and result commands: `task: add edge`, `remove edge`, `show blockers`,
//! `set unblock point`, `set merge ship`, `retarget`, `set result` and `clear result`. They chain
//! pickers and prompts like the commands in `commands`, and a refusal reaches a toast.

use bongos_work_model::commands::{self as model, Retarget};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::graph::state_label;
use bongos_work_model::rules::{EdgeKind, Place};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Extra, ResultKind, ResultState, TaskResult, UnblockPoint};
use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::task;
use crate::commands::{ready, show_toast, submit, with_task};
use crate::modals::{self, Then};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &task::AddEdge, window, cx| add_edge(w, window, cx))
        .register_action(|w, _: &task::RemoveEdge, window, cx| remove_edge(w, window, cx))
        .register_action(|w, _: &task::ShowBlockers, window, cx| show_blockers(w, window, cx))
        .register_action(|w, _: &task::SetUnblockPoint, window, cx| {
            set_unblock_point(w, window, cx)
        })
        .register_action(|w, _: &task::SetMergeShip, window, cx| set_merge_ship(w, window, cx))
        .register_action(|w, _: &task::Retarget, window, cx| retarget(w, window, cx))
        .register_action(|w, _: &task::SetResult, window, cx| set_result(w, window, cx))
        .register_action(|w, _: &task::ClearResult, window, cx| clear_result(w, window, cx));
}

const UNBLOCK_POINTS: [UnblockPoint; 3] = [
    UnblockPoint::Review,
    UnblockPoint::Implemented,
    UnblockPoint::Merged,
];

/// The siblings of the task `id`: the tasks sharing its parent, not itself.
fn siblings_of<'a>(tree: &'a WorkTree, id: &str) -> Vec<&'a Task> {
    Place::locate(tree, id)
        .map(|place| {
            place
                .siblings
                .iter()
                .filter(|task| task.dir != place.task.dir)
                .collect()
        })
        .unwrap_or_default()
}

fn has_siblings(task: &Task, tree: &WorkTree) -> bool {
    task.file.is_some()
        && task
            .id()
            .is_some_and(|id| !siblings_of(tree, id).is_empty())
}

fn has_edge(task: &Task, _: &WorkTree) -> bool {
    task.file
        .as_ref()
        .is_some_and(|file| !file.after.is_empty() || file.stacked_on.is_some())
}

fn is_leaf(task: &Task, tree: &WorkTree) -> bool {
    task.file.is_some() && derive::shape(task, tree) == Shape::Leaf
}

/// Asks which sibling of the task `id`, then calls `then` with its id.
fn with_sibling(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    id: &str,
    placeholder: &'static str,
    then: Then<String>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let (ids, labels): (Vec<String>, Vec<String>) = siblings_of(&tree, id)
        .into_iter()
        .filter_map(|sibling| {
            let label = format!(
                "{} ({})  [{}]",
                sibling.named_goal(),
                sibling.slug,
                state_label(derive::state(sibling, &tree))
            );
            Some((sibling.id()?.to_string(), label))
        })
        .unzip();
    if ids.is_empty() {
        show_toast(workspace, "That task has no siblings".to_string(), cx);
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

/// Asks which edge of the task `id`, then calls `then` with the slug of the sibling it names.
fn with_edge(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    id: &str,
    placeholder: &'static str,
    then: Then<String>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let mut slugs = Vec::new();
    let mut labels = Vec::new();
    if let Some(file) = tree.task_by_id(id).and_then(|task| task.file.as_ref()) {
        for edge in &file.after {
            slugs.push(edge.task.clone());
            labels.push(format!(
                "after {} ({})",
                edge.task,
                model::unblock_name(edge.unblock)
            ));
        }
        if let Some(edge) = &file.stacked_on {
            slugs.push(edge.task.clone());
            labels.push(format!(
                "stacked on {} ({})",
                edge.task,
                model::unblock_name(edge.unblock)
            ));
        }
    }
    if slugs.is_empty() {
        show_toast(workspace, "That task has no edges".to_string(), cx);
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

/// Asks for the unblock point, starting on `default`, then calls `then`.
fn with_unblock_point(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    default: Option<UnblockPoint>,
    then: Then<UnblockPoint>,
) {
    let labels = UNBLOCK_POINTS
        .iter()
        .map(|point| {
            if Some(*point) == default {
                format!("{} (default)", model::unblock_name(*point))
            } else {
                model::unblock_name(*point).to_string()
            }
        })
        .collect();
    let selected = UNBLOCK_POINTS
        .iter()
        .position(|point| Some(*point) == default)
        .unwrap_or(0);
    modals::pick(
        workspace,
        "Unblocks when the other task reaches",
        labels,
        selected,
        Box::new(move |index, workspace, window, cx| {
            then(UNBLOCK_POINTS[index], workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn add_edge(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Add an edge to which task?",
        "No task has a sibling to depend on",
        has_siblings,
        Box::new(|id, workspace, window, cx| {
            let task = id.clone();
            with_sibling(
                workspace,
                window,
                cx,
                &task,
                "Depend on which task?",
                Box::new(move |target, workspace, window, cx| {
                    modals::pick(
                        workspace,
                        "Kind of edge",
                        vec!["after".to_string(), "stacked on".to_string()],
                        0,
                        Box::new(move |choice, workspace, window, cx| {
                            let kind = [EdgeKind::After, EdgeKind::StackedOn][choice];
                            with_unblock_point(
                                workspace,
                                window,
                                cx,
                                Some(model::default_unblock(kind)),
                                Box::new(move |point, workspace, _, cx| {
                                    submit(workspace, cx, move |tree, ctx| {
                                        model::add_edge(tree, ctx, &id, &target, kind, Some(point))
                                    })
                                }),
                            )
                        }),
                        window,
                        cx,
                    )
                }),
            )
        }),
    );
}

fn remove_edge(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Remove an edge of which task?",
        "No task has an edge",
        has_edge,
        Box::new(|id, workspace, window, cx| {
            let task = id.clone();
            with_edge(
                workspace,
                window,
                cx,
                &task,
                "Remove which edge?",
                Box::new(move |target, workspace, _, cx| {
                    submit(workspace, cx, move |tree, ctx| {
                        model::remove_edge(tree, ctx, &id, &target)
                    })
                }),
            )
        }),
    );
}

/// What holds the task `id` back, as one sentence.
pub fn blockers_message(tree: &WorkTree, id: &str) -> String {
    let Some(task) = tree.task_by_id(id) else {
        return format!("no task has the id {id}");
    };
    let blockers = derive::blocking_targets(task, tree);
    if blockers.is_empty() {
        return format!("{} is not blocked by any edge", task.slug);
    }
    let names: Vec<String> = blockers
        .iter()
        .map(|blocker| {
            format!(
                "{} ({})",
                blocker.slug,
                state_label(derive::state(blocker, tree))
            )
        })
        .collect();
    format!("{} is blocked by {}", task.slug, names.join(", "))
}

fn show_blockers(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Show what blocks which task?",
        "There is no task to show",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, _, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let message = blockers_message(&tree, &id);
            show_toast(workspace, message, cx);
        }),
    );
}

fn set_unblock_point(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Set the unblock point of an edge of which task?",
        "No task has an edge",
        has_edge,
        Box::new(|id, workspace, window, cx| {
            let task = id.clone();
            with_edge(
                workspace,
                window,
                cx,
                &task,
                "Set the unblock point of which edge?",
                Box::new(move |target, workspace, window, cx| {
                    with_unblock_point(
                        workspace,
                        window,
                        cx,
                        None,
                        Box::new(move |point, workspace, _, cx| {
                            submit(workspace, cx, move |tree, ctx| {
                                model::set_unblock_point(tree, ctx, &id, &target, point)
                            })
                        }),
                    )
                }),
            )
        }),
    );
}

fn set_merge_ship(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    const VALUES: [Option<bool>; 3] = [Some(true), Some(false), None];
    with_task(
        workspace,
        window,
        cx,
        "Set merge ship of which task?",
        "There is no leaf task",
        is_leaf,
        Box::new(|id, workspace, window, cx| {
            modals::pick(
                workspace,
                "Does a merged PR complete the ships todo?",
                vec![
                    "on".to_string(),
                    "off".to_string(),
                    "repository default".to_string(),
                ],
                0,
                Box::new(move |choice, workspace, _, cx| {
                    let value = VALUES[choice];
                    submit(workspace, cx, move |tree, ctx| {
                        model::set_merge_ship(tree, ctx, &id, value)
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

/// Where a retargeted task points, as the picker lists it.
#[derive(Clone)]
enum Target {
    StackedOn(String),
    After(String),
    Main,
}

fn retarget(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Retarget which stacked task?",
        "No task is stacked on another",
        |task, _| {
            task.file
                .as_ref()
                .is_some_and(|file| file.stacked_on.is_some())
        },
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let mut targets = Vec::new();
            let mut labels = Vec::new();
            for sibling in siblings_of(&tree, &id) {
                let Some(sibling_id) = sibling.id() else {
                    continue;
                };
                for (target, verb) in [
                    (Target::StackedOn(sibling_id.to_string()), "stacked on"),
                    (Target::After(sibling_id.to_string()), "after"),
                ] {
                    targets.push(target);
                    labels.push(format!("{verb} {}", sibling.slug));
                }
            }
            targets.push(Target::Main);
            labels.push("main (no edge)".to_string());
            modals::pick(
                workspace,
                "Point it at",
                labels,
                0,
                Box::new(move |choice, workspace, _, cx| {
                    let to = match targets[choice].clone() {
                        Target::StackedOn(target) => Retarget::StackedOn(target),
                        Target::After(target) => Retarget::After(target),
                        Target::Main => Retarget::Main,
                    };
                    submit(workspace, cx, move |tree, ctx| {
                        model::retarget(tree, ctx, &id, to.clone())
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

const RESULT_KINDS: [(ResultKind, &str); 6] = [
    (ResultKind::Pr, "pr"),
    (ResultKind::Prs, "prs"),
    (ResultKind::Document, "document"),
    (ResultKind::Link, "link"),
    (ResultKind::Findings, "findings"),
    (ResultKind::Branch, "branch"),
];

const RESULT_STATES: [(Option<ResultState>, &str); 5] = [
    (None, "no state"),
    (Some(ResultState::Open), "open"),
    (Some(ResultState::Approved), "approved"),
    (Some(ResultState::Merged), "merged"),
    (Some(ResultState::Closed), "closed"),
];

fn set_result(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Set the result of which task?",
        "There is no leaf task",
        is_leaf,
        Box::new(|id, workspace, window, cx| {
            modals::pick(
                workspace,
                "Kind of result",
                RESULT_KINDS
                    .iter()
                    .map(|(_, name)| name.to_string())
                    .collect(),
                0,
                Box::new(move |choice, workspace, window, cx| {
                    let kind = RESULT_KINDS[choice].0;
                    modals::ask(
                        workspace,
                        "URL or path of the result; for prs, several separated by spaces; may be empty",
                        "",
                        true,
                        Box::new(move |url, workspace, window, cx| {
                            modals::pick(
                                workspace,
                                "State of the result",
                                RESULT_STATES
                                    .iter()
                                    .map(|(_, name)| name.to_string())
                                    .collect(),
                                0,
                                Box::new(move |choice, workspace, _, cx| {
                                    let state = RESULT_STATES[choice].0;
                                    let result = build_result(kind, &url, state);
                                    submit(workspace, cx, move |tree, ctx| {
                                        model::set_result(tree, ctx, &id, result.clone())
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
                window,
                cx,
            )
        }),
    );
}

fn build_result(kind: ResultKind, url: &str, state: Option<ResultState>) -> TaskResult {
    let (url, urls) = if kind == ResultKind::Prs {
        (None, url.split_whitespace().map(str::to_string).collect())
    } else {
        ((!url.is_empty()).then(|| url.to_string()), Vec::new())
    };
    TaskResult {
        kind,
        url,
        state,
        urls,
        extra: Extra::new(),
    }
}

fn clear_result(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Clear the result of which task?",
        "No task has a result",
        |task, _| task.file.as_ref().is_some_and(|file| file.result.is_some()),
        Box::new(|id, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                model::clear_result(tree, ctx, &id)
            })
        }),
    );
}
