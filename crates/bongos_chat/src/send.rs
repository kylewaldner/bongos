//! The send path (design D13) and the names it gives (D16).
//!
//! [`send`] is the one way text reaches a chat's CLI. It never drops or splits the text: the text
//! goes in as one bracketed paste (`ESC[200~ ... ESC[201~`, so newlines, tabs, quotes and fences
//! arrive as typed), then one Enter after `chats.submit_delay_ms` when asked. A CLI that has not
//! asked for bracketed paste within [`BRACKETED_WAIT`] of its launch is refused, with nothing
//! written to its terminal: a plain paste turns each newline into Enter, one submission per line.
//! The caller keeps its text; on any error it is still there to send again.
//!
//! A successful send marks the chat prompted and, while the chat still has its default name,
//! names it from the text's first line ([`bongos_chat_model::from_prompt`]). A name Kyle chose is
//! never replaced.
//!
//! `chat: send region` sends the selection of the active editor, or its whole buffer, to the chat
//! whose prompts file the editor shows (or, from the prompts pane of the Chats panel, to the chat
//! the panel shows). Focus stays where it is. To a chat that is not running it asks "Resume
//! <name> and send?" and, on yes, resumes the chat and sends once it runs.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bongos_chat_model::{Agent, ChatRecord, NamedBy, from_prompt, timestamp_utc};
use bongos_work_model::layout;
use editor::{Editor, MultiBufferOffset};
use gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity, Window};
use settings::Settings as _;
use terminal::{Modes, Terminal};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::actions::chat;
use crate::live_modes::is_open;
use crate::{ChatPanel, ChatRuntime, ChatsSettings};

/// How long after its launch a chat has to ask for bracketed paste before a send is refused.
pub const BRACKETED_WAIT: Duration = Duration::from_secs(5);
/// How often a send that waits for bracketed paste looks again.
const MODE_POLL: Duration = Duration::from_millis(50);

/// How to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendOptions {
    /// Press Enter after the paste.
    pub submit: bool,
}

/// What a send did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// Enter was pressed after the paste.
    pub submitted: bool,
    /// The name the send gave the chat, when it named it.
    pub named: Option<String>,
}

/// Why a send did not happen. The caller keeps its text whatever the error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// `chats.enabled` is false.
    Off,
    /// The text is empty or only whitespace.
    Empty,
    /// No chat has this id.
    Unknown(String),
    /// The chat has no running process. `chat: send region` asks "Resume <name> and send?".
    NotRunning { name: String },
    /// The CLI did not ask for bracketed paste in time, so a paste would be split into lines.
    NotAcceptingPastes { agent: &'static str },
    /// The text was pasted, but the chat stopped before its Enter. The chat counts as prompted.
    NotSubmitted { name: String },
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Off => write!(f, "Chats are off (chats.enabled)"),
            SendError::Empty => write!(f, "Nothing to send"),
            SendError::Unknown(id) => write!(f, "No chat has the id {id}"),
            SendError::NotRunning { name } => write!(f, "{name} is not running"),
            SendError::NotAcceptingPastes { agent } => {
                write!(f, "{agent} is not accepting pastes")
            }
            SendError::NotSubmitted { name } => {
                write!(
                    f,
                    "The text was pasted into {name} but not submitted: it stopped"
                )
            }
        }
    }
}

impl std::error::Error for SendError {}

/// Sends `text` to the chat `chat_id` (see the module documentation). Refused while chats are off,
/// for an empty text, and for a chat that is not running.
pub fn send(
    chat_id: &str,
    text: String,
    options: SendOptions,
    cx: &mut App,
) -> Task<Result<Sent, SendError>> {
    if !ChatsSettings::get_global(cx).enabled {
        return Task::ready(Err(SendError::Off));
    }
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(Err(SendError::Off));
    };
    runtime.update(cx, |runtime, cx| runtime.send(chat_id, text, options, cx))
}

/// `change` for a record the send or a title names: the name applies only while the chat has its
/// default name, so a name Kyle chose, or an earlier automatic one, stays.
/// Returns whether it applied.
pub(crate) fn apply_name(record: &mut ChatRecord, name: String) -> bool {
    record.named_by == NamedBy::None && record.apply_auto_name(name)
}

