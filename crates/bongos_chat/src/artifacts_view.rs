//! `chat: show artifacts` (design D20): a read-only buffer with the artifacts of the shown chat.
//!
//! - A running chat shows the list it was given when it started, kept in memory by the runtime
//!   ([`crate::LiveChat::given`]); the view never reads it from a record, because no record holds
//!   a path (only a digest of the list).
//! - When the task's shared list is not the one the chat was given (the digests differ), the view
//!   adds [`STALE_LINE`], and the Chats list shows [`ROW_NOTE`] on the row. Both say nothing about
//!   what changed.
//! - A closed chat shows the list its next start is given: the task's shared list as it is now.
//!
//! Human-only artifacts never appear, for a running chat or a closed one. The current list never
//! holds one, and what a running chat was given is shown only where it is still shared now: an
//! artifact hidden since the start is left out of the view (and named nowhere else), so what is
//! on screen never reveals what Kyle has locked since. When the manifest does not parse the
//! current list is empty, so a running chat then shows nothing.
//!
//! A view reads the task's files and the in-memory list and writes nothing, in particular no
//! record.

use std::path::PathBuf;

use bongos_chat_model::context_digest;
use bongos_work_model::{SharedArtifact, shared_artifacts};
use editor::{Editor, MultiBuffer};
use gpui::{App, AppContext as _, Context, Task, Window};
use language::Capability;
use workspace::Workspace;

use crate::ChatRuntime;
use crate::actions::chat;
use crate::commands::{chat_name, chats_on, shown_chat};
use crate::live_modes::is_open;
use crate::send::show_toast;

/// The line that says the task's artifacts are not the ones the running chat was given.
pub const STALE_LINE: &str = "The task's artifacts changed since this session started; the next \
resume gets the current list";

/// What a row of the Chats list says about a running chat whose artifact list is stale.
pub const ROW_NOTE: &str = "artifact list updates on next resume";

/// Whether `current`, the task's shared artifacts now, is not the list `given`: their digests
/// differ (see [`bongos_chat_model::context_digest`]). Neither list is named.
pub fn is_stale(given: &[SharedArtifact], current: &[SharedArtifact]) -> bool {
    context_digest(given) != context_digest(current)
}

/// The text of the view of the chat `name`. `given` is the list a running chat was given, `None`
/// for a closed chat; `current` is the task's shared list now.
pub fn render(name: &str, given: Option<&[SharedArtifact]>, current: &[SharedArtifact]) -> String {
    let mut text = format!("Artifacts of {name}\n\n");
    let (shown, intro): (Vec<&SharedArtifact>, &str) = match given {
        Some(given) => (
            given
                .iter()
                .filter(|artifact| current.iter().any(|now| now.path == artifact.path))
                .collect(),
            "This session was given these artifacts when it started:",
        ),
        None => (
            current.iter().collect(),
            "The next start of this session is given these artifacts:",
        ),
    };
    text.push_str(intro);
    text.push_str("\n\n");
    if shown.is_empty() {
        text.push_str("(none)\n");
    }
    for artifact in shown {
        text.push_str(&format!("- {}\n", artifact.path.display()));
        if !artifact.description.is_empty() {
            text.push_str(&format!("  {}\n", artifact.description));
        }
    }
    if given.is_some_and(|given| is_stale(given, current)) {
        text.push_str(&format!("\n{STALE_LINE}\n"));
    }
    text
}

/// What `chat_id`'s view says now: reads the task's shared list in the background. `Err` is the
/// text to show.
pub fn artifacts_text(chat_id: &str, cx: &mut App) -> Task<Result<String, String>> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(Err(crate::SendError::Off.to_string()));
    };
    let (indexed, given) = {
        let runtime = runtime.read(cx);
        let Some(indexed) = runtime.store().read(cx).find(chat_id).cloned() else {
            return Task::ready(Err(format!("No chat has the id {chat_id}")));
        };
        let given = runtime
            .live(chat_id)
            .filter(|live| is_open(live.state()))
            .map(|live| live.given().to_vec());
        (indexed, given)
    };
    let name = chat_name(chat_id, cx);
    let scope_dir = indexed.scope_dir;
    cx.background_spawn(async move {
        let current = shared_artifacts(&scope_dir);
        Ok(render(&name, given.as_deref(), &current))
    })
}

/// Whether the chat is running and was given a list other than the task's shared list now: the
/// Chats list puts [`ROW_NOTE`] on its row. Reads the task's files in the background; a chat that
/// does not run is never stale (its next start gets the current list anyway).
pub fn stale_chat(chat_id: &str, cx: &mut App) -> Task<bool> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(false);
    };
    let (scope_dir, given): (PathBuf, Vec<SharedArtifact>) = {
        let runtime = runtime.read(cx);
        let Some(indexed) = runtime.store().read(cx).find(chat_id) else {
            return Task::ready(false);
        };
        let Some(live) = runtime.live(chat_id).filter(|live| is_open(live.state())) else {
            return Task::ready(false);
        };
        (indexed.scope_dir.clone(), live.given().to_vec())
    };
    cx.background_spawn(async move { is_stale(&given, &shared_artifacts(&scope_dir)) })
}

/// Registers `chat: show artifacts` on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &chat::ShowArtifacts, window, cx| show_artifacts(w, window, cx));
}

/// `chat: show artifacts`: the shown chat's list in a read-only buffer.
fn show_artifacts(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    let title = format!("Artifacts: {}", chat_name(&chat_id, cx));
    let text = artifacts_text(&chat_id, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let text = match text.await {
            Ok(text) => text,
            Err(message) => {
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                    .ok();
                return;
            }
        };
        let created = workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
            (project, buffer)
        });
        let Ok((project, buffer)) = created else {
            return;
        };
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the artifacts".to_string(), cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer.set_text(text, cx);
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
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
            .ok();
    })
    .detach();
}
