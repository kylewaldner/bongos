//! `task: new from linear`: a picker of recent tickets (or a typed identifier or URL), the
//! refusal for a ticket a task already links (it is focused instead), the placement, the read of
//! the ticket and its subtickets level by level, the preview buffer `Import SEQ-120` and the
//! confirm.
//!
//! Every step before the confirm only reads from Linear and shows something: no file is written
//! and no journal entry is made, so Cancel (or closing the confirm) leaves the work tree as it
//! was. The confirm is one `WorkStore::write` of `bongos_linear_model::import::change`, which
//! builds one feature-03 `Change` for the whole tree and refuses when a ticket of the plan became
//! linked meanwhile. The plan, the preview text and the change are the model's; this file only
//! drives the pickers and the reads.

use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::import::{self as model, ImportError, ImportPlan};
use bongos_linear_model::sync::{linked_task, task_path};
use bongos_linear_model::{ApiError, Issue, IssueRef, IssueSummary};
use bongos_work::{Focus, WorkSettings};
use bongos_work_model::commands::{NewTask, Placement};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, TaskId};
use futures::future::BoxFuture;
use gpui::{App, Context, Entity, EntityId, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::LinearSettings;
use crate::actions::show_toast;
use crate::link::{self, Choice, RECENT_LIMIT};
use crate::preview;
use crate::refresh;
use crate::sync::{self, LinearSync, Refusal, Scope};
use crate::token;

/// The two answers of the confirm.
pub(crate) const IMPORT: &str = "Import";
pub(crate) const CANCEL: &str = "Cancel";

/// The question of the confirm for the ticket `identifier`.
pub(crate) fn confirm_prompt(identifier: &str) -> String {
    format!("Import {identifier}?")
}

/// Why a read of the ticket tree stopped.
#[derive(Debug, PartialEq)]
pub(crate) enum ReadError {
    Refused(Refusal),
    Api(ApiError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Refused(refusal) => refusal.fmt(f),
            ReadError::Api(error) => error.fmt(f),
        }
    }
}

type Read = BoxFuture<'static, Result<Vec<Issue>, ApiError>>;

fn enabled(cx: &App) -> bool {
    LinearSettings::get_global(cx).enabled
}

/// Starts one read of `refs` through the gate, under `Scope::NewFromLinear`.
fn start_read(
    linear: &Entity<LinearSync>,
    refs: Vec<IssueRef>,
    cx: &mut App,
) -> Result<Read, Refusal> {
    linear.update(cx, |linear, cx| {
        linear.call(Scope::NewFromLinear, cx, move |client| client.issues(refs))
    })
}

/// The index paths (child positions from the root) of the tickets `level` levels below `node`,
/// skipping every ticket under one that has more subtickets than the cap: those are not imported.
fn at_level(node: &Issue, level: usize, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if level == 0 {
        out.push(path.clone());
        return;
    }
    if node.children_truncated {
        return;
    }
    for (index, child) in node.children.iter().enumerate() {
        path.push(index);
        at_level(child, level - 1, path, out);
        path.pop();
    }
}

fn node_at<'a>(root: &'a Issue, path: &[usize]) -> &'a Issue {
    path.iter().fold(root, |node, index| &node.children[*index])
}

fn node_at_mut<'a>(root: &'a mut Issue, path: &[usize]) -> &'a mut Issue {
    path.iter()
        .fold(root, |node, index| &mut node.children[*index])
}

/// Reads the subtickets of `root` (which has its first level) level by level down to
/// [`model::MAX_DEPTH`] levels, one `read` per level over every ticket of that level; the client
/// splits a read into batches sized by complexity. A ticket whose subtickets are past the cap is
/// not read further. Every ticket of the result has its own subtickets, one level of them, so
/// the plan can list what is deeper than the limit.
pub(crate) async fn read_levels(
    mut root: Issue,
    mut read: impl FnMut(Vec<IssueRef>) -> Result<Read, ReadError>,
) -> Result<Issue, ReadError> {
    for level in 1..=model::MAX_DEPTH {
        let mut paths = Vec::new();
        at_level(&root, level, &mut Vec::new(), &mut paths);
        let refs: Vec<IssueRef> = paths
            .iter()
            .filter_map(|path| parse_identifier(&node_at(&root, path).identifier))
            .map(IssueRef::from)
            .collect();
        if refs.is_empty() {
            break;
        }
        let issues = read(refs)?.await.map_err(ReadError::Api)?;
        for path in &paths {
            let slot = node_at_mut(&mut root, path);
            if let Some(fresh) = issues.iter().find(|issue| issue.id == slot.id) {
                *slot = fresh.clone();
            }
        }
    }
    Ok(root)
}

