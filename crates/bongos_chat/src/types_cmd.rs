//! `chat: edit session type` (design D20): picks a session type and opens its two files,
//! `type.toml` and `context.md`, in editor tabs. When a type has no `context.md`, an
//! empty one is created first (the file is optional and empty when missing, so nothing changes
//! for its chats). Nothing else is written; the chats that run keep the files they were launched
//! with.

use std::path::{Path, PathBuf};

use bongos_work::pick;
use fs::CreateOptions;
use gpui::{Context, TaskExt as _, Window};
use workspace::{OpenOptions, Workspace};

use crate::ChatRuntime;
use crate::actions::chat;
use crate::commands::{chats_on, list_types};
use crate::send::show_toast;

/// The file with the type's settings.
const TYPE_FILE: &str = "type.toml";
/// The file with what the type tells the chat.
const CONTEXT_FILE: &str = "context.md";

/// Registers `chat: edit session type` on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|w, _: &chat::EditSessionType, window, cx| {
        edit_session_type(w, window, cx)
    });
}

/// The files of the type `name` under `types_root`, in the order they are opened.
pub fn type_files(types_root: &Path, name: &str) -> [PathBuf; 2] {
    let dir = types_root.join(name);
    [dir.join(TYPE_FILE), dir.join(CONTEXT_FILE)]
}

fn edit_session_type(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(store) = ChatRuntime::global(cx).map(|runtime| runtime.read(cx).store().clone())
    else {
        return;
    };
    let types_root = store.read(cx).types_root(cx);
    let types = list_types(&types_root);
    if types.is_empty() {
        show_toast(
            workspace,
            format!("There is no session type under {}", types_root.display()),
            cx,
        );
        return;
    }
    let (names, labels): (Vec<String>, Vec<String>) = types.into_iter().unzip();
    pick(
        workspace,
        "Edit which session type?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            open_type_files(
                type_files(&types_root, &names[index]),
                workspace,
                window,
                cx,
            )
        }),
        window,
        cx,
    );
}

/// Opens `files` (the type's `type.toml`, then its `context.md`), making the second one first
/// when it is missing.
fn open_type_files(
    files: [PathBuf; 2],
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.project().read(cx).fs().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let [_, context] = &files;
        fs.create_file(
            context,
            CreateOptions {
                overwrite: false,
                ignore_if_exists: true,
            },
        )
        .await?;
        for path in files {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                })?
                .await?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
