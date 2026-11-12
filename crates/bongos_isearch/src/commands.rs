//! The commands of a session that build or change the search: the yanks into the string, M-e,
//! the toggles, the jumps to the first and last match, and recenter. Each runs on the workspace's
//! session through `session::register_in_session`; "append" sets the minibuffer input and the
//! model together, so the input, `on_input` and the model stay one path.

use bongos_emacs::recenter::scroll_row_to;
use bongos_emacs::snapshot_text::SnapshotText;
use bongos_emacs_model::TextSource as _;
use bongos_emacs_model::forward_word_target;
use bongos_emacs_model::recenter::Cycle;
use bongos_isearch_model::{Direction, SearchSpec, prompt, to_input};
use gpui::{App, Window};
use multi_buffer::MultiBufferOffset;

use crate::seams::kill_text;
use crate::session::{Command, Shared, handle_of, run, stop_find};

/// The prompt while the string is edited.
const EDIT_PROMPT: &str = "Edit I-search string: ";

/// The offset yanks read from: the end of the current match going forward, its start going
/// backward (where the cursor is), the origin while nothing matched.
fn point(state: &Shared) -> usize {
    let state = state.borrow();
    let step = state.model.current();
    match (&step.found, step.direction) {
        (Some(found), Direction::Forward) => found.end,
        (Some(found), Direction::Backward) => found.start,
        (None, _) => state.model.origin(),
    }
}

/// Reads the text yanked from the editor's text at `point`.
fn read_text<R>(
    state: &Shared,
    cx: &App,
    read: impl FnOnce(&SnapshotText, usize) -> R,
) -> Option<R> {
    let from = point(state);
    let editor = state.borrow().editor.upgrade()?;
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let text = SnapshotText::new(&snapshot);
    Some(read(&text, from.min(text.len())))
}

/// Puts `string` in the input and searches for it, from the first step that is a prefix of it.
fn set_string(state: &Shared, string: String, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let request = {
        let mut state = state.borrow_mut();
        state.string = string.clone();
        state.ring_position = None;
        state.model.set_string(&string)
    };
    handle.set_input(&to_input(&string), window, cx);
    run(state, request, &handle, window, cx);
}

fn append(state: &Shared, text: Option<String>, window: &mut Window, cx: &mut App) {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return;
    };
    let string = format!("{}{text}", state.borrow().string);
    set_string(state, string, window, cx);
}

/// A yank reads where the search stands, so while a find runs it waits for it and runs when it
/// lands. True when it was put off.
fn wait_for_find(state: &Shared, command: Command) -> bool {
    let mut state = state.borrow_mut();
    let running = state.find_running();
    if running {
        state.queued_yanks.push(command);
    }
    running
}

/// C-w: from the match end to the end of the next word; repeated, it extends.
pub(crate) fn yank_word(state: &Shared, window: &mut Window, cx: &mut App) {
    if wait_for_find(state, yank_word) {
        return;
    }
    let text = read_text(state, cx, |text, from| {
        let to = forward_word_target(text, from, None);
        slice(text, from, to)
    });
    append(state, text, window, cx);
}

fn slice(text: &SnapshotText, start: usize, end: usize) -> String {
    text.snapshot()
        .text_for_range(MultiBufferOffset(start)..MultiBufferOffset(end))
        .collect()
}

/// C-M-y: the next character.
pub(crate) fn yank_char(state: &Shared, window: &mut Window, cx: &mut App) {
    if wait_for_find(state, yank_char) {
        return;
    }
    let text = read_text(state, cx, |text, from| {
        text.chars_at(from).next().map(String::from)
    })
    .flatten();
    append(state, text, window, cx);
}

/// M-s C-e: the rest of the line; at its end, the newline.
pub(crate) fn yank_line(state: &Shared, window: &mut Window, cx: &mut App) {
    if wait_for_find(state, yank_line) {
        return;
    }
    let text = read_text(state, cx, |text, from| {
        let line: String = text
            .chars_at(from)
            .take_while(|character| *character != '\n')
            .collect();
        if line.is_empty() && text.chars_at(from).next().is_some() {
            "\n".to_string()
        } else {
            line
        }
    });
    append(state, text, window, cx);
}

/// M-s .: the symbol at the cursor replaces the string and is matched as a whole symbol, until
/// M-r or the end of the session.
pub(crate) fn yank_symbol(state: &Shared, window: &mut Window, cx: &mut App) {
    if wait_for_find(state, yank_symbol) {
        return;
    }
    let symbol = read_text(state, cx, |text, from| {
        let mut start = from;
        for character in text.reversed_chars_at(from) {
            let before = start - character.len_utf8();
            if !text.is_word_char(character, before) {
                break;
            }
            start = before;
        }
        let mut end = from;
        for character in text.chars_at(from) {
            if !text.is_word_char(character, end) {
                break;
            }
            end += character.len_utf8();
        }
        slice(text, start, end)
    });
    let Some(symbol) = symbol.filter(|symbol| !symbol.is_empty()) else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        // Back to the first step, so the symbol is searched from the origin.
        state.model.set_string("");
        let mut spec = state.model.spec().clone();
        spec.symbol = true;
        state.model.set_spec(spec);
    }
    set_string(state, symbol, window, cx);
}