/// The rows of the placement picker: the group at point (the task at point when it is a group,
/// else its parent group), the top of the focused project, then every other project. Only
/// writable groups are listed, each once.
pub(crate) fn placements(
    tree: &WorkTree,
    at_point: Option<&str>,
    focused: Option<&str>,
) -> Vec<(TaskId, String)> {
    let mut rows: Vec<(TaskId, String)> = Vec::new();
    let mut add = |task: &bongos_work_model::store::Task, label: String| {
        let Some(id) = task.id() else { return };
        if task.check_writable().is_err() || rows.iter().any(|(seen, _)| seen == id) {
            return;
        }
        rows.push((id.to_string(), label));
    };
    let group_at_point = at_point
        .and_then(|id| Place::locate(tree, id).ok())
        .and_then(|place| {
            if derive::shape(place.task, tree) == Shape::Group {
                Some(place.task)
            } else {
                place.ancestors.first().copied()
            }
        });
    if let Some(group) = group_at_point {
        add(group, format!("{} (group at point)", group.goal()));
    }
    let project_title = |project: &bongos_work_model::store::Project| {
        project
            .file
            .as_ref()
            .map_or(project.slug.clone(), |file| file.title.clone())
    };
    let focused_project = focused
        .and_then(|id| Place::locate(tree, id).ok())
        .and_then(|place| place.project);
    if let Some(project) = focused_project {
        add(
            &project.task,
            format!("{} (top of the focused project)", project_title(project)),
        );
    }
    for project in &tree.projects {
        if project.file.as_ref().is_some_and(|file| file.archived) {
            continue;
        }
        add(&project.task, format!("{} (project)", project_title(project)));
    }
    rows
}

/// The message and the task to focus when a task already links `issue`.
pub(crate) fn already_imported(tree: &WorkTree, issue: &Issue) -> Option<(Option<TaskId>, String)> {
    let existing = linked_task(tree, &issue.id, &issue.identifier, None)?;
    let message = ImportError::AlreadyImported {
        identifier: issue.identifier.clone(),
        task_id: existing.id().map(str::to_string),
        path: task_path(tree, existing),
    }
    .to_string();
    Some((existing.id().map(str::to_string), message))
}

/// The default kind of the leaves made under `placement`.
fn leaf_kind(tree: &WorkTree, placement: &str, cx: &App) -> Kind {
    bongos_work::default_kind(
        tree,
        &Placement::Under(placement.to_string()),
        &NewTask::default(),
        &WorkSettings::get_global(cx).repositories,
    )
    .unwrap_or(Kind::PrShip)
}

/// `task: new from linear`. The token is read first; no token means a toast and no request.
pub(crate) fn new_from_linear(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if bongos_work::ready(workspace, cx).is_none() {
        return;
    }
    let handle = window.window_handle();
    token::with_token(cx, move |_, _, cx| {
        // The setting may have changed while the token was read.
        if !enabled(cx) {
            return;
        }
        let workspace = cx.entity().downgrade();
        // The token read has no window; the picker needs one.
        cx.defer(move |cx| {
            handle
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| start(workspace, window, cx))
                        .ok();
                })
                .ok();
        });
    });
}

fn unavailable(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    show_toast(
        workspace,
        "The Linear sync is not available".to_string(),
        cx,
    );
}

/// Reads the recent tickets that fill the picker.
fn start(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(linear) = sync::global(cx) else {
        unavailable(workspace, cx);
        return;
    };
    let started = linear.update(cx, |linear, cx| {
        linear.call(Scope::NewFromLinear, cx, |client| {
            client.recent(RECENT_LIMIT)
        })
    });
    let recent = match started {
        Ok(recent) => recent,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let recent = recent.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                open_picker(workspace, recent, window, cx)
            })
            .ok();
    })
    .detach();
}

fn open_picker(
    workspace: &mut Workspace,
    recent: Result<Vec<IssueSummary>, ApiError>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    // A failed read still leaves the typed row, so a ticket can be imported by its identifier.
    let recent = recent.unwrap_or_else(|error| {
        show_toast(
            workspace,
            format!("Could not list recent Linear tickets: {error}"),
            cx,
        );
        Vec::new()
    });
    let rows = link::choices(&tree, "", &[], &recent);
    let (picks, labels): (Vec<Choice>, Vec<String>) = rows.into_iter().unzip();
    let selected = usize::from(picks.len() > 1);
    bongos_work::pick(
        workspace,
        "Import Linear ticket",
        labels,
        selected,
        Box::new(move |index, workspace, window, cx| match &picks[index] {
            Choice::Ticket(ticket) => match parse_identifier(&ticket.identifier) {
                Some(identifier) => chosen(workspace, identifier.into(), window, cx),
                None => show_toast(
                    workspace,
                    format!("{} is not a Linear identifier", ticket.identifier),
                    cx,
                ),
            },
            Choice::Typed => ask_ticket(workspace, window, cx),
        }),
        window,
        cx,
    );
}

fn ask_ticket(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    bongos_work::ask(
        workspace,
        "Linear ticket (identifier or URL)",
        "",
        false,
        Box::new(
            move |text, workspace, window, cx| match link::parse_typed(&text) {
                Some(reference) => chosen(workspace, reference, window, cx),
                None => show_toast(workspace, "Not a Linear identifier or URL".to_string(), cx),
            },
        ),
        window,
        cx,
    );
}

