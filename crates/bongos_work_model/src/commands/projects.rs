//! `project: new`, `project: archive` and `project: unarchive`. A project's history lives in its
//! top group's `task.toml`, so each command writes that file next to `project.toml`.

use super::{Change, Context, Outcome, Write};
use crate::layout;
use crate::slug::{slugify, unique_slug};
use crate::store::{Project, WorkTree};
use crate::types::{Kind, ProjectFile, TaskFile};

/// What `project: new` asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewProject {
    pub title: String,
    /// Names of repositories from the settings.
    pub repositories: Vec<String>,
}

/// `project: new`: `projects/<slug>/` with `project.toml` and the top group's `task.toml`.
pub fn project_new(tree: &WorkTree, ctx: &Context, new: NewProject) -> Outcome {
    let title = new.title.trim();
    if title.is_empty() {
        return Err("a project needs a title".to_string());
    }
    let taken: Vec<&str> = tree
        .projects
        .iter()
        .map(|project| project.slug.as_str())
        .collect();
    let slug = unique_slug(&slugify(title), &taken);
    let dir = layout::project_dir(&tree.root, &slug);

    let mut project = ProjectFile::new(title);
    project.repositories = new.repositories;
    let mut task = TaskFile::new(title, Kind::Other);
    task.created = ctx.at;
    let entry = ctx.entry("project created", Some(title.to_string()));
    task.history.push(entry.clone());
    Ok(Change {
        dir: dir.clone(),
        writes: vec![
            Write::Task {
                dir: dir.clone(),
                file: task,
            },
            Write::Project { dir, file: project },
        ],
        entry,
        moves: Vec::new(),
    })
}

fn set_archived(tree: &WorkTree, ctx: &Context, slug: &str, archived: bool) -> Outcome {
    let project: &Project = tree
        .projects
        .iter()
        .find(|project| project.slug == slug)
        .ok_or_else(|| format!("no project is named {slug}"))?;
    // Errors in `project.toml` are badges on the top group, so this guards both files.
    project
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    let (Some(file), Some(task)) = (&project.file, &project.task.file) else {
        return Err(format!("{slug} has no project.toml or task.toml"));
    };
    if file.archived == archived {
        let state = if archived { "archived" } else { "not archived" };
        return Err(format!("{slug} is already {state}"));
    }
    let mut file = file.clone();
    file.archived = archived;
    let mut task = task.clone();
    let event = if archived {
        "project archived"
    } else {
        "project unarchived"
    };
    let entry = ctx.entry(event, None);
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

/// `project: archive`: the project moves to the Closed section.
pub fn project_archive(tree: &WorkTree, ctx: &Context, slug: &str) -> Outcome {
    set_archived(tree, ctx, slug, true)
}

/// `project: unarchive`.
pub fn project_unarchive(tree: &WorkTree, ctx: &Context, slug: &str) -> Outcome {
    set_archived(tree, ctx, slug, false)
}
