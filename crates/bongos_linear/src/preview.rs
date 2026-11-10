//! The preview of an import (D20 step 5): the text of `ImportPlan::preview` in a read-only buffer
//! titled `Import SEQ-120`, opened in the active pane, as `proposal: apply` opens its preview.
//!
//! The buffer is in memory only: it has no path, so opening it creates no file and no journal
//! entry, and closing it asks nothing. Nothing of the import exists before Kyle confirms.

use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, EntityId, TaskExt as _, Window};
use language::Capability;
use workspace::{SaveIntent, Workspace};

use crate::actions::show_toast;

/// What runs once the preview is open, on the workspace, with the id of the opened item.
pub(crate) type Opened =
    Box<dyn FnOnce(&mut Workspace, EntityId, &mut Window, &mut Context<Workspace>)>;

/// The title of the preview of the import of `identifier`.
pub(crate) fn title(identifier: &str) -> String {
    format!("Import {identifier}")
}

/// Opens `text`, the preview of the import of `identifier`, in a read-only buffer, then calls
/// `then`. When the buffer cannot be made, a toast says so and `then` is not called.
pub(crate) fn open(
    identifier: &str,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: Opened,
) {
    let title = title(identifier);
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the preview".to_string(), cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                let id = editor.entity_id();
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                then(workspace, id, window, cx);
            })
            .ok();
    })
    .detach();
}

/// Closes the preview item `id`, wherever it is.
pub(crate) fn close(
    workspace: &mut Workspace,
    id: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    for pane in workspace.panes().to_vec() {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(id, SaveIntent::Skip, window, cx)
                .detach_and_log_err(cx)
        });
    }
}
