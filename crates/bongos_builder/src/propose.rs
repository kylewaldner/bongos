//! `task: propose` (D13): on the chat the Chats panel shows (else the task at point, else a task
//! picked by name), writes the next numbered proposal file of that chat's task, pre-filled with
//! the task's subtree printed as a proposal and `by chat <id>` (or `by kyle` without a chat).
//! With a chat it pastes a line asking the chat to edit the file, never submitted, so Kyle adds
//! why and presses Enter; without one it opens the file as a buffer.
//!
//! The file is created with `create_new`, so an existing proposal is never overwritten: when the
//! number is taken meanwhile, the next one is tried. An untouched pre-filled file diffs to
//! nothing (see [`is_untouched`]), so it neither applies nor asks for Kyle.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use bongos_builder_model::{Choices, LiveSet, diff, next_number, parse, print};
use bongos_chat::{ChatPanel, ChatStore, SendOptions};
use bongos_work::WorkStore;
use bongos_work::model::layout::{PROPOSALS_DIR, proposal_file};
use bongos_work::model::store::WorkTree;
use fs::{CreateOptions, Fs};
use gpui::{App, Context, Window};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::actions::show_toast;
use crate::live;

/// How many numbers after the first free one `task: propose` tries before it gives up.
const MAX_TRIES: u32 = 100;

/// The `by` line of a proposal written for the chat `chat_id`, or for Kyle without a chat.
pub(crate) fn by_line(chat_id: Option<&str>) -> String {
    match chat_id {
        Some(id) => format!("by chat {id}"),
        None => "by kyle".to_string(),
    }
}

/// What the chat is asked once its proposal file is written.
pub(crate) fn hint(number: u32, path: &Path) -> String {
    format!(
        "Edit proposal {number} at {} to add the tasks you found; the rules are in its header.",
        path.display()
    )
}

/// The printed subtree `printed` (from [`print`]) with the header naming proposal `number` and
/// the `by` line `by`. Only the header, the lines before the first empty one, is touched.
pub(crate) fn prefilled(printed: &str, number: u32, by: &str) -> String {
    let mut in_header = true;
    let mut text = String::with_capacity(printed.len() + by.len());
    for line in printed.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        let ending = &line[bare.len()..];
        if bare.is_empty() {
            in_header = false;
        }
        if in_header && bare.starts_with("proposal ") {
            text.push_str(&format!("proposal {number}{ending}"));
        } else if in_header && bare == "by kyle" {
            text.push_str(by);
            text.push_str(ending);
        } else {
            text.push_str(line);
        }
    }
    text
}

/// Whether the proposal `text` changes nothing in `tree` with the running work `live`: its diff
/// is empty, as an untouched file written by `task: propose` is. Such a proposal neither applies
/// nor asks for Kyle (D13): the watcher and the needs-you source skip it.
pub fn is_untouched(text: &str, tree: &WorkTree, live: &LiveSet) -> bool {
    diff(&parse(text), tree, live, &Choices::default()).is_empty()
}

