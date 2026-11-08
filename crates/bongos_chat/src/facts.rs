//! [`TaskFacts`] from the Work snapshot (design D10): what `context.md` says about a task besides
//! its `SessionContext`. The builder reads the task's place in the tree, its siblings it depends
//! on, and its `[result]`. It never lists a task directory and never reads `artifacts.toml`, so
//! the facts hold no artifact path or description: `notes_dir` is the `notes/` directory path
//! alone. The only source of artifact paths in anything injected stays `SessionContext`.

use bongos_chat_model::TaskFacts;
use bongos_work_model::derive;
use bongos_work_model::graph::state_label;
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{ResultKind, TaskResult};

/// The facts of the task with `id` in `tree`; `None` when `tree` has no such task.
///
/// - Project: the title and directory of the project the task is in (the project's top task gets
///   its own); none for a one-off and its descendants.
/// - Notes directory: `<task dir>/notes`, whether or not it exists.
/// - Dependencies: the goal and derived state label of each sibling the task is `after` or
///   `stacked_on`, in file order, once each.
/// - Pull requests: the URLs of a `pr` or `prs` `[result]`.
pub fn task_facts(tree: &WorkTree, id: &str) -> Option<TaskFacts> {
    let place = Place::locate(tree, id).ok()?;
    let task = place.task;
    let (project_title, project_dir) = match place.project {
        Some(project) => (
            project.file.as_ref().map(|file| file.title.clone()),
            Some(project.task.dir.clone()),
        ),
        None => (None, None),
    };

    let mut deps: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    if let Some(file) = &task.file {
        let targets = file
            .after
            .iter()
            .map(|edge| edge.task.as_str())
            .chain(file.stacked_on.iter().map(|edge| edge.task.as_str()));
        for slug in targets {
            if seen.contains(&slug) {
                continue;
            }
            seen.push(slug);
            if let Some(sibling) = place.sibling(slug) {
                deps.push((
                    sibling.goal().to_string(),
                    state_label(derive::state(sibling, tree)).to_string(),
                ));
            }
        }
    }

    let pr_links = task
        .file
        .as_ref()
        .and_then(|file| file.result.as_ref())
        .map(pr_links)
        .unwrap_or_default();

    Some(TaskFacts {
        project_title,
        project_dir,
        notes_dir: Some(task.dir.join("notes")),
        deps,
        pr_links,
    })
}

fn pr_links(result: &TaskResult) -> Vec<String> {
    if !matches!(result.kind, ResultKind::Pr | ResultKind::Prs) {
        return Vec::new();
    }
    let mut links: Vec<String> = Vec::new();
    for url in result.url.iter().chain(&result.urls) {
        let url = url.trim();
        if !url.is_empty() && !links.iter().any(|link| link == url) {
            links.push(url.to_string());
        }
    }
    links
}
