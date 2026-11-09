//! The switches of the automatic apply (D15): the user-level `task_builder.auto_apply` and a
//! project's `auto_apply_proposals` in its `project.toml`, which `task: toggle auto apply` writes
//! on the project of the task at point. The project's value wins; a one-off has no project and
//! takes the setting.

use bongos_work::WorkStore;
use bongos_work::model::commands::{Change, Context as WorkContext, Outcome, Write};
use bongos_work::model::rules::Place;
use bongos_work::model::store::WorkTree;
use bongos_work::model::{HistoryEntry, ProjectFile};
use gpui::{App, Context, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::TaskBuilderSettings;
use crate::actions::show_toast;

/// The refusal of `task: toggle auto apply` on a one-off.
pub(crate) const ONE_OFF_MESSAGE: &str =
    "A one-off has no project: its proposals follow the setting task_builder.auto_apply";

/// The effective switch: the project's `auto_apply_proposals` when it sets one, else `global`
/// (`task_builder.auto_apply`). `project` is `None` for a one-off.
pub fn effective_auto_apply(project: Option<&ProjectFile>, global: bool) -> bool {
    project
        .and_then(|project| project.auto_apply_proposals)
        .unwrap_or(global)
}

/// Whether a proposal whose root is the task `task_id` of `tree` may apply on its own as far as
/// the switches go: `task_builder.enabled` is true and the effective switch of the task's project
/// (D15) is on. A task of a project whose `project.toml` does not read is never allowed, since its
/// switch is unknown; a task not in `tree` neither. The other conditions of D13 (who wrote it, the
/// subtree, the diff) are the caller's.
pub fn auto_apply_allowed(tree: &WorkTree, task_id: &str, cx: &App) -> bool {
    let settings = TaskBuilderSettings::get_global(cx);
    if !settings.enabled {
        return false;
    }
    let Ok(place) = Place::locate(tree, task_id) else {
        return false;
    };
    match place.project {
        None => settings.auto_apply,
        Some(project) => project
            .file
            .as_ref()
            .is_some_and(|file| effective_auto_apply(Some(file), settings.auto_apply)),
    }
}

/// The toast after a toggle.
pub(crate) fn toggled_message(slug: &str, on: bool) -> String {
    let state = if on { "on" } else { "off" };
    format!("Auto apply of proposals is {state} for the project {slug}")
}

/// The writer's command of `task: toggle auto apply`: flips the effective switch of the project
/// of the task `task_id`, with `global` the setting, writing it into `project.toml` and a line in
/// the history of the project's top group.
pub(crate) fn toggle_command(
    tree: &WorkTree,
    ctx: &WorkContext,
    task_id: &str,
    global: bool,
) -> Outcome {
    let place = Place::locate(tree, task_id)?;
    let project = place.project.ok_or_else(|| ONE_OFF_MESSAGE.to_string())?;
    // Errors in `project.toml` are badges on the top group, so this guards both files.
    project
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    let (Some(file), Some(task)) = (&project.file, &project.task.file) else {
        return Err(format!("{} has no project.toml or task.toml", project.slug));
    };
    let on = !effective_auto_apply(Some(file), global);
    let mut file = file.clone();
    file.auto_apply_proposals = Some(on);
    let entry = HistoryEntry {
        at: ctx.at,
        by: ctx.by.clone(),
        event: if on {
            "auto apply of proposals turned on".to_string()
        } else {
            "auto apply of proposals turned off".to_string()
        },
        detail: None,
        extra: Default::default(),
    };
    let mut task = task.clone();
    task.history.push(entry.clone());
    let dir = project.task.dir.clone();
    Ok(Change {
        dir: dir.clone(),
        writes: vec![
            Write::Task {
                dir: dir.clone(),
                file: task,
            },
            Write::Project { dir, file },
        ],
        entry,
        moves: Vec::new(),
    })
}

/// `task: toggle auto apply`: on the project of the task at point, else of a task picked by name.
pub(crate) fn toggle_auto_apply(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if bongos_work::ready(workspace, cx).is_none() {
        return;
    }
    match bongos_work::task_at_point(workspace, window, cx) {
        Some(id) => toggle_on(id, workspace, window, cx),
        None => bongos_work::with_task(
            workspace,
            window,
            cx,
            "Toggle auto apply for the project of which task?",
            "There is no task",
            |task, _| task.file.is_some(),
            Box::new(toggle_on),
        ),
    }
}

fn toggle_on(
    task_id: String,
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, bongos_work::WriteError::Off.to_string(), cx);
        return;
    };
    let global = TaskBuilderSettings::get_global(cx).auto_apply;
    let write = store.update(cx, |store, cx| {
        store.write(
            WorkContext::kyle(),
            move |tree, ctx| toggle_command(tree, ctx, &task_id, global),
            cx,
        )
    });
    cx.spawn(async move |workspace, cx| {
        let message = match write.await {
            Ok(change) => change
                .writes
                .iter()
                .find_map(|write| match write {
                    Write::Project { dir, file } => Some(toggled_message(
                        &dir.file_name().unwrap_or_default().to_string_lossy(),
                        file.auto_apply_proposals.unwrap_or(global),
                    )),
                    _ => None,
                })
                .unwrap_or_default(),
            Err(error) => error.to_string(),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}
