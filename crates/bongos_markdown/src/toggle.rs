//! `markdown: toggle live view` (design D5): flips the mode of the active markdown editor's
//! buffer, so every editor of that buffer follows and other buffers stay as they are. Point and
//! the selections are anchors and stay put; the top visible line is put back after the typography
//! and the conceals change. In a prompts file or any other buffer that is not markdown it echoes
//! [`NOT_MARKDOWN`] and changes nothing. It also turns the live view on for a buffer that the size
//! limit opened in source mode.

use editor::Editor;
use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::markdown;
use crate::commands::NOT_MARKDOWN;
use crate::mode::{self, Mode, is_attached, mode_for_buffer, set_mode};

/// Registers the toggle's handler on `workspace`.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &markdown::ToggleLiveView, window, cx| {
        toggle_live_view(workspace, window, cx);
    });
}

fn toggle_live_view(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let editor = workspace
        .active_item_as::<Editor>(cx)
        .filter(|editor| is_attached(editor.read(cx)));
    let Some((editor, buffer)) = editor.and_then(|editor| {
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        Some((editor, buffer))
    }) else {
        bongos_minibuffer::message(window, cx, NOT_MARKDOWN);
        return;
    };
    let next = match mode_for_buffer(&buffer, cx) {
        Some(Mode::Live) => Mode::Source,
        _ => Mode::Live,
    };
    let handle = window.window_handle();
    editor.update(cx, |editor, cx| {
        let top = editor
            .scroll_manager
            .native_anchor(&editor.display_snapshot(cx), cx);
        set_mode(&buffer, next, cx);
        // The other editors of the buffer follow when they see the new mode; this one changes now,
        // so its top line can be put back over the new typography and conceals.
        mode::refresh(editor, Some(handle), cx);
        editor.set_scroll_anchor(top, window, cx);
    });
}
