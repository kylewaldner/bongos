//! `task: move to project` and `task: make one off` for a task of a project repository: the
//! task and everything under it move to another project (the one-offs project for `make one
//! off`) through a [`MoveCoordinator`]. The plan is shown in a read-only buffer first (the
//! subtree, the target folder, the edges dropped, the repositories the target lacks, a renamed
//! slug, or why it cannot move), and nothing moves until the person confirms it. The move then
//! runs on the background executor; a move a restart interrupted is resumed when its project
//! opens again.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::Result;
use bongos_project_repo::{
    MoveCoordinator, MoveOutcome, MovePhase, MovePlan, MoveRefusal, MoveRequest, ProjectRepo,
};
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::store::{Project, Task, WorkTree};
use chrono::{DateTime, Utc};
use gpui::{AppContext as _, Context, Window};
use parking_lot::Mutex;
use util::ResultExt as _;
use workspace::Workspace;

use crate::backup_commands::open_read_only;
use crate::commands::{keyboard_context, ready, show_toast};
use crate::modals;
use crate::project_repos::open_repos;

/// The moves a coordinator of this process is driving now, so a resume never runs one twice.
static RUNNING: LazyLock<Mutex<BTreeSet<String>>> = LazyLock::new(Default::default);

/// Holds a move id in [`RUNNING`] until dropped.
struct Running(String);

impl Running {
    fn claim(move_id: &str) -> Option<Self> {
        RUNNING
            .lock()
            .insert(move_id.to_string())
            .then(|| Self(move_id.to_string()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.lock().remove(&self.0);
    }
}

/// The project repository's id of `project`, when it is one (a project in the new format).
pub(crate) fn repository_id(project: &Project) -> Option<&str> {
    if project.task.definition.is_none() {
        return None;
    }
    project.file.as_ref()?.id.as_deref()
}

/// The project in the new format that holds the task `task_id`, when the task is in one.
pub(crate) fn repository_project_of<'a>(tree: &'a WorkTree, task_id: &str) -> Option<&'a Project> {
    tree.projects.iter().find(|project| {
        repository_id(project).is_some()
            && project
                .task
                .walk()
                .into_iter()
                .any(|task| task.id() == Some(task_id))
    })
}

fn project_by_id<'a>(tree: &'a WorkTree, id: &str) -> Option<&'a Project> {
    tree.projects
        .iter()
        .find(|project| repository_id(project) == Some(id))
}

fn title(project: &Project) -> &str {
    project
        .file
        .as_ref()
        .map_or(project.slug.as_str(), |file| file.title.as_str())
}

/// The tasks of `project` a moved subtree can go under, by id and label: its top group first,
/// then its tasks (only the top level's for the one-offs project, whose groups are activities).
pub(crate) fn parent_choices(project: &Project, top_level_only: bool) -> Vec<(String, String)> {
    let label = |task: &Task| {
        let goal = task.file.as_ref().map_or("", |file| file.goal.as_str());
        format!("{} ({goal})", task.slug)
    };
    let mut choices = Vec::new();
    if let Some(top) = project.task.id() {
        choices.push((
            top.to_string(),
            format!("{}, at its top level", title(project)),
        ));
    }
    let tasks: Vec<&Task> = if top_level_only {
        project.task.children.iter().collect()
    } else {
        project.task.walk().into_iter().skip(1).collect()
    };
    for task in tasks {
        if let Some(id) = task.id() {
            choices.push((id.to_string(), label(task)));
        }
    }
    choices
}

/// The preview buffer: what the move does, or why it cannot be made.
pub(crate) fn render_preview(preview: &Result<MovePlan, MoveRefusal>) -> String {
    let mut text = String::from(
        "task: move to project moves a task and every task under it to another project.\n\
         Nothing moves until you confirm. A task that has started cannot move.\n",
    );
    let plan = match preview {
        Ok(plan) => plan,
        Err(refusal) => {
            text.push_str("\nCannot move:\n");
            for reason in &refusal.reasons {
                text.push_str(&format!("  - {reason}\n"));
            }
            return text;
        }
    };
    text.push_str(&format!(
        "\nFrom {} to {}\n",
        plan.source.title, plan.target.title
    ));
    text.push_str(&format!("Target folder: definitions/{}\n", plan.root_to()));
    if let Some(old) = &plan.renamed_from {
        text.push_str(&format!(
            "Renamed: {old} becomes {}, because {} already has a task named {old} there\n",
            plan.slug, plan.target.title
        ));
    }
    text.push_str(&format!(
        "Moves {} task{}:\n",
        plan.tasks.len(),
        plural(plan.tasks.len())
    ));
    for task in &plan.tasks {
        text.push_str(&format!(
            "  - {} ({} becomes {})\n",
            task.id,
            folder(&task.from),
            folder(&task.to)
        ));
    }
    text.push_str("Dropped edges:\n");
    if plan.dropped_edges.is_empty() {
        text.push_str("  none\n");
    }
    for edge in &plan.dropped_edges {
        text.push_str(&format!("  - {edge}\n"));
    }
    text.push_str(&format!(
        "Repositories {} lacks, added to its project.toml:\n",
        plan.target.title
    ));
    if plan.missing_repositories.is_empty() {
        text.push_str("  none\n");
    }
    for repository in &plan.missing_repositories {
        text.push_str(&format!("  - {repository}\n"));
    }
    text
}