/// Reads the chosen ticket with its first level of subtickets.
fn chosen(
    workspace: &mut Workspace,
    reference: IssueRef,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(linear) = sync::global(cx) else {
        unavailable(workspace, cx);
        return;
    };
    let wanted = reference.clone();
    let read = match start_read(&linear, vec![wanted], cx) {
        Ok(read) => read,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let result = read.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(issues) => match issues.into_iter().next() {
                    Some(issue) => found(workspace, issue, window, cx),
                    None => show_toast(
                        workspace,
                        format!("{reference} was not found in Linear"),
                        cx,
                    ),
                },
                Err(error) => show_toast(workspace, error.to_string(), cx),
            })
            .ok();
    })
    .detach();
}

/// Focuses the task that already imported the ticket and says so; nothing is created.
fn focus_existing(
    workspace: &mut Workspace,
    task: Option<TaskId>,
    message: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(id) = task {
        bongos_work::focus_task(&id, workspace, window, cx);
    }
    show_toast(workspace, message, cx);
}

/// A ticket no task links goes on to the placement picker; one a task links is focused.
fn found(workspace: &mut Workspace, issue: Issue, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    if let Some((task, message)) = already_imported(&tree, &issue) {
        focus_existing(workspace, task, message, window, cx);
        return;
    }
    let at_point = bongos_work::task_at_point(workspace, window, cx);
    let focused = Focus::global(cx).and_then(|focus| focus.read(cx).task_id().map(str::to_string));
    let (ids, labels): (Vec<TaskId>, Vec<String>) =
        placements(&tree, at_point.as_deref(), focused.as_deref())
            .into_iter()
            .unzip();
    if ids.is_empty() {
        show_toast(workspace, "There is no project to import into".to_string(), cx);
        return;
    }
    bongos_work::pick(
        workspace,
        format!("Import {} into", issue.identifier),
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            read_tree(workspace, issue, ids[index].clone(), window, cx)
        }),
        window,
        cx,
    );
}

/// Reads the rest of the tree level by level, then plans.
fn read_tree(
    workspace: &mut Workspace,
    issue: Issue,
    placement: TaskId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(linear) = sync::global(cx) else {
        unavailable(workspace, cx);
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let result = read_levels(issue, |refs| {
            cx.update(|_, cx| start_read(&linear, refs, cx))
                .map_err(|error| ReadError::Api(ApiError::Network(error.to_string())))?
                .map_err(ReadError::Refused)
        })
        .await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(issue) => planned(workspace, issue, placement, window, cx),
                Err(error) => show_toast(workspace, error.to_string(), cx),
            })
            .ok();
    })
    .detach();
}

/// Plans on the tree as it is now and opens the preview, or says why there is no plan.
pub(crate) fn planned(
    workspace: &mut Workspace,
    issue: Issue,
    placement: TaskId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    match model::plan(&issue, &tree, &placement) {
        Ok(plan) => {
            let identifier = plan.root.identifier.clone();
            let text = plan.preview();
            preview::open(
                &identifier,
                text,
                workspace,
                window,
                cx,
                Box::new(move |workspace, item, window, cx| {
                    confirm(workspace, plan, item, window, cx)
                }),
            );
        }
        Err(ImportError::AlreadyImported { task_id, .. }) => {
            let message = already_imported(&tree, &issue).map_or_else(String::new, |(_, m)| m);
            focus_existing(workspace, task_id, message, window, cx);
        }
        Err(error) => show_toast(workspace, error.to_string(), cx),
    }
}

/// Asks Import / Cancel behind the preview.
fn confirm(
    workspace: &mut Workspace,
    plan: ImportPlan,
    item: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    bongos_work::pick(
        workspace,
        confirm_prompt(&plan.root.identifier),
        vec![IMPORT.to_string(), CANCEL.to_string()],
        0,
        Box::new(move |index, workspace, window, cx| {
            answered(workspace, plan, item, index == 0, window, cx)
        }),
        window,
        cx,
    );
}

/// The answer of the confirm: Cancel closes the preview and writes nothing.
pub(crate) fn answered(
    workspace: &mut Workspace,
    plan: ImportPlan,
    item: EntityId,
    import: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    preview::close(workspace, item, window, cx);
    if import {
        finish_import(workspace, plan, cx);
    }
}

/// Writes the whole tree in one command and toasts the outcome. The command re-checks the
/// placement and every ticket on the tree the writer hands it, so a link made since the preview
/// still refuses.
pub(crate) fn finish_import(
    workspace: &mut Workspace,
    plan: ImportPlan,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let kind = leaf_kind(&tree, &plan.placement, cx);
    let identifier = plan.root.identifier.clone();
    let Some(write) = link::queue(workspace, cx, move |tree, ctx| {
        model::change(tree, ctx, &plan, kind)
    }) else {
        return;
    };
    cx.spawn(async move |workspace, cx| {
        let message = match write.await {
            Ok(_) => format!("Imported {identifier}"),
            Err(error) => error.to_string(),
        };
        workspace
            .update(cx, |workspace, cx| {
                // The store has the new links: the sync's linked set follows them.
                refresh::sync_linked(cx);
                show_toast(workspace, message, cx)
            })
            .ok();
    })
    .detach();
}
