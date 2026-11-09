//! The preview of an apply (D7 step 6): the text of `bongos_builder_model::preview` in a read-only
//! buffer titled `Proposal N preview`, opened in the active pane, as `chat: show artifacts` opens
//! its list. The text of the last preview is kept for `proposal: dump preview`.

use editor::{Editor, MultiBuffer};
use gpui::{App, AppContext as _, Context, Global, Window};
use language::Capability;
use workspace::Workspace;

use crate::actions::show_toast;

/// What runs once the preview is open, on the workspace.
pub(crate) type Opened = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// The text of the last preview opened in this process.
#[derive(Default)]
struct LastPreview(Option<String>);

impl Global for LastPreview {}

/// The title of the preview of proposal `number`.
pub(crate) fn title(number: u32) -> String {
    format!("Proposal {number} preview")
}

/// The text of the last preview opened, if any.
pub(crate) fn last_preview(cx: &App) -> Option<String> {
    cx.try_global::<LastPreview>()
        .and_then(|last| last.0.clone())
}

/// Opens `text`, the preview of proposal `number`, in a read-only buffer, then calls `then`. When
/// the buffer cannot be made, a toast says so and `then` is not called.
pub(crate) fn open(
    number: u32,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: Opened,
) {
    cx.set_global(LastPreview(Some(text.clone())));
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
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title(number)));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                then(workspace, window, cx);
            })
            .ok();
    })
    .detach();
}
