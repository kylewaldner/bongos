//! The navigation, new and copy commands of the prompts mode (design D12, D19, D24):
//! `prompts: new`, `previous`, `next`, `next pending` and `copy`. Each works on the prompts editor
//! the command is for (the focused one, else the active item, else the one focused last) and
//! says in a toast what it could not do. The core of each command is a function of the editor, so
//! tests can read its outcome; the handlers only pick the editor and show the toast.

use std::ops::Range;

use bongos_prompts_model::new_block_after;
use editor::{Editor, SelectionEffects};
use gpui::{ClipboardItem, Context, Entity, Focusable as _, Window};
use language::Point;
use workspace::Workspace;

use crate::actions::{prompts, show_toast};
use crate::guard::finalize_own_edit;
use crate::mode::{head_row, is_attached, last_focused, mode_of, sync};

/// The toast of a `prompts: previous` on the first prompt.
pub(crate) const FIRST_MESSAGE: &str = "This is the first prompt";
/// The toast of a `prompts: next` on the last prompt.
pub(crate) const LAST_MESSAGE: &str = "This is the last prompt";
/// The toast of a `prompts: next pending` that went around the end of the file.
pub(crate) const WRAPPED_MESSAGE: &str = "Wrapped around to the first pending prompt";
/// The toast of a `prompts: next pending` when no other prompt is pending.
pub(crate) const NO_PENDING_MESSAGE: &str = "No other pending prompt";
/// The toast of a `prompts: copy` with nothing to copy.
pub(crate) const NOTHING_TO_COPY_MESSAGE: &str = "Nothing to copy: this prompt is empty";
/// The toast of a `prompts: copy`.
pub(crate) const COPIED_MESSAGE: &str = "Copied";

/// The toast of a command with no prompts editor to act on.
pub(crate) const NO_VIEW_MESSAGE: &str = "No prompts view";

/// What a navigation command did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Moved {
    /// Point went to the start of this block (counted from 0).
    To(usize),
    /// `next pending` went around the end of the file to this block.
    Wrapped(usize),
    /// Point was on the first or last block already.
    AtEnd,
    /// No other block is pending.
    NoPending,
}

impl Moved {
    fn message(self, end: &'static str) -> Option<String> {
        match self {
            Moved::To(_) => None,
            Moved::Wrapped(_) => Some(WRAPPED_MESSAGE.to_string()),
            Moved::AtEnd => Some(end.to_string()),
            Moved::NoPending => Some(NO_PENDING_MESSAGE.to_string()),
        }
    }
}

/// What a copy did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Copied {
    Block,
    Selection,
    Nothing,
}

/// The prompts editor a command acts on: the one with focus, else the active item when it has
/// the mode, else the one focused last (the prompts pane of the Chats panel is not an item).
pub(crate) fn target(
    workspace: &Workspace,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<Editor>> {
    let last = last_focused(cx);
    if let Some(last) = last.as_ref()
        && last.focus_handle(cx).contains_focused(window, cx)
    {
        return Some(last.clone());
    }
    workspace
        .active_item_as::<Editor>(cx)
        .filter(|editor| is_attached(editor.read(cx)))
        .or(last)
}

/// Runs `command` on the target editor, or says there is none.
fn with_target<T>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    command: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) -> Option<T>,
) -> Option<T> {
    let Some(editor) = target(workspace, window, cx) else {
        show_toast(workspace, NO_VIEW_MESSAGE.to_string(), cx);
        return None;
    };
    editor.update(cx, |editor, cx| command(editor, window, cx))
}

/// `prompts: new`.
pub(crate) fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_target(workspace, window, cx, |editor, window, cx| {
        new_block(editor, window, cx).then_some(())
    });
}

/// `prompts: previous`.
pub(crate) fn previous(
    workspace: &mut Workspace,
    action: &prompts::Previous,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let count = action.count;
    let moved = with_target(workspace, window, cx, |editor, window, cx| {
        previous_block(editor, count, window, cx)
    });
    if let Some(message) = moved.and_then(|moved| moved.message(FIRST_MESSAGE)) {
        show_toast(workspace, message, cx);
    }
}

/// `prompts: next`.
pub(crate) fn next(
    workspace: &mut Workspace,
    action: &prompts::Next,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let count = action.count;
    let moved = with_target(workspace, window, cx, |editor, window, cx| {
        next_block(editor, count, window, cx)
    });
    if let Some(message) = moved.and_then(|moved| moved.message(LAST_MESSAGE)) {
        show_toast(workspace, message, cx);
    }
}

/// `prompts: next pending`.
pub(crate) fn next_pending(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let moved = with_target(workspace, window, cx, next_pending_block);
    if let Some(message) = moved.and_then(|moved| moved.message(NO_PENDING_MESSAGE)) {
        show_toast(workspace, message, cx);
    }
}

/// `prompts: copy`.
pub(crate) fn copy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let copied = with_target(workspace, window, cx, |editor, _, cx| {
        Some(copy_text(editor, cx))
    });
    match copied {
        Some(Copied::Nothing) => show_toast(workspace, NOTHING_TO_COPY_MESSAGE.to_string(), cx),
        Some(_) => show_toast(workspace, COPIED_MESSAGE.to_string(), cx),
        None => {}
    }
}

