//! `proposal: dump view` and `proposal: dump preview` (debug builds): the state of the proposal
//! view as text (the proposal number, the breadcrumb, the problems and the rows of the level),
//! written to `proposal-view.txt` in the logs directory, and the text of the last preview of an
//! apply, written to `proposal-preview.txt`. For tests and the smoke scenario.

use std::path::{Path, PathBuf};

use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::show_toast;
use crate::preview_view;
use crate::view::ProposalView;

/// The file the dump is written to, in the logs directory.
pub(crate) const DUMP_FILE: &str = "proposal-view.txt";

/// The file the preview dump is written to, in the logs directory.
pub(crate) const PREVIEW_DUMP_FILE: &str = "proposal-preview.txt";

/// Writes `text` to [`DUMP_FILE`] in `logs_dir` and returns the path.
pub(crate) fn write_view(text: &str, logs_dir: &Path) -> anyhow::Result<PathBuf> {
    write_file(text, &logs_dir.join(DUMP_FILE))
}

fn write_file(text: &str, path: &Path) -> anyhow::Result<PathBuf> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(path.to_path_buf())
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

/// Writes the text of the last preview to [`PREVIEW_DUMP_FILE`] in `logs_dir`, and toasts where
/// it went.
pub(crate) fn dump_preview_into(
    workspace: &mut Workspace,
    logs_dir: &Path,
    cx: &mut Context<Workspace>,
) {
    let Some(text) = preview_view::last_preview(cx) else {
        show_toast(workspace, "No proposal preview to dump".to_string(), cx);
        return;
    };
    let message = match write_file(&text, &logs_dir.join(PREVIEW_DUMP_FILE)) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the proposal preview: {error}"),
    };
    show_toast(workspace, message, cx);
}

/// `proposal: dump preview`.
pub(crate) fn dump_preview(workspace: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    dump_preview_into(workspace, paths::logs_dir(), cx);
}
