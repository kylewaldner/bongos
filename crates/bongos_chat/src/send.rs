//! The send path (design D13) and the names it gives (D16).
//!
//! [`send`] is the one way text reaches a chat's CLI. It never drops or splits the text: the text
//! goes in as one bracketed paste (`ESC[200~ ... ESC[201~`, so newlines, tabs, quotes and fences
//! arrive as typed), then, when asked, Enter. A CLI that has not asked for bracketed paste within
//! [`BRACKETED_WAIT`] of its launch is refused, with nothing written to its terminal: a plain paste
//! turns each newline into Enter, one submission per line. The caller keeps its text; on any error
//! it is still there to send again.
//!
//! The Enter waits until the terminal has been quiet for 100 ms after the paste (at most 1 s plus
//! 1 s per 256 KiB of text, and never more than 30 s), so a large paste is handled before it. A
//! chat whose channel reports submits gets its Enter then, and the send waits up to
//! [`CONFIRM_WAIT`] for the report of the submit, pressing Enter once more when the chat was idle
//! and none came ([`SendError::NotConfirmed`] when the second one is not reported either). Any
//! other chat gets its Enter `chats.submit_delay_ms` after the paste at the earliest.
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

use bongos_chat_model::{Agent, ChatRecord, NamedBy, Timestamp, from_prompt, timestamp_utc};
use bongos_work_model::layout;
use editor::{Editor, MultiBufferOffset};
use gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity, Window};
use settings::Settings as _;
use terminal::{Modes, Terminal};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::actions::chat;
use crate::live_modes::is_open;
use crate::{ChatPanel, ChatRuntime, ChatState, ChatStore, ChatWriteError, ChatsSettings, Source};

/// How long after its launch a chat has to ask for bracketed paste before a send is refused.
pub const BRACKETED_WAIT: Duration = Duration::from_secs(5);
/// How often a send that waits for bracketed paste looks again.
const MODE_POLL: Duration = Duration::from_millis(50);
/// How long a send waits, after its Enter, for the chat's channel to report the submit.
pub const CONFIRM_WAIT: Duration = Duration::from_secs(3);
/// The terminal counts as settled after a paste once it was quiet this long.
const SETTLE_QUIET: Duration = Duration::from_millis(100);
/// The longest settle wait, whatever the size of the text.
const SETTLE_CAP: Duration = Duration::from_secs(30);
/// How often a send that waits for the terminal to settle, or for a confirmation, looks again.
const SEND_POLL: Duration = Duration::from_millis(20);