fn folder(folder: &str) -> String {
    format!("definitions/{folder}")
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// What a person is told when a move ended.
pub(crate) fn outcome_message(plan: &MovePlan, move_id: &str, outcome: &MoveOutcome) -> String {
    match outcome {
        MoveOutcome::Moved => format!("Moved {} to {}", plan.slug, plan.target.title),
        MoveOutcome::Refused { reasons } => {
            format!("Not moved, neither project changed: {}", reasons.join("; "))
        }
        MoveOutcome::RolledBack { reasons } => format!(
            "Rolled back: {} landed in {} but not in {}, so it was undone: {}",
            plan.slug,
            plan.target.title,
            plan.source.title,
            reasons.join("; ")
        ),
        MoveOutcome::NeedsYou { reason } => format!("Move {move_id} needs you: {reason}"),
    }
}

/// What a person is told when the plan changed between the preview and the confirmation.
pub(crate) const PREVIEW_CHANGED: &str = "the projects changed since the preview was shown; \
     nothing was moved. Run the command again to see what it would do now";

/// Confirms the move of `request` by the person whose context `ctx` is, provided it still does
/// what `confirmed` (the plan the person saw) says, then drives it to its end. The message to
/// show.
pub(crate) async fn confirm_and_run(
    source: &ProjectRepo,
    target: &ProjectRepo,
    request: &MoveRequest,
    confirmed: &MovePlan,
    mut ctx: WorkContext,
    now: DateTime<Utc>,
) -> Result<String> {
    let author = ctx.author.clone();
    let coordinator = match MoveCoordinator::confirm_shown(
        source,
        target,
        request,
        confirmed,
        author,
        Some(&mut ctx),
    )
    .await?
    {
        Some(Ok(coordinator)) => coordinator,
        Some(Err(refusal)) => return Ok(format!("Not moved: {refusal}")),
        None => return Ok(format!("Not moved: {PREVIEW_CHANGED}")),
    };
    let move_id = coordinator.move_id().to_string();
    let Some(_running) = Running::claim(&move_id) else {
        anyhow::bail!("move {move_id} is already being driven");
    };
    let outcome = coordinator.run(now).await?;
    Ok(outcome_message(confirmed, &move_id, &outcome))
}

/// Drives every move started in one of `repos` that a restart interrupted to its end, each on a
/// thread of its own (one move waiting for an approval never holds up another), when its target
/// is among `repos` too and no coordinator of this process drives it already. A move that needs a
/// person is left for the person. Each move's outcome, by move id.
pub(crate) async fn resume_moves(
    repos: &[ProjectRepo],
    now: DateTime<Utc>,
) -> Vec<(String, Result<MoveOutcome>)> {
    let mut runs = Vec::new();
    for source in repos {
        let in_flight = match source.moves_in_flight() {
            Ok(in_flight) => in_flight,
            Err(error) => {
                log::error!("reading the moves of {}: {error:#}", source.location().id);
                continue;
            }
        };
        for move_id in in_flight {
            let Some(record) = source.move_record(&move_id).log_err().flatten() else {
                continue;
            };
            if record.phase == MovePhase::NeedsYou {
                log::warn!("move {move_id} needs you");
                continue;
            }
            let Some(target) = repos
                .iter()
                .find(|repo| repo.location().id == record.plan.target.id)
            else {
                continue;
            };
            let Some(running) = Running::claim(&move_id) else {
                continue;
            };
            let (source, target) = (source.clone(), target.clone());
            let id = move_id.clone();
            let run = on_own_thread(move || {
                let _running = running;
                let outcome = match MoveCoordinator::resume(&source, &target, &id) {
                    Ok(coordinator) => futures::executor::block_on(coordinator.run(now)),
                    Err(error) => Err(error),
                };
                match &outcome {
                    Ok(outcome) => log::info!("resumed move {id}: {outcome:?}"),
                    Err(error) => log::error!("resuming move {id}: {error:#}"),
                }
                outcome
            });
            runs.push(async move { (move_id, run.await.and_then(|outcome| outcome)) });
        }
    }
    futures::future::join_all(runs).await
}

/// Runs `work` on a thread of its own and resolves with what it returned. A move's landings run
/// side by side in a future that cannot leave its thread, and may wait long for an approval, so
/// it gets a thread rather than the background executor.
pub(crate) fn on_own_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl std::future::Future<Output = Result<T>> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("bongos-move".to_string())
        .spawn(move || {
            sender.send(work()).ok();
        });
    async move {
        spawned?;
        Ok(receiver.await?)
    }
}

