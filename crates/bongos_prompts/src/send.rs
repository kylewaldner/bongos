//! `prompts: send`, `prompts: send without marking` and `prompts: send region` (design D14, D16).
//!
//! A send takes the text of the block at point (or the selection) at once, with anchors on the
//! block's marker row (or its body start when it has no marker) and its end, and sends it to the
//! attached chat ([`crate::attach`]) with the effective auto-submit. Once the text reached the CLI
//! the block is marked sent: its marker row is replaced with the stamped one, or a marker row is
//! inserted above a block without one. The anchors are resolved first, so edits made meanwhile
//! never misplace the mark; a marker that no longer parses with the same created stamp is not
//! marked. Body rows are never rewritten. Then point moves to the next block (a new one after the
//! last), but only while it is still in the sent block, and the file is saved unless it changed on
//! disk. On every error the buffer stays as it was and a toast says why.

use bongos_chat::{ChatsSettings, SendError, SendOptions, Sent};
use bongos_prompts_model::{BlockInfo, mark_sent};
use chrono::NaiveDateTime;
use editor::{Anchor, Editor, MultiBufferOffset, ToPoint as _};
use gpui::{App, Context, Entity, WeakEntity, Window};
use language::Point;
use multi_buffer::MultiBufferRow;
use settings::Settings as _;
use workspace::Workspace;
use workspace::item::{Item as _, SaveOptions};

use crate::PromptsSettings;
use crate::actions::show_toast;
use crate::attach::{Attached, OnSent, effective_auto_submit, file_path, link, with_chat};
use crate::commands::{NO_VIEW_MESSAGE, mode_and_row, new_block, put_at_body, target};
use crate::guard::finalize_own_edit;
use crate::mode::sync;

/// The toast of a send from an empty block.
pub(crate) const EMPTY_MESSAGE: &str = "Nothing to send in this prompt";
/// The question before a block that was sent is sent again.
pub(crate) const AGAIN_QUESTION: &str = "Send this prompt again?";
/// The toast when the block changed between the send and its result.
pub(crate) const CHANGED_MESSAGE: &str = "Sent; the prompt changed meanwhile, so it is not marked";
/// The toast when the file changed on disk, so the send did not save it.
pub(crate) const CONFLICT_MESSAGE: &str =
    "Sent; the prompts file changed on disk, so it is not saved";
/// The question when no chat uses the prompts file.
pub(crate) const START_QUESTION: &str = "No chat uses this prompts file. Start one?";
/// The toast when no chat uses the prompts file and no task owns it.
pub(crate) const NO_TASK_MESSAGE: &str =
    "No task owns this file; start a chat from a task and use this file";

/// Which send command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendKind {
    /// `prompts: send`: confirm a re-send, mark, move, save.
    Mark,
    /// `prompts: send without marking`: only the send.
    WithoutMarking,
    /// `prompts: send region`: the selection exactly, only the send; `Mark` without one.
    Region,
}

/// Where the block a send marks sits, taken with its text.
#[derive(Clone, Debug)]
pub(crate) struct BlockAnchors {
    /// The start of the marker row, or of the first body row of a block without a marker.
    pub(crate) start: Anchor,
    /// The end of the block's last row.
    pub(crate) end: Anchor,
    /// The created stamp of the marker; `None` for a block without one.
    pub(crate) created: Option<NaiveDateTime>,
}

/// The block at point as a send takes it.
pub(crate) struct TakenBlock {
    /// Its text, `None` when it is empty.
    pub(crate) text: Option<String>,
    pub(crate) anchors: BlockAnchors,
    pub(crate) sent: bool,
}

/// What became of the mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marked {
    Marked,
    /// The marker no longer parses with the same created stamp: nothing was changed.
    Changed,
}

/// A send under way.
struct Request {
    editor: Entity<Editor>,
    text: String,
    /// The block to mark once sent; `None` for a send that marks nothing.
    mark: Option<BlockAnchors>,
}

