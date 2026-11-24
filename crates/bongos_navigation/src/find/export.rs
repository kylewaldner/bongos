//! Exporting find in files results to a multibuffer.

use gpui::{Context, Entity, Task, Window};
use project::search::SearchQuery;
use search::ProjectSearchView;
use search::project_search::ProjectSearch;
use workspace::Workspace;

use crate::find::engine::FindRequest;
use crate::find::picker::FindPicker;
use crate::registry;

/// Opens the results of the open find picker in a multibuffer.
pub fn to_multibuffer(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    registry::in_session(window, cx, |session, window, cx| {
        if let Some(picker) = session.as_any().downcast_ref::<FindPicker>() {
            open(picker, workspace, window, cx);
        }
    })
}

/// Closes the session of `picker`, if it is still open, and opens a project search item that
/// runs the picker's query over every match. Nothing happens when there is no valid query.
pub(crate) fn open(
    picker: &FindPicker,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let request = picker.request();
    if query(&request, workspace, cx).is_none() {
        return;
    }
    if let Some(handle) = picker.handle() {
        handle.close(window, cx);
    }
    if let Some(search) = search(&request, workspace, window, cx) {
        search.detach();
    }
}

/// Opens a project search item that runs `request` over every match. The task resolves with the
/// search once it has ended, and with `None` when the item is closed before that. `None` when
/// there is no valid query.
pub(crate) fn search(
    request: &FindRequest,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Task<Option<Entity<ProjectSearch>>>> {
    let query = query(request, workspace, cx)?;
    Some(ProjectSearchView::open_with_query(
        workspace, query, window, cx,
    ))
}

/// The project search for `request`, or `None` for empty text or an invalid query.
fn query(
    request: &FindRequest,
    workspace: &Workspace,
    cx: &Context<Workspace>,
) -> Option<SearchQuery> {
    if request.text.is_empty() {
        return None;
    }
    let project = workspace.project().read(cx);
    let full_paths = project.visible_worktrees(cx).nth(1).is_some();
    request.query(full_paths, project.path_style(cx)).ok()
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