/// The longest a send waits for the terminal to settle after pasting `len` bytes: 1 s, plus 1 s
/// per 256 KiB, at most [`SETTLE_CAP`].
pub(crate) fn settle_limit(len: usize) -> Duration {
    let per_size = u64::try_from(len).unwrap_or(u64::MAX).saturating_mul(1000) / (256 * 1024);
    Duration::from_millis(1000u64.saturating_add(per_size)).min(SETTLE_CAP)
}

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
    /// The chat's channel reported the submit. False for a chat without such a channel, and for
    /// a send into a turn that is running (the CLI takes the prompt when the turn ends).
    pub confirmed: bool,
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
    /// The text was pasted and Enter pressed (twice), but the chat's channel never reported the
    /// submit. The chat counts as prompted.
    NotConfirmed { name: String },
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
            SendError::NotConfirmed { name } => {
                write!(
                    f,
                    "Pasted into {name}, but the submit was not confirmed; check the chat"
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
            // One send at a time per chat: this one starts once the one before is done with its
            // Enter (and its confirmation), so a retried Enter never submits another send's text.
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
            let delivered = delivered?;

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
            let name = |cx: &mut AsyncApp| {
                this.update(cx, |this, cx| match this.not_running(&chat_id, cx) {
                    SendError::NotRunning { name } => name,
                    _ => chat_id.clone(),
                })
                .unwrap_or_else(|_| chat_id.clone())
            };
            let confirmed = match delivered {
                Delivered::Pasted => false,
                Delivered::Submitted { confirmed } => confirmed,
                Delivered::Stopped => return Err(SendError::NotSubmitted { name: name(cx) }),
                Delivered::Unconfirmed => return Err(SendError::NotConfirmed { name: name(cx) }),
            };
            let named = named.lock().unwrap_or_else(|e| e.into_inner()).take();
            Ok(Sent {
                submitted: options.submit,
                confirmed,
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

/// What [`deliver`] did once the text was pasted.
enum Delivered {
    /// Pasted, without Enter (none was asked).
    Pasted,
    /// Enter was pressed; `confirmed` when the chat's channel reported the submit.
    Submitted { confirmed: bool },
    /// Pasted, but the chat stopped before its Enter.
    Stopped,
    /// Enter was pressed, and once more, but no submit was reported.
    Unconfirmed,
}

/// The chat as a send's Enter found it.
struct AtEnter {
    state: ChatState,
    source: Source,
    /// The chat's channel reports submits.
    reports: bool,
    /// Its count of applied submit reports before the Enter.
    submits: u64,
}

/// Pastes `text` once the CLI accepts bracketed pastes and, when `options.submit`, presses Enter
/// once the terminal settled (see the module documentation). An error means nothing was written.
///
/// A chat whose channel reports submits has its Enter confirmed: a send into a running turn is
/// not (the CLI takes it when the turn ends, which may be minutes later); otherwise the send waits
/// for the report of the submit, and when none comes and the chat was idle at the Enter, presses
/// Enter once more and waits again. The second Enter is pressed only while no submit was reported,
/// so the text is never submitted twice. Any other chat gets its Enter `chats.submit_delay_ms`
/// after the paste at the earliest.
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
) -> Result<Delivered, SendError> {
    // The check and the paste happen in one update, so the mode cannot change between them.
    let pasted_at = loop {
        let pasted = this
            .update(cx, |this, cx| {
                if !this.runs_in(chat_id, terminal) {
                    return Err(this.not_running(chat_id, cx));
                }
                if !accepts_pastes(terminal, cx) {
                    return Ok(None);
                }
                this.write_send_input(
                    chat_id,
                    terminal,
                    false,
                    |terminal| terminal.paste(text),
                    cx,
                );
                this.note_pasted(chat_id, cx);
                Ok(Some(this.now_ms()))
            })
            .map_err(|_| SendError::Off)??;
        if let Some(pasted_at) = pasted {
            break pasted_at;
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
    };
    if !options.submit {
        return Ok(Delivered::Pasted);
    }
    settle(this, chat_id, terminal, pasted_at, text.len(), cx).await;

    let reports = this
        .read_with(cx, |this, _| {
            this.live(chat_id)
                .is_some_and(|chat| chat.terminal() == terminal && chat.reports_submits())
        })
        .unwrap_or(false);
    if !reports {
        // No channel says when the CLI took the paste: wait the configured delay from the paste.
        let delay =
            Duration::from_millis(cx.update(|cx| ChatsSettings::get_global(cx).submit_delay_ms));
        let elapsed = elapsed_since(this, pasted_at, cx);
        if elapsed < delay {
            cx.background_executor().timer(delay - elapsed).await;
        }
    }

    let Some(at_enter) = press_enter(this, chat_id, terminal, None, cx) else {
        return Ok(Delivered::Stopped);
    };
    if !at_enter.reports {
        return Ok(Delivered::Submitted { confirmed: false });
    }
    if at_enter.state == ChatState::Running && at_enter.source == Source::Exact {
        // A send into a running turn: the CLI queues it and reports its submit only when it takes
        // it, so there is nothing to wait for.
        return Ok(Delivered::Submitted { confirmed: false });
    }
    if wait_for_submit(this, chat_id, terminal, at_enter.submits, cx).await {
        return Ok(Delivered::Submitted { confirmed: true });
    }
    if at_enter.state != ChatState::Idle {
        // Waiting (a dialog may own the keyboard) or a guessed turn: a second Enter could answer
        // something else, so none is sent.
        return Ok(Delivered::Submitted { confirmed: false });
    }
    match press_enter(this, chat_id, terminal, Some(at_enter.submits), cx) {
        // A submit was reported just before the second Enter, which was not sent.
        Some(AtEnter { submits, .. }) if submits > at_enter.submits => {
            return Ok(Delivered::Submitted { confirmed: true });
        }
        Some(_) => {}
        None => return Ok(Delivered::Unconfirmed),
    }
    if wait_for_submit(this, chat_id, terminal, at_enter.submits, cx).await {
        Ok(Delivered::Submitted { confirmed: true })
    } else {
        Ok(Delivered::Unconfirmed)
    }
}

/// The time since `since` on the runtime's clock.
fn elapsed_since(this: &WeakEntity<ChatRuntime>, since: u64, cx: &mut AsyncApp) -> Duration {
    this.read_with(cx, |this, _| {
        Duration::from_millis(this.now_ms().saturating_sub(since))
    })
    .unwrap_or(Duration::MAX)
}

/// Waits until the terminal of `chat_id` was quiet for [`SETTLE_QUIET`] after the paste at
/// `pasted_at`, at most [`settle_limit`] of the paste's `len`, or until the chat stopped.
async fn settle(
    this: &WeakEntity<ChatRuntime>,
    chat_id: &str,
    terminal: &Entity<Terminal>,
    pasted_at: u64,
    len: usize,
    cx: &mut AsyncApp,
) {
    let limit = settle_limit(len);
    loop {
        let settled = this
            .read_with(cx, |this, _| {
                let Some(chat) = this
                    .live(chat_id)
                    .filter(|chat| chat.terminal() == terminal)
                else {
                    return true;
                };
                let now = this.now_ms();
                let last = chat.last_output_at().unwrap_or(0).max(pasted_at);
                Duration::from_millis(now.saturating_sub(pasted_at)) >= limit
                    || Duration::from_millis(now.saturating_sub(last)) >= SETTLE_QUIET
            })
            .unwrap_or(true);
        if settled {
            return;
        }
        cx.background_executor().timer(SEND_POLL).await;
    }
}

/// Presses Enter in `chat_id` while it still runs in `terminal`, and returns how the chat was at
/// that moment, or `None` when it stopped. With `unless_after`, the Enter is not pressed when the
/// chat applied more submit reports than that already (the returned count says so).
fn press_enter(
    this: &WeakEntity<ChatRuntime>,
    chat_id: &str,
    terminal: &Entity<Terminal>,
    unless_after: Option<u64>,
    cx: &mut AsyncApp,
) -> Option<AtEnter> {
    this.update(cx, |this, cx| {
        if !this.runs_in(chat_id, terminal) {
            return None;
        }
        let chat = this.live(chat_id)?;
        let at_enter = AtEnter {
            state: chat.state(),
            source: chat.state_source(),
            reports: chat.reports_submits(),
            submits: chat.prompt_submits(),
        };
        if unless_after.is_some_and(|before| at_enter.submits > before) {
            return Some(at_enter);
        }
        this.write_send_input(
            chat_id,
            terminal,
            true,
            |terminal| terminal.input(b"\r".as_slice()),
            cx,
        );
        Some(at_enter)
    })
    .ok()
    .flatten()
}

/// Waits up to the runtime's confirm wait for `chat_id` to apply more submit reports than
/// `before`. Returns whether it did; false at once when the chat stopped.
async fn wait_for_submit(
    this: &WeakEntity<ChatRuntime>,
    chat_id: &str,
    terminal: &Entity<Terminal>,
    before: u64,
    cx: &mut AsyncApp,
) -> bool {
    let Ok((start, wait)) = this.read_with(cx, |this, _| (this.now_ms(), this.confirm_wait()))
    else {
        return false;
    };
    loop {
        let seen = this
            .read_with(cx, |this, _| {
                let chat = this
                    .live(chat_id)
                    .filter(|chat| chat.terminal() == terminal)?;
                Some(chat.prompt_submits() > before)
            })
            .ok()
            .flatten();
        match seen {
            Some(true) => return true,
            None => return false,
            Some(false) => {}
        }
        if elapsed_since(this, start, cx) >= wait {
            return false;
        }
        cx.background_executor().timer(SEND_POLL).await;
    }
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

/// A chat that uses a prompts file, as [`chats_for_prompts_file`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRef {
    pub id: String,
    pub name: String,
    /// The chat has a running process.
    pub running: bool,
    pub last_active: Timestamp,
}

/// The chats whose prompts file is `path`, the one [`chat_for_prompts_file`] picks first: running
/// ones before closed ones, then the one active last first. Archived chats are left out.
pub fn chats_for_prompts_file(path: &Path, cx: &App) -> Vec<ChatRef> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Vec::new();
    };
    let runtime = runtime.read(cx);
    let store = runtime.store().read(cx);
    let mut chats: Vec<ChatRef> = store
        .index()
        .values()
        .filter_map(|chat| Some((chat, chat.entry.record()?)))
        .filter(|(chat, record)| !record.archived && prompts_file(&chat.scope_dir, record) == path)
        .map(|(_, record)| ChatRef {
            id: record.id.clone(),
            name: record.name.clone(),
            running: is_open(runtime.state(&record.id)),
            last_active: record.last_active,
        })
        .collect();
    // `max_by_key` in `chat_for_prompts_file` keeps the last of equal keys: so does this order.
    chats.reverse();
    chats.sort_by_key(|chat| std::cmp::Reverse((chat.running, timestamp_utc(&chat.last_active))));
    chats
}

/// The chat whose prompts file is `path`: the running one when one runs, else the one active
/// last. Archived chats are left out.
pub fn chat_for_prompts_file(path: &Path, cx: &App) -> Option<String> {
    chats_for_prompts_file(path, cx)
        .into_iter()
        .next()
        .map(|chat| chat.id)
}

/// The record file and the scope directory of `chat_id`.
fn record_of(
    chat_id: &str,
    cx: &App,
) -> Result<(Entity<ChatStore>, PathBuf, PathBuf), ChatWriteError> {
    let store = ChatRuntime::global(cx)
        .ok_or(ChatWriteError::Off)?
        .read(cx)
        .store()
        .clone();
    let chat = store
        .read(cx)
        .find(chat_id)
        .ok_or_else(|| ChatWriteError::Failed(format!("No chat has the id {chat_id}")))?;
    let (path, scope_dir) = (chat.entry.path.clone(), chat.scope_dir.clone());
    Ok((store, path, scope_dir))
}

/// Writes `change` to the record of `chat_id`.
fn write_record_of(
    chat_id: &str,
    cx: &mut App,
    change: impl FnOnce(&Path, &mut ChatRecord) + Send + 'static,
) -> Task<Result<(), ChatWriteError>> {
    if !ChatsSettings::get_global(cx).enabled {
        return Task::ready(Err(ChatWriteError::Off));
    }
    let (store, path, scope_dir) = match record_of(chat_id, cx) {
        Ok(found) => found,
        Err(error) => return Task::ready(Err(error)),
    };
    let write = store.update(cx, |store, cx| {
        store.update(
            path,
            move |record| {
                change(&scope_dir, record);
                Ok(())
            },
            cx,
        )
    });
    cx.spawn(async move |_| write.await.map(|_| ()))
}

/// Makes `path` the prompts file of `chat_id`. The record keeps `None` when `path` is the scope's
/// own `prompts.txt` (the task's, or the project's), the absolute path otherwise; `None` goes
/// back to the scope's file. A relative `path` is taken from the scope directory.
pub fn set_prompts_file(
    chat_id: &str,
    path: Option<PathBuf>,
    cx: &mut App,
) -> Task<Result<(), ChatWriteError>> {
    write_record_of(chat_id, cx, move |scope_dir, record| {
        record.prompts_file = path
            .map(|path| scope_dir.join(path))
            .filter(|path| *path != layout::prompts_file(scope_dir));
    })
}

/// Sets the auto-submit override of `chat_id`; `None` follows `prompts.auto_submit`.
pub fn set_auto_submit(
    chat_id: &str,
    value: Option<bool>,
    cx: &mut App,
) -> Task<Result<(), ChatWriteError>> {
    write_record_of(chat_id, cx, move |_, record| record.auto_submit = value)
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
                        chat_id,
                        name,
                        text,
                        SendOptions { submit: true },
                        Box::new(|_, _, _| {}),
                        workspace,
                        window,
                        cx,
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
