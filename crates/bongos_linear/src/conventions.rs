//! The PR conventions of a task with a synced link (D15), on the app side: the section the chats'
//! context extension adds to a linked leaf's `context.md`, and `task: copy pr title`.
//!
//! The words come from `bongos_linear_model::conventions`. The section names identifiers, goals
//! and branch names only, never an artifact, and it is empty for an unlinked task, a hand-set
//! link and a pending create. Both parts do nothing while `linear.enabled` is false (the command
//! toasts that the sync is off, like every command of the sync). Nothing here sends a request.

use bongos_chat::ContextExtension;
use bongos_linear_model::conventions::{context_text, pr_title};
use bongos_work_model::store::WorkTree;
use gpui::{App, ClipboardItem, Context, Window};
use workspace::Workspace;

use crate::actions::show_toast;
use crate::link::selected_task;
use crate::refresh::{enabled, snapshot};

/// The name of the extension among the chats' context extensions.
pub(crate) const EXTENSION: &str = "linear";

/// The toast for `task: copy pr title` on a task with no synced link.
pub(crate) const NO_LINK: &str =
    "This task has no synced Linear link, so it has no PR title (task: link to linear)";

/// The section of the chat of `task_id` on `tree`: a heading and the model's text, or `None` for
/// a task that is not a linked leaf.
pub(crate) fn section_for(tree: &WorkTree, task_id: &str) -> Option<String> {
    let text = context_text(tree, task_id)?;
    Some(format!("## Linear\n\n{text}\n"))
}

pub(crate) struct LinearContext;

impl ContextExtension for LinearContext {
    fn section(&self, _chat_type: &str, task_id: Option<&str>, cx: &App) -> Option<String> {
        if !enabled(cx) {
            return None;
        }
        let tree = snapshot(cx)?;
        section_for(&tree, task_id?)
    }
}

/// Adds the context extension, next to the builder's (its own name, so neither replaces the
/// other).
pub(crate) fn install(cx: &mut App) {
    bongos_chat::add_context_extension(EXTENSION, LinearContext, cx);
}

/// `task: copy pr title`: copies the PR title of the task at point and says so; a task with no
/// synced link says that instead and copies nothing.
pub(crate) fn copy_pr_title(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(task_id) = selected_task(workspace, window, cx) else {
        return;
    };
    let title = tree
        .task_by_id(&task_id)
        .and_then(|task| task.file.as_ref())
        .and_then(pr_title);
    match title {
        Some(title) => {
            let message = format!("Copied PR title: {title}");
            cx.write_to_clipboard(ClipboardItem::new_string(title));
            show_toast(workspace, message, cx);
        }
        None => show_toast(workspace, NO_LINK.to_string(), cx),
    }
}
