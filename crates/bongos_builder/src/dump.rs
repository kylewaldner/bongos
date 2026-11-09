//! `proposal: dump view` (debug builds): the state of the proposal view as text, for tests and
//! the smoke scenario: the proposal number, the breadcrumb, the problems and the rows of the
//! level. Written to `proposal-view.txt` in the logs directory.

use std::path::{Path, PathBuf};

use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::show_toast;
use crate::view::ProposalView;

/// The file the dump is written to, in the logs directory.
pub(crate) const DUMP_FILE: &str = "proposal-view.txt";

/// Writes `text` to [`DUMP_FILE`] in `logs_dir` and returns the path.
pub(crate) fn write_view(text: &str, logs_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Writes the active proposal view, else the first one open, to [`DUMP_FILE`] in `logs_dir`, and
/// toasts where it went.
pub(crate) fn dump_into(workspace: &mut Workspace, logs_dir: &Path, cx: &mut Context<Workspace>) {
    let view = workspace
        .active_item_as::<ProposalView>(cx)
        .or_else(|| workspace.items_of_type::<ProposalView>(cx).next());
    let Some(view) = view else {
        show_toast(workspace, "No proposal view to dump".to_string(), cx);
        return;
    };
    let text = view.read(cx).view_text();
    let message = match write_view(&text, logs_dir) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the proposal view: {error}"),
    };
    show_toast(workspace, message, cx);
}

/// `proposal: dump view`.
pub(crate) fn dump_view(workspace: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    dump_into(workspace, paths::logs_dir(), cx);
}
