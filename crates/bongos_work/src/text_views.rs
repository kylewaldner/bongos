//! Two small views other crates open: a read-only buffer that follows a changing text, and a
//! file opened with the cursor on a line.

use std::path::PathBuf;
use std::time::Duration;

use editor::{Editor, MultiBuffer};
use gpui::{App, AppContext as _, Context, Window};
use language::{Capability, Point};
use util::ResultExt as _;
use workspace::{OpenOptions, Workspace};

use crate::commands::show_toast;

/// How often a following buffer asks for the text again.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

/// Opens `text` in a new read-only buffer titled `title`, then asks `next` for the text every
/// half second and shows it when it changed. `next` answers the text now and whether it is the
/// last one; the buffer stops following after that, or when it is closed.
pub fn open_following_text(
    workspace: &mut Workspace,
    title: String,
    text: String,
    next: impl Fn(&App) -> (String, bool) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, format!("Could not open {title}"), cx)
                })
                .log_err();
            return;
        };
        let mut shown = text.clone();
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        let weak = buffer.downgrade();
        workspace
            .update_in(cx, |workspace, window, cx| {
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
            .log_err();
        loop {
            cx.background_executor().timer(FOLLOW_INTERVAL).await;
            let Ok((latest, last)) = cx.update(|_, cx| next(cx)) else {
                break;
            };
            if latest != shown {
                let replaced = weak.update(cx, |buffer, cx| {
                    buffer.set_capability(Capability::ReadWrite, cx);
                    buffer.set_text(latest.clone(), cx);
                    buffer.set_capability(Capability::ReadOnly, cx);
                });
                if replaced.is_err() {
                    break;
                }
                shown = latest;
            }
            if last {
                break;
            }
        }
    })
    .detach();
}

/// Opens `path` with the cursor at the start of the 0-based line `row`.
pub fn open_file_at_line(
    path: PathBuf,
    row: u32,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let open = workspace.open_abs_path(path, OpenOptions::default(), window, cx);
    cx.spawn_in(window, async move |workspace, cx| match open.await {
        Ok(item) => {
            if let Some(editor) = item.downcast::<Editor>() {
                editor
                    .update_in(cx, |editor, window, cx| {
                        editor.go_to_singleton_buffer_point(Point::new(row, 0), window, cx)
                    })
                    .log_err();
            }
        }
        Err(error) => {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, format!("Could not open the file: {error:#}"), cx)
                })
                .log_err();
        }
    })
    .detach();
}
