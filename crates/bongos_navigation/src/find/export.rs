//! Exporting find in files results to a multibuffer.

use gpui::{Context, Window};
use project::search::SearchQuery;
use search::ProjectSearchView;
use workspace::Workspace;

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
    let Some(query) = query(picker, workspace, cx) else {
        return;
    };
    if let Some(handle) = picker.handle() {
        handle.close(window, cx);
    }
    ProjectSearchView::open_with_query(workspace, query, window, cx).detach();
}

/// The project search for what the picker holds, or `None` for empty text or an invalid query.
fn query(
    picker: &FindPicker,
    workspace: &Workspace,
    cx: &Context<Workspace>,
) -> Option<SearchQuery> {
    let request = picker.request();
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