/// The text of the block at point and anchors on it. `None` without the mode.
pub(crate) fn take_block(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<TakenBlock> {
    let (mode, row) = mode_and_row(editor, cx)?;
    let block = mode.borrow().index().block_at(row);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let line = |row: u32| -> String {
        snapshot
            .text_for_range(
                Point::new(row, 0)..Point::new(row, snapshot.line_len(MultiBufferRow(row))),
            )
            .collect()
    };
    let rows: Vec<String> = block.body.clone().map(line).collect();
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let text = bongos_prompts_model::send_text(&rows);
    let start_row = block.marker_row.unwrap_or(block.body.start);
    let end_row = if block.body.is_empty() {
        start_row
    } else {
        block.body.end - 1
    };
    let end = Point::new(end_row, snapshot.line_len(MultiBufferRow(end_row)));
    Some(TakenBlock {
        text: (!text.is_empty()).then_some(text),
        anchors: BlockAnchors {
            start: snapshot.anchor_after(Point::new(start_row, 0)),
            end: snapshot.anchor_after(end),
            created: block.marker.map(|marker| marker.created),
        },
        sent: !block.is_pending(),
    })
}

/// The text of the newest selection exactly, when it is not empty.
pub(crate) fn selection_text(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<String> {
    let snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
    (!selection.is_empty()).then(|| {
        snapshot
            .buffer_snapshot()
            .text_for_range(selection.start..selection.end)
            .collect()
    })
}

/// Marks the block of `anchors` sent `now` with the model's plan: its marker row is replaced, or
/// a marker row is inserted above a block without one. Then, while point is still in that block,
/// point goes to the next block's first body row, or to a new block after the last. `None` when
/// the editor lost the mode, which changes nothing.
pub(crate) fn mark_block(
    editor: &mut Editor,
    anchors: &BlockAnchors,
    now: NaiveDateTime,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Marked> {
    let (mode, _) = mode_and_row(editor, cx)?;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let start = anchors.start.to_point(&snapshot);
    let end = anchors.end.to_point(&snapshot);
    let head = editor.selections.newest_anchor().head().to_point(&snapshot);
    let in_block = start <= head && head <= end;
    if start.column != 0 {
        return Some(Marked::Changed);
    }
    let row = start.row;
    let line_len = snapshot.line_len(MultiBufferRow(row));
    let line: String = snapshot
        .text_for_range(Point::new(row, 0)..Point::new(row, line_len))
        .collect();
    let plan = {
        let mode = mode.borrow();
        let index = mode.index();
        let syntax = index.syntax();
        let parsed = syntax.parse(&line);
        match anchors.created {
            Some(created) => {
                if parsed.map(|marker| marker.created) != Some(created) {
                    return Some(Marked::Changed);
                }
                mark_sent(&index.block_at(row), now, syntax)
            }
            None => {
                if parsed.is_some() {
                    return Some(Marked::Changed);
                }
                let block = BlockInfo {
                    index: 0,
                    id: None,
                    marker_row: None,
                    marker: None,
                    body: row..row,
                };
                mark_sent(&block, now, syntax)
            }
        }
    };
    let text = plan.lines.join("\n");
    let losses = mode.borrow().guard.losses.len();
    if plan.rows.is_empty() {
        let at = Point::new(plan.rows.start, 0);
        editor.edit([(at..at, format!("{text}\n"))], cx);
    } else {
        editor.edit([(Point::new(row, 0)..Point::new(row, line_len), text)], cx);
    }
    {
        let mut mode = mode.borrow_mut();
        sync(&mut mode, editor, cx);
        // Replacing a marker row is the mode's own edit, not a join for the guard.
        mode.guard.losses.truncate(losses);
    }
    finalize_own_edit(editor, cx);
    if in_block {
        let next = mode.borrow().index().next(row, 1);
        match next {
            Some(block) => put_at_body(editor, block.body, window, cx),
            None => {
                new_block(editor, window, cx);
            }
        }
    }
    Some(Marked::Marked)
}

/// `prompts: send`, `prompts: send without marking` and `prompts: send region`.
pub(crate) fn send_command(
    kind: SendKind,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !ChatsSettings::get_global(cx).enabled {
        show_toast(workspace, SendError::Off.to_string(), cx);
        return;
    }
    let Some(editor) = target(workspace, window, cx) else {
        show_toast(workspace, NO_VIEW_MESSAGE.to_string(), cx);
        return;
    };
    if kind == SendKind::Region
        && let Some(text) = editor.update(cx, selection_text)
    {
        let request = Request {
            editor,
            text,
            mark: None,
        };
        to_chat(request, workspace, window, cx);
        return;
    }
    let Some(taken) = editor.update(cx, take_block) else {
        show_toast(workspace, NO_VIEW_MESSAGE.to_string(), cx);
        return;
    };
    let Some(text) = taken.text else {
        show_toast(workspace, EMPTY_MESSAGE.to_string(), cx);
        return;
    };
    let marks = kind != SendKind::WithoutMarking;
    let request = Request {
        editor,
        text,
        mark: marks.then_some(taken.anchors),
    };
    if marks && taken.sent {
        bongos_work::pick(
            workspace,
            AGAIN_QUESTION,
            vec!["Send again".to_string(), "Cancel".to_string()],
            0,
            Box::new(move |index, workspace, window, cx| {
                if index == 0 {
                    to_chat(request, workspace, window, cx);
                }
            }),
            window,
            cx,
        );
        return;
    }
    to_chat(request, workspace, window, cx);
}

/// Sends to the attached chat, or offers to resume or start one.
fn to_chat(
    request: Request,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editor = request.editor.clone();
    with_chat(
        workspace,
        editor,
        window,
        cx,
        move |attached, workspace, window, cx| match attached {
            Attached::Chat(chat_id) => send_to(chat_id, request, window, cx),
            Attached::Closed(chat) => {
                offer_resume(chat.id, chat.name, request, workspace, window, cx)
            }
            Attached::NoChat => offer_start(&request.editor, workspace, window, cx),
            Attached::Several(_) => {}
        },
    );
}

fn options_for(chat_id: &str, cx: &App) -> SendOptions {
    SendOptions {
        submit: effective_auto_submit(Some(chat_id), cx),
    }
}

/// Whether the text reached the CLI: a success, or a paste whose Enter could not follow.
fn delivered(result: &Result<Sent, SendError>) -> bool {
    matches!(result, Ok(_) | Err(SendError::NotSubmitted { .. }))
}

fn send_to(chat_id: String, request: Request, window: &mut Window, cx: &mut Context<Workspace>) {
    let options = options_for(&chat_id, cx);
    let task = link(cx).send(&chat_id, request.text.clone(), options, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = task.await;
        if let Err(SendError::NotRunning { name }) = &result {
            let name = name.clone();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    offer_resume(chat_id, name, request, workspace, window, cx)
                })
                .ok();
            return;
        }
        let note = result.as_ref().err().map(ToString::to_string);
        if let Some(message) = note.clone() {
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        }
        if delivered(&result) {
            cx.update(|window, cx| finish(request, workspace, note, window, cx))
                .ok();
        }
    })
    .detach();
}