impl ChatRuntime {
    /// [`send`] on this runtime.
    pub fn send(
        &mut self,
        chat_id: &str,
        text: String,
        options: SendOptions,
        cx: &mut Context<Self>,
    ) -> Task<Result<Sent, SendError>> {
        if !ChatsSettings::get_global(cx).enabled {
            return Task::ready(Err(SendError::Off));
        }
        if text.trim().is_empty() {
            return Task::ready(Err(SendError::Empty));
        }
        let Some(chat) = self.live(chat_id).filter(|chat| is_open(chat.state())) else {
            return Task::ready(Err(self.not_running(chat_id, cx)));
        };
        let terminal = chat.terminal().clone();
        let agent = chat.agent();
        let deadline = chat.started() + self.bracketed_wait();
        let record_path = chat.record_path().clone();
        let chat_id = chat_id.to_string();
        let (previous, done, number) = self.join_send_queue(&chat_id);
        cx.spawn(async move |this, cx| {
            // One send at a time per chat: this one starts once the one before wrote its Enter.
            if let Some(previous) = previous {
                previous.await.ok();
            }
            let delivered = deliver(
                &this, &chat_id, &terminal, &text, options, agent, deadline, cx,
            )
            .await;
            drop(done);
            this.update(cx, |this, _| this.leave_send_queue(&chat_id, number))
                .ok();
            let submitted = delivered?;

            // The text is in the CLI's input: the record says prompted, last active, and gets the
            // name while it has the default one, even when the Enter could not follow.
            let name = from_prompt(&text);
            let named = Arc::new(Mutex::new(None));
            let named_in_write = named.clone();
            let write = this
                .update(cx, |this, cx| {
                    this.store().clone().update(cx, |store, cx| {
                        store.update(
                            record_path,
                            move |record| {
                                record.prompted = true;
                                record.last_active = bongos_work_model::now();
                                if let Some(name) = name
                                    && apply_name(record, name.clone())
                                {
                                    *named_in_write.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(name);
                                }
                                Ok(())
                            },
                            cx,
                        )
                    })
                })
                .map_err(|_| SendError::Off)?;
            if let Err(error) = write.await {
                // The text was sent; only the record lags until the next write.
                log::warn!("chats: could not record a send: {error}");
            }
            if options.submit && !submitted {
                let name = this
                    .update(cx, |this, cx| match this.not_running(&chat_id, cx) {
                        SendError::NotRunning { name } => name,
                        _ => chat_id.clone(),
                    })
                    .unwrap_or_else(|_| chat_id.clone());
                return Err(SendError::NotSubmitted { name });
            }
            let named = named.lock().unwrap_or_else(|e| e.into_inner()).take();
            Ok(Sent {
                submitted: options.submit,
                named,
            })
        })
    }

    /// Whether `chat_id` still runs, in `terminal` (the one the send began with).
    fn runs_in(&self, chat_id: &str, terminal: &Entity<Terminal>) -> bool {
        self.live(chat_id)
            .is_some_and(|chat| chat.terminal() == terminal && is_open(chat.state()))
    }

    /// The refusal for a chat with no running process: [`SendError::NotRunning`] with its name,
    /// or [`SendError::Unknown`] when no record has the id.
    fn not_running(&self, chat_id: &str, cx: &App) -> SendError {
        let name = self
            .store()
            .read(cx)
            .find(chat_id)
            .and_then(|chat| Some(chat.entry.record()?.name.clone()));
        match name {
            Some(name) => SendError::NotRunning { name },
            None if self.live(chat_id).is_some() => SendError::NotRunning {
                name: chat_id.to_string(),
            },
            None => SendError::Unknown(chat_id.to_string()),
        }
    }
}

/// Pastes `text` once the CLI accepts bracketed pastes, then presses Enter after the submit delay
/// when `options.submit`. An error means nothing was written. `Ok(false)` means the paste went in
/// but the chat stopped before its Enter.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    this: &WeakEntity<ChatRuntime>,
    chat_id: &str,
    terminal: &Entity<Terminal>,
    text: &str,
    options: SendOptions,
    agent: Agent,
    deadline: SystemTime,
    cx: &mut AsyncApp,
) -> Result<bool, SendError> {
    // The check and the paste happen in one update, so the mode cannot change between them.
    loop {
        let pasted = this
            .update(cx, |this, cx| {
                if !this.runs_in(chat_id, terminal) {
                    return Err(this.not_running(chat_id, cx));
                }
                if !accepts_pastes(terminal, cx) {
                    return Ok(false);
                }
                terminal.update(cx, |terminal, _| terminal.paste(text));
                this.note_pasted(chat_id, cx);
                Ok(true)
            })
            .map_err(|_| SendError::Off)??;
        if pasted {
            break;
        }
        let left = deadline
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        if left.is_zero() {
            return Err(SendError::NotAcceptingPastes {
                agent: crate::health::agent_title(agent),
            });
        }
        cx.background_executor().timer(left.min(MODE_POLL)).await;
    }
    if !options.submit {
        return Ok(true);
    }
    // Enter, once the CLI has handled the paste.
    let delay = cx.update(|cx| ChatsSettings::get_global(cx).submit_delay_ms);
    cx.background_executor()
        .timer(Duration::from_millis(delay))
        .await;
    Ok(this
        .update(cx, |this, cx| {
            let running = this.runs_in(chat_id, terminal);
            if running {
                terminal.update(cx, |terminal, _| terminal.input(b"\r".as_slice()));
            }
            running
        })
        .unwrap_or(false))
}