/// C-y: the kill source's top (the clipboard by default), newlines shown as `⏎`.
pub(crate) fn yank_kill(state: &Shared, window: &mut Window, cx: &mut App) {
    append(state, kill_text(cx), window, cx);
}

/// C-j: a newline in the string.
pub(crate) fn quote_newline(state: &Shared, window: &mut Window, cx: &mut App) {
    append(state, Some("\n".to_string()), window, cx);
}

/// M-e: the input is edited as any input; RET, C-s and C-r resume, C-g restores the string.
pub(crate) fn edit_string(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        stop_find(&mut state);
        state.queued_repeats.clear();
        state.queued_yanks.clear();
        state.edit_text = state.string.clone();
        state.editing = Some(state.string.clone());
        state.prompt = EDIT_PROMPT.to_string();
    }
    handle.set_prompt(EDIT_PROMPT, window, cx);
    handle.set_status("", cx);
}

/// The edit ends with RET (`direction` is `None`), C-s or C-r: the edited string is searched from
/// the origin, in the direction of the key (the search's direction for RET).
pub(crate) fn resume_editing(
    state: &Shared,
    direction: Option<Direction>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let request = {
        let mut state = state.borrow_mut();
        state.editing = None;
        let edited = std::mem::take(&mut state.edit_text);
        state.string = edited.clone();
        state.ring_position = None;
        // Back to the first step: the edited string is a new search from the origin.
        state.model.set_string("");
        let direction = direction.unwrap_or_else(|| state.model.direction());
        state.model.repeat(direction, Some(&edited))
    };
    show_prompt(state, &handle, window, cx);
    run(state, request, &handle, window, cx);
}

/// C-g while editing: the string from before M-e comes back and the search goes on.
pub(crate) fn cancel_editing(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let saved = {
        let mut state = state.borrow_mut();
        state.edit_text.clear();
        state.editing.take().unwrap_or_default()
    };
    handle.set_input(&to_input(&saved), window, cx);
    show_prompt(state, &handle, window, cx);
    run(
        state,
        bongos_isearch_model::Request::None,
        &handle,
        window,
        cx,
    );
}

/// Shows the prompt of the model's current state again.
fn show_prompt(
    state: &Shared,
    handle: &bongos_minibuffer::SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let text = {
        let mut state = state.borrow_mut();
        let text = prompt(state.model.current(), state.model.spec());
        state.prompt = text.clone();
        text
    };
    handle.set_prompt(text, window, cx);
}

/// Changes the spec and searches again from the current step's start, which restarts the count.
fn toggle(state: &Shared, change: impl FnOnce(&mut SearchSpec), window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let request = {
        let mut state = state.borrow_mut();
        let mut spec = state.model.spec().clone();
        change(&mut spec);
        state.model.set_spec(spec)
    };
    run(state, request, &handle, window, cx);
}

/// M-c: smart case, then explicit case-sensitive or case-insensitive.
pub(crate) fn toggle_case_fold(state: &Shared, window: &mut Window, cx: &mut App) {
    toggle(state, SearchSpec::toggle_case, window, cx);
}

/// M-r: text and regexp; leaves symbol mode.
pub(crate) fn toggle_regexp(state: &Shared, window: &mut Window, cx: &mut App) {
    toggle(
        state,
        |spec| {
            spec.regexp = !spec.regexp;
            spec.symbol = false;
        },
        window,
        cx,
    );
}

/// M-s SPC: lax or strict spaces.
pub(crate) fn toggle_lax_whitespace(state: &Shared, window: &mut Window, cx: &mut App) {
    toggle(state, |spec| spec.lax = !spec.lax, window, cx);
}

/// M-s M-<: the first match of the buffer.
pub(crate) fn first_match(state: &Shared, window: &mut Window, cx: &mut App) {
    jump(state, true, window, cx);
}

/// M-s M->: the last match of the buffer.
pub(crate) fn last_match(state: &Shared, window: &mut Window, cx: &mut App) {
    jump(state, false, window, cx);
}

fn jump(state: &Shared, first: bool, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let request = {
        let mut state = state.borrow_mut();
        if first {
            state.model.first()
        } else {
            state.model.last()
        }
    };
    run(state, request, &handle, window, cx);
}

/// C-l: the cursor's row (the match's) goes to the middle, top and bottom of the window in turn
/// while C-l repeats, with no cursor moving.
pub(crate) fn recenter(state: &Shared, window: &mut Window, cx: &mut App) {
    let (editor, cycle) = {
        let state = state.borrow();
        (state.editor.upgrade(), Cycle::after(state.cycle_before))
    };
    let Some(editor) = editor else {
        return;
    };
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let row = editor.selections.newest_display(&display).head().row();
        drop(display);
        scroll_row_to(editor, row, cycle, window, cx);
    });
    state.borrow_mut().last_cycle = Some(cycle);
}