/// Where the subtree goes.
pub(crate) enum Destination {
    /// A project the person picks.
    Pick,
    /// The one-offs project, by id: the subtree goes at its top level or under an activity.
    OneOffs(String),
}

/// Moves the task `task_id` of the project repository `source_id` to `destination`: picks the
/// project and the task to go under, then previews and asks.
pub(crate) fn start_move(
    task_id: String,
    source_id: String,
    destination: Destination,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    match destination {
        Destination::OneOffs(target_id) => {
            choose_parent(task_id, source_id, target_id, true, workspace, window, cx)
        }
        Destination::Pick => {
            let (ids, labels): (Vec<String>, Vec<String>) = tree
                .projects
                .iter()
                .filter_map(|project| {
                    let id = repository_id(project)?;
                    let archived = project.file.as_ref().is_some_and(|file| file.archived);
                    (id != source_id && !archived).then(|| {
                        (
                            id.to_string(),
                            format!("{} ({})", title(project), project.slug),
                        )
                    })
                })
                .unzip();
            if ids.is_empty() {
                show_toast(
                    workspace,
                    "There is no other project repository to move it to".to_string(),
                    cx,
                );
                return;
            }
            modals::pick(
                workspace,
                "Move it to which project?",
                labels,
                0,
                Box::new(move |index, workspace, window, cx| {
                    if let Some(target_id) = ids.get(index).cloned() {
                        choose_parent(task_id, source_id, target_id, false, workspace, window, cx);
                    }
                }),
                window,
                cx,
            );
        }
    }
}

fn choose_parent(
    task_id: String,
    source_id: String,
    target_id: String,
    top_level_only: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let Some(target) = project_by_id(&tree, &target_id) else {
        show_toast(
            workspace,
            format!("The project {target_id} is not open"),
            cx,
        );
        return;
    };
    let choices = parent_choices(target, top_level_only);
    let request = move |parent_id: String| MoveRequest {
        task_id: task_id.clone(),
        parent_id,
    };
    if choices.len() == 1 {
        let parent = choices[0].0.clone();
        preview_and_ask(request(parent), source_id, target_id, workspace, window, cx);
        return;
    }
    let (ids, labels): (Vec<String>, Vec<String>) = choices.into_iter().unzip();
    modals::pick(
        workspace,
        format!("Move it under which task of {}?", title(target)),
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            if let Some(parent) = ids.get(index).cloned() {
                preview_and_ask(request(parent), source_id, target_id, workspace, window, cx);
            }
        }),
        window,
        cx,
    );
}

fn open_repo(id: &str, cx: &gpui::App) -> Option<ProjectRepo> {
    open_repos(cx)
        .into_iter()
        .map(|loaded| loaded.repo().clone())
        .find(|repo| repo.location().id == id)
}

/// Shows the plan of `request` in a read-only buffer, then asks before moving.
fn preview_and_ask(
    request: MoveRequest,
    source_id: String,
    target_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (Some(source), Some(target)) = (open_repo(&source_id, cx), open_repo(&target_id, cx))
    else {
        show_toast(
            workspace,
            "Move: both projects must be open in this window's bongos".to_string(),
            cx,
        );
        return;
    };
    let previewing = {
        let (source, target, request) = (source.clone(), target.clone(), request.clone());
        cx.background_spawn(
            async move { MoveCoordinator::preview(&source, &target, &request).await },
        )
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let preview = match previewing.await {
            Ok(preview) => preview,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Move: {error:#}"), cx)
                    })
                    .log_err();
                return;
            }
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                open_read_only(workspace, "Move", render_preview(&preview), window, cx);
                let plan = match preview {
                    Ok(plan) => plan,
                    Err(refusal) => {
                        show_toast(workspace, format!("Not moved: {refusal}"), cx);
                        return;
                    }
                };
                modals::pick(
                    workspace,
                    format!(
                        "Move {} to {} as the preview shows?",
                        plan.slug, plan.target.title
                    ),
                    vec!["Move".to_string(), "Cancel".to_string()],
                    1,
                    Box::new(move |choice, _, window, cx| {
                        if choice != 0 {
                            return;
                        }
                        let ctx = keyboard_context(window, cx);
                        let moving = on_own_thread(move || {
                            futures::executor::block_on(confirm_and_run(
                                &source,
                                &target,
                                &request,
                                &plan,
                                ctx,
                                Utc::now(),
                            ))
                        });
                        cx.spawn(async move |workspace, cx| {
                            let message = match moving.await.and_then(|result| result) {
                                Ok(message) => message,
                                Err(error) => format!("Move: {error:#}"),
                            };
                            workspace
                                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                                .log_err();
                        })
                        .detach();
                    }),
                    window,
                    cx,
                );
            })
            .log_err();
    })
    .detach();
}