/// Whether the program in `terminal` asked for bracketed paste.
fn accepts_pastes(terminal: &Entity<Terminal>, cx: &App) -> bool {
    terminal
        .read(cx)
        .current_modes()
        .contains(Modes::BRACKETED_PASTE)
}

/// The prompts file of an indexed chat: its `prompts_file` (relative to the scope directory when
/// relative), else the scope's `prompts.txt`.
pub(crate) fn prompts_file(scope_dir: &Path, record: &ChatRecord) -> PathBuf {
    match &record.prompts_file {
        Some(path) => scope_dir.join(path),
        None => layout::prompts_file(scope_dir),
    }
}

/// The chat whose prompts file is `path`: the running one when one runs, else the one active
/// last. Archived chats are left out.
pub fn chat_for_prompts_file(path: &Path, cx: &App) -> Option<String> {
    let runtime = ChatRuntime::global(cx)?;
    let runtime = runtime.read(cx);
    let store = runtime.store().read(cx);
    store
        .index()
        .values()
        .filter_map(|chat| Some((chat, chat.entry.record()?)))
        .filter(|(chat, record)| !record.archived && prompts_file(&chat.scope_dir, record) == path)
        .max_by_key(|(_, record)| {
            (
                is_open(runtime.state(&record.id)),
                timestamp_utc(&record.last_active),
            )
        })
        .map(|(_, record)| record.id.clone())
}

/// A toast of the chat commands.
pub struct ChatToast;

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<ChatToast>(), message),
        cx,
    );
}

/// Registers `chat: send region` on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &chat::SendRegion, window, cx| {
        send_region(workspace, window, cx);
    });
}

/// The selection of `editor`, or its whole buffer when nothing is selected.
fn region_text(editor: &mut Editor, cx: &mut Context<Editor>) -> String {
    let snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
    if selection.is_empty() {
        return editor.text(cx);
    }
    snapshot
        .buffer_snapshot()
        .text_for_range(selection.start..selection.end)
        .collect()
}

/// `chat: send region`: sends the region of the active editor to the chat whose prompts file it
/// shows, or, while the prompts pane of the Chats panel has focus, the region of its editor to the
/// chat the panel shows. The buffer is only read; a refusal leaves it as it was and says why in a
/// toast.
fn send_region(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !ChatsSettings::get_global(cx).enabled {
        show_toast(workspace, SendError::Off.to_string(), cx);
        return;
    }
    let in_panel = workspace
        .panel::<ChatPanel>(cx)
        .and_then(|panel| panel.read(cx).focused_prompts(window, cx));
    let Some(editor) = in_panel
        .as_ref()
        .map(|(editor, _)| editor.clone())
        .or_else(|| workspace.active_item_as::<Editor>(cx))
    else {
        show_toast(
            workspace,
            "Open the prompts file of a chat to send from it".to_string(),
            cx,
        );
        return;
    };
    let (text, path) = editor.update(cx, |editor, cx| {
        (region_text(editor, cx), editor.target_file_abs_path(cx))
    });
    let chat_id = match in_panel {
        Some((_, chat_id)) => Some(chat_id),
        None => path.and_then(|path| chat_for_prompts_file(&path, cx)),
    };
    let Some(chat_id) = chat_id else {
        show_toast(
            workspace,
            "This buffer is not the prompts file of a chat".to_string(),
            cx,
        );
        return;
    };
    let task = send(&chat_id, text.clone(), SendOptions { submit: true }, cx);
    cx.spawn_in(window, async move |workspace, cx| match task.await {
        Ok(_) => {}
        Err(SendError::NotRunning { name }) => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    crate::commands::offer_resume_and_send(
                        chat_id, name, text, workspace, window, cx,
                    )
                })
                .ok();
        }
        Err(error) => {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, error.to_string(), cx)
                })
                .ok();
        }
    })
    .detach();
}
