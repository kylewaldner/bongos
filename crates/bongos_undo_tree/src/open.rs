//! Opening the undo tree panel beside a buffer (`undo_tree::Vundo`).

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{AppContext as _, Context, Entity, Focusable as _, Window};
use language::Buffer;
use workspace::{MinibufferMessageKind, MinibufferRouting, Pane, SplitDirection, Workspace};

use crate::{
    actions::Vundo,
    panel::UndoTreePanel,
    preview::{NEEDS_SINGLE_BUFFER_MESSAGE, PreviewSession},
};

/// Opens the undo tree of the focused editor's buffer beside it, or focuses the one already open.
/// Anything other than a full editor over a single file buffer gets an echo instead.
pub(crate) fn vundo(
    workspace: &mut Workspace,
    _: &Vundo,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = workspace.active_pane().clone();
    let active = pane.read(cx).active_item();
    if let Some(panel) = active
        .as_ref()
        .and_then(|item| item.downcast::<UndoTreePanel>())
    {
        window.focus(&panel.focus_handle(cx), cx);
        return;
    }
    let opened = active
        .and_then(|item| item.downcast::<Editor>())
        .filter(|editor| editor.read(cx).mode().is_full())
        .context(NEEDS_SINGLE_BUFFER_MESSAGE)
        .and_then(|editor| open_beside(workspace, &editor, &pane, window, cx));
    if let Err(error) = opened {
        MinibufferRouting::message(
            window,
            cx,
            &error.to_string(),
            MinibufferMessageKind::Message,
        );
    }
}

/// Opens the undo tree of `editor`'s buffer in a new dedicated pane right of `editor_pane`, which
/// previews the states, and focuses it. When the buffer's tree is already open, that panel is
/// shown and focused instead.
pub fn open_beside(
    workspace: &mut Workspace,
    editor: &Entity<Editor>,
    editor_pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<Entity<UndoTreePanel>> {
    let buffer = editor
        .read(cx)
        .buffer()
        .read(cx)
        .as_singleton()
        .context(NEEDS_SINGLE_BUFFER_MESSAGE)?;
    if let Some(panel) = panel_for_buffer(workspace, &buffer, cx) {
        if let Some(pane) = workspace.pane_for(&panel) {
            pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(&panel) {
                    pane.activate_item(index, true, true, window, cx);
                }
            });
        }
        // Opening the preview again hands back the open session without focusing anything.
        window.focus(&panel.focus_handle(cx), cx);
        return Ok(panel);
    }

    let session = PreviewSession::open(editor, editor_pane, window, cx)?;
    let new_pane = workspace.split_pane(editor_pane.clone(), SplitDirection::Right, window, cx);
    new_pane.update(cx, |pane, cx| pane.set_dedicated(true, cx));
    let home_pane_id = new_pane.entity_id();
    let panel =
        cx.new(|cx| UndoTreePanel::new(session, editor, &buffer, editor_pane, home_pane_id, cx));
    new_pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(panel.clone()), true, true, None, window, cx)
    });
    window.focus(&panel.focus_handle(cx), cx);
    Ok(panel)
}

/// The open undo tree panel of `buffer`, in any pane of `workspace`.
pub fn panel_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<Buffer>,
    cx: &gpui::App,
) -> Option<Entity<UndoTreePanel>> {
    let buffer_id = buffer.entity_id();
    workspace.panes().iter().find_map(|pane| {
        pane.read(cx)
            .items_of_type::<UndoTreePanel>()
            .find(|panel| panel.read(cx).buffer_id() == buffer_id)
    })
}
