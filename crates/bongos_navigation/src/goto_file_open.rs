//! What a row of the go to file picker does when it is chosen, by target.

use std::path::PathBuf;

use gpui::{Context, TaskExt as _, Window};
use project::ProjectPath;
use workspace::{OpenOptions, Workspace};

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