/// Writes `text(n)` to `<task_dir>/proposals/<n>.proposal.txt` for the first `n` from `first`
/// whose file does not exist, creating it with `create_new` so no existing file is ever
/// overwritten, even one that appears meanwhile. Returns the number and the path written.
pub(crate) async fn write_next(
    fs: &dyn Fs,
    task_dir: &Path,
    first: u32,
    text: impl Fn(u32) -> String,
) -> Result<(u32, PathBuf)> {
    let dir = task_dir.join(PROPOSALS_DIR);
    fs.create_dir(&dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;
    for number in first..first.saturating_add(MAX_TRIES) {
        let path = proposal_file(task_dir, number);
        let created = fs
            .create_file(
                &path,
                CreateOptions {
                    overwrite: false,
                    ignore_if_exists: false,
                },
            )
            .await;
        match created {
            Ok(()) => {
                fs.write(&path, text(number).as_bytes())
                    .await
                    .with_context(|| format!("could not write {}", path.display()))?;
                return Ok((number, path));
            }
            // Taken since the number was chosen: try the next one.
            Err(_) if fs.metadata(&path).await.ok().flatten().is_some() => continue,
            Err(error) => return Err(error),
        }
    }
    bail!(
        "no free proposal number from {first} to {}",
        first.saturating_add(MAX_TRIES) - 1
    )
}

/// The chat the Chats panel of `workspace` shows and its task, when that chat is on a task.
fn shown_chat_task(workspace: &Workspace, cx: &App) -> Option<(String, String)> {
    let panel = workspace.panel::<ChatPanel>(cx)?;
    let chat_id = panel.read(cx).shown_chat()?.to_string();
    let store = ChatStore::global(cx)?;
    let task_id = store.read(cx).find(&chat_id)?.task_id.clone()?;
    Some((chat_id, task_id))
}

/// `task: propose`.
pub(crate) fn propose(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if bongos_work::ready(workspace, cx).is_none() {
        return;
    }
    if let Some((chat_id, task_id)) = shown_chat_task(workspace, cx) {
        propose_on(task_id, Some(chat_id), workspace, window, cx);
        return;
    }
    match bongos_work::task_at_point(workspace, window, cx) {
        Some(id) => propose_on(id, None, workspace, window, cx),
        None => bongos_work::with_task(
            workspace,
            window,
            cx,
            "Propose for which task?",
            "There is no task to propose for",
            |task, _| task.file.is_some(),
            Box::new(|id, workspace, window, cx| propose_on(id, None, workspace, window, cx)),
        ),
    }
}

/// Writes the next proposal file of the task `task_id` for the chat `chat_id` (or Kyle), then
/// pastes the hint into the chat without submitting it, or opens the file without a chat.
pub(crate) fn propose_on(
    task_id: String,
    chat_id: Option<String>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(store) = WorkStore::global(cx) else {
        return;
    };
    let Some(task) = tree.task_by_id(&task_id) else {
        show_toast(workspace, format!("No task has the id {task_id}"), cx);
        return;
    };
    let live = live::collect(&tree, cx);
    let Some(printed) = print(&tree, &task_id, &live) else {
        show_toast(
            workspace,
            format!(
                "The task file of {} does not read; nothing was written",
                task.slug
            ),
            cx,
        );
        return;
    };
    let first = next_number(task);
    let task_dir = task.dir.clone();
    let by = by_line(chat_id.as_deref());
    let fs = store.read(cx).fs();
    cx.spawn_in(window, async move |workspace, cx| {
        let written = write_next(fs.as_ref(), &task_dir, first, |number| {
            prefilled(&printed, number, &by)
        })
        .await;
        workspace
            .update_in(cx, |workspace, window, cx| match written {
                Ok((number, path)) => match chat_id {
                    Some(chat_id) => paste_hint(chat_id, number, &path, cx),
                    None => open_file(path, workspace, window, cx),
                },
                Err(error) => show_toast(
                    workspace,
                    format!("Could not write the proposal: {error:#}"),
                    cx,
                ),
            })
            .ok();
    })
    .detach();
}

/// The last paste of `task: propose`, for tests: the chat, the text and whether it was submitted.
#[cfg(test)]
pub(crate) struct LastPaste(pub String, pub String, pub bool);

#[cfg(test)]
impl gpui::Global for LastPaste {}

/// Pastes the hint of proposal `number` into the chat, never submitted: Kyle adds why and
/// presses Enter.
fn paste_hint(chat_id: String, number: u32, path: &Path, cx: &mut Context<Workspace>) {
    let text = hint(number, path);
    let options = SendOptions { submit: false };
    #[cfg(test)]
    cx.set_global(LastPaste(chat_id.clone(), text.clone(), options.submit));
    let send = bongos_chat::send(&chat_id, text, options, cx);
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = send.await {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!(
                            "Wrote proposal {number}, but could not paste into the chat: {error}"
                        ),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
}

/// Opens the written file as a buffer, in the `Proposal` language.
fn open_file(
    path: PathBuf,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let open = workspace.open_abs_path(
        path,
        OpenOptions {
            visible: Some(OpenVisible::None),
            ..OpenOptions::default()
        },
        window,
        cx,
    );
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = open.await {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!("Could not open the proposal: {error}"),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
}