/// D16: "Resume <name> and send?". Nothing is kept pending: the mark runs only from the send the
/// offer makes.
fn offer_resume(
    chat_id: String,
    name: String,
    request: Request,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let options = options_for(&chat_id, cx);
    let text = request.text.clone();
    let weak = cx.entity().downgrade();
    let on_sent: OnSent = Box::new(move |result, window, cx| {
        if delivered(&result) {
            finish(request, weak, None, window, cx);
        }
    });
    link(cx).offer_resume_and_send(chat_id, name, text, options, on_sent, workspace, window, cx);
}

/// D16 `NoChat`: offers to start a chat on the task that owns the file; nothing is sent.
fn offer_start(
    editor: &Entity<Editor>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let task = file_path(editor, cx).and_then(|path| link(cx).task_for_dir(path.parent()?, cx));
    let Some(task) = task else {
        show_toast(workspace, NO_TASK_MESSAGE.to_string(), cx);
        return;
    };
    bongos_work::pick(
        workspace,
        START_QUESTION,
        vec!["Start chat".to_string(), "Cancel".to_string()],
        0,
        Box::new(move |index, workspace, window, cx| {
            if index == 0 {
                link(cx).start_chat_on_task(task, workspace, window, cx);
            }
        }),
        window,
        cx,
    );
}

/// After the text reached the CLI: marks, moves and saves (D14 steps 6 to 8). Toasts are shown
/// later, since the workspace may be the entity being updated. `note` is the toast the send
/// itself showed (a paste whose Enter could not follow); every prompts toast shares one id, so the
/// toast of this step would replace it and so repeats it.
fn finish(
    request: Request,
    workspace: WeakEntity<Workspace>,
    note: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(anchors) = request.mark else {
        return;
    };
    let now = chrono::Local::now().naive_local();
    let marked = request.editor.update(cx, |editor, cx| {
        mark_block(editor, &anchors, now, window, cx)
    });
    let Some(marked) = marked else {
        return;
    };
    let mut messages = Vec::new();
    if marked == Marked::Changed {
        messages.push(CHANGED_MESSAGE.to_string());
    }
    if let Some(message) = save(&request.editor, &workspace, window, cx) {
        messages.push(message.to_string());
    }
    if messages.is_empty() {
        return;
    }
    if let Some(note) = note {
        messages.insert(0, note);
    }
    let message = messages.join(". ");
    cx.defer(move |cx| {
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    });
}

/// Saves the prompts file when `prompts.save_on_send` is on, unless it changed on disk (then the
/// returned toast says so). No formatting runs, so the bytes are saved as they are.
/// A failed save is said in a toast.
fn save(
    editor: &Entity<Editor>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Option<&'static str> {
    if !PromptsSettings::get_global(cx).save_on_send {
        return None;
    }
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    if buffer.read(cx).has_conflict() {
        return Some(CONFLICT_MESSAGE);
    }
    let project = editor.read(cx).project().cloned()?;
    let options = SaveOptions {
        format: false,
        force_format: false,
        autosave: false,
    };
    let saving = editor.update(cx, |editor, cx| editor.save(options, project, window, cx));
    let workspace = workspace.clone();
    cx.spawn(async move |cx| {
        if let Err(error) = saving.await {
            log::warn!("prompts: could not save after a send: {error:#}");
            let message = format!("Sent; could not save the prompts file: {error}");
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        }
    })
    .detach();
    None
}
