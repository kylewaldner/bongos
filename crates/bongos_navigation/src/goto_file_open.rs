//! What a row of the go to file picker does when it is chosen, by target.

use std::path::{Path, PathBuf};

use anyhow::Result;
use gpui::{App, AsyncWindowContext, Context, Entity, TaskExt as _, WeakEntity, Window};
use project::{CreateOptions, Project, ProjectPath};
use util::paths::home_dir;
use workspace::{OpenOptions, Workspace};

use crate::goto_file::HOST;
use crate::target::{PaneChoice, realize, resolve};

/// What a row of the go to file picker stands for.
#[derive(Clone, Debug)]
pub enum FileTarget {
    /// A file of a visible worktree.
    Project(ProjectPath),
    /// A file outside every visible worktree, typed as an absolute path.
    Abs(PathBuf),
}

/// Opens `target`.
///
/// A project file opens by project path in the pane `choice` realizes (splitting once when the
/// choice asks for a new pane), as a permanent item that is focused. A file outside the project
/// opens through `Workspace::open_abs_path`, which always uses the workspace's default pane, so
/// it ignores `choice`: this is a stated limitation of the picker.
pub(crate) fn open(
    workspace: &mut Workspace,
    target: &FileTarget,
    choice: &PaneChoice,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match target {
        FileTarget::Project(path) => {
            let resolved = resolve(workspace, choice, cx);
            let pane = realize(workspace, resolved, window, cx);
            workspace
                .open_path_preview(
                    path.clone(),
                    Some(pane.downgrade()),
                    true,
                    false,
                    true,
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
        }
        FileTarget::Abs(path) => {
            workspace
                .open_abs_path(path.clone(), OpenOptions::default(), window, cx)
                .detach_and_log_err(cx);
        }
    }
}

/// Where the typed `input` names a file: absolute and `~/` paths as given, any other relative to
/// the first visible worktree.
fn typed_file_path(input: &str, root: Option<&Path>) -> Option<PathBuf> {
    if input.starts_with('/') {
        Some(PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        Some(home_dir().join(rest))
    } else {
        root.map(|root| root.join(input))
    }
}

/// How a directory is named in the question that asks to create it.
fn directory_label(directory: &Path, root: Option<&Path>) -> String {
    let shown = root
        .and_then(|root| directory.strip_prefix(root).ok())
        .unwrap_or(directory);
    format!("{}/", shown.display())
}

/// The file `input` names, opened as a row is, after it is created when it does not exist.
///
/// Empty input is refused. When the parent directory is missing, a yes or no question asks to
/// create it first and no answer creates nothing. A failure is shown in the echo area; directories
/// made before it stay.
pub(crate) fn create_and_open(
    workspace: &mut Workspace,
    input: &str,
    choice: &PaneChoice,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let input = input.trim();
    if input.is_empty() || input.ends_with('/') {
        bongos_minibuffer::error(window, cx, "Give the name of a file");
        return;
    }
    let root: Option<PathBuf> = workspace
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
    let Some(abs_path) = typed_file_path(input, root.as_deref()) else {
        bongos_minibuffer::error(window, cx, "No project to create the file in");
        return;
    };
    let project = workspace.project().clone();
    let weak = cx.entity().downgrade();
    let choice = choice.clone();
    window
        .spawn(cx, async move |cx| {
            let fs = project.read_with(cx, |project, _| project.fs().clone());
            let metadata = fs.metadata(&abs_path).await.ok().flatten();
            let parent = abs_path.parent().map(Path::to_path_buf);
            let parent_exists = match &parent {
                Some(parent) => fs.metadata(parent).await.ok().flatten().is_some(),
                None => true,
            };
            cx.update(|window, cx| {
                if metadata.is_some_and(|metadata| metadata.is_dir) {
                    bongos_minibuffer::error(window, cx, "That is a directory");
                    return;
                }
                let exists = metadata.is_some();
                match parent {
                    Some(parent) if !parent_exists && !exists => {
                        let label = directory_label(&parent, root.as_deref());
                        let asker = weak.clone();
                        asker
                            .update(cx, |workspace, cx| {
                                bongos_minibuffer::yes_or_no(
                                    workspace,
                                    HOST,
                                    format!("Create directory {label}? "),
                                    false,
                                    move |yes, window, cx| {
                                        if yes {
                                            create(weak, project, abs_path, choice, window, cx);
                                        }
                                    },
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                    }
                    _ => create(weak, project, abs_path, choice, window, cx),
                }
            })
            .ok();
        })
        .detach();
}

/// Makes the missing directories and the file, then opens it.
fn create(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    abs_path: PathBuf,
    choice: PaneChoice,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let made = make_file(&project, &abs_path, cx).await;
            cx.update(|window, cx| {
                if let Err(error) = made {
                    bongos_minibuffer::error(window, cx, format!("Cannot create file: {error}"));
                    return;
                }
                workspace
                    .update(cx, |workspace, cx| {
                        let target = project
                            .read(cx)
                            .find_project_path(&abs_path, cx)
                            .map_or(FileTarget::Abs(abs_path), FileTarget::Project);
                        open(workspace, &target, &choice, window, cx)
                    })
                    .ok();
            })
            .ok();
        })
        .detach();
}

/// Creates the parent directory when it is missing and then the file. A project path goes through
/// `Project::create_entry` for a directory only; a file is always made by `fs.create_file` that
/// keeps what exists, so an existing file is never truncated.
async fn make_file(
    project: &Entity<Project>,
    abs_path: &Path,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    if let Some(parent) = abs_path.parent()
        && fs.metadata(parent).await?.is_none()
    {
        let directory = project.read_with(cx, |project, cx| project.find_project_path(parent, cx));
        match directory {
            Some(path) => {
                project
                    .update(cx, |project, cx| project.create_entry(path, true, cx))
                    .await?;
            }
            None => fs.create_dir(parent).await?,
        }
    }
    fs.create_file(
        abs_path,
        CreateOptions {
            overwrite: false,
            ignore_if_exists: true,
        },
    )
    .await?;
    // The worktree learns of the file before it is opened by project path.
    let refresh = project.update(cx, |project, cx| {
        let (worktree, relative) = project.find_worktree(abs_path, cx)?;
        worktree.update(cx, |worktree, cx| {
            let local = worktree.as_local()?;
            Some(local.refresh_entry(relative, None, cx))
        })
    });
    if let Some(refresh) = refresh {
        refresh.await.ok();
    }
    Ok(())
}

#[cfg(test)]
#[path = "goto_file_open_tests.rs"]
mod tests;