/// Puts point at `point` (or the end of the file when it is past it) and scrolls it into view.
fn put_point(editor: &mut Editor, point: Point, window: &mut Window, cx: &mut Context<Editor>) {
    let max = editor.buffer().read(cx).snapshot(cx).max_point();
    let point = point.min(max);
    editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
        selections.select_ranges([point..point])
    });
}

/// Puts point at the first body row of a block. A block with no body has no such row; point goes
/// to the row where its body would be, which point skipping moves off a marker.
pub(crate) fn put_at_body(
    editor: &mut Editor,
    body: Range<u32>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    put_point(editor, Point::new(body.start, 0), window, cx);
}

/// The mode of `editor` with its index up to date, and the row of its newest head.
pub(crate) fn mode_and_row(
    editor: &mut Editor,
    cx: &mut Context<Editor>,
) -> Option<(std::rc::Rc<std::cell::RefCell<crate::PromptsMode>>, u32)> {
    let mode = mode_of(editor)?;
    sync(&mut mode.borrow_mut(), editor, cx);
    let row = head_row(editor, cx);
    Some((mode, row))
}

/// Inserts a new pending block after the one at point and puts point in its body: at the end of
/// the file a marker row and an empty body row are appended; in the middle they go where the
/// block's body ends. Returns whether it did.
pub(crate) fn new_block(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    let Some((mode, row)) = mode_and_row(editor, cx) else {
        return false;
    };
    let plan = {
        let mode = mode.borrow();
        let block = mode.index().block_at(row);
        let now = chrono::Local::now().naive_local();
        (
            new_block_after(&block, now, mode.index().syntax()),
            mode.index().line_count(),
        )
    };
    let (plan, line_count) = plan;
    let at = plan.rows.start;
    let body = plan.lines.join("\n");
    if at >= line_count {
        // After the last row: a line break first, so the marker starts a row of its own.
        let end = editor.buffer().read(cx).snapshot(cx).max_point();
        editor.edit([(end..end, format!("\n{body}"))], cx);
    } else {
        let start = Point::new(at, 0);
        editor.edit([(start..start, format!("{body}\n"))], cx);
    }
    // The edit is the addon's own: close its undo step, so a join typed next is reverted alone.
    finalize_own_edit(editor, cx);
    put_at_body(editor, at + 1..at + 2, window, cx);
    true
}

/// Moves point to the start of the block `count` (1 when `None`) before the current one.
pub(crate) fn previous_block(
    editor: &mut Editor,
    count: Option<u32>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Moved> {
    let (mode, row) = mode_and_row(editor, cx)?;
    let count = count.unwrap_or(1).max(1) as usize;
    let block = mode.borrow().index().previous(row, count);
    Some(match block {
        Some(block) => {
            put_at_body(editor, block.body, window, cx);
            Moved::To(block.index)
        }
        None => Moved::AtEnd,
    })
}

/// Moves point to the start of the block `count` (1 when `None`) after the current one.
pub(crate) fn next_block(
    editor: &mut Editor,
    count: Option<u32>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Moved> {
    let (mode, row) = mode_and_row(editor, cx)?;
    let count = count.unwrap_or(1).max(1) as usize;
    let block = mode.borrow().index().next(row, count);
    Some(match block {
        Some(block) => {
            put_at_body(editor, block.body, window, cx);
            Moved::To(block.index)
        }
        None => Moved::AtEnd,
    })
}

/// Moves point to the start of the next pending block, going around the end of the file once.
pub(crate) fn next_pending_block(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Moved> {
    let (mode, row) = mode_and_row(editor, cx)?;
    let found = mode.borrow().index().next_pending(row);
    Some(match found {
        Some((block, wrapped)) => {
            put_at_body(editor, block.body, window, cx);
            if wrapped {
                Moved::Wrapped(block.index)
            } else {
                Moved::To(block.index)
            }
        }
        None => Moved::NoPending,
    })
}

/// Copies the selection when it is not empty, else the text of the block at point (its body
/// without the blank rows around it), to the clipboard and to the kill ring, so `C-y` yanks it.
pub(crate) fn copy_text(editor: &mut Editor, cx: &mut Context<Editor>) -> Copied {
    let Some((mode, row)) = mode_and_row(editor, cx) else {
        return Copied::Nothing;
    };
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let selected: Vec<String> = {
        let display = editor.display_snapshot(cx);
        editor
            .selections
            .all::<Point>(&display)
            .iter()
            .filter(|selection| !selection.is_empty())
            .map(|selection| snapshot.text_for_range(selection.range()).collect())
            .collect()
    };
    let (text, copied) = if selected.is_empty() {
        let body = mode.borrow().index().block_at(row).body;
        let rows: Vec<String> = body
            .map(|row| {
                snapshot
                    .text_for_range(
                        Point::new(row, 0)
                            ..Point::new(row, snapshot.line_len(multi_buffer::MultiBufferRow(row))),
                    )
                    .collect()
            })
            .collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        (bongos_prompts_model::send_text(&rows), Copied::Block)
    } else {
        (selected.join("\n"), Copied::Selection)
    };
    if text.is_empty() {
        return Copied::Nothing;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
    editor.set_kill_ring_text(text, cx);
    copied
}
