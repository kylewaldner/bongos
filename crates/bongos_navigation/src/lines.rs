//! The search in buffer picker: one row per line of the current buffer, narrowed by the
//! minibuffer's engine (space-separated terms, any order, smart case).
//!
//! The input starts as the symbol at the cursor. With no symbol the line the cursor is on is
//! selected first. Highlighting a row previews the line, centered and highlighted, in a transient
//! editor in the pane of the editor that asked (see [`crate::preview`]); nothing of the real
//! editor moves. RET pushes the mark, moves the cursor to the first non-blank character of the line
//! and centers it. M-o opens the buffer's file at the line in a chosen pane and leaves the cursor
//! of the editor that asked where it is.

use bongos_minibuffer::{
    Candidate, Outcome, Preview as SessionPreview, Session, SessionHandle, Source,
};
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, Context, Entity, TaskExt as _, Window};
use language::{Buffer, Point};
use project::ProjectPath;
use workspace::{MinibufferMessageKind, MinibufferRouting, Workspace};

use crate::preview::{Preview, PreviewSession, PreviewTarget};
use crate::target::add_pane_choices;

/// The name the picker's session is routed by.
pub const HOST: &str = "search_buffer_lines";

/// The key context of the picker's session.
pub const KEY_CONTEXT: &str = "NavLines";

/// What the echo area says when the active item is not a full editor over one buffer.
pub const NO_EDITOR: &str = "No editor to search";

/// The value of a row: the 0-based row of its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRow(pub u32);

/// Opens the picker over the lines of the active editor's buffer.
pub fn open(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace = cx.entity();
    // The preview session reads the workspace when it is made, which it cannot do while the
    // workspace is being updated.
    window.defer(cx, move |window, cx| {
        start(&workspace, window, cx);
    });
}

/// One row per line of `text`: the line as `primary`, its 1-based number as `secondary`.
pub(crate) fn candidates(text: &str) -> Vec<Candidate> {
    text.split('\n')
        .enumerate()
        .map(|(row, line)| {
            Candidate::new(line.trim_end_matches('\r').to_string(), LineRow(row as u32))
                .secondary((row + 1).to_string())
        })
        .collect()
}

/// The symbol (letters, digits and underscores) the byte `column` of `line` is in or touches.
pub(crate) fn symbol_in(line: &str, column: usize) -> Option<&str> {
    let is_symbol = |c: char| c.is_alphanumeric() || c == '_';
    let column = column.min(line.len());
    if !line.is_char_boundary(column) {
        return None;
    }
    let start = line[..column]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_symbol(*c))
        .last()
        .map_or(column, |(at, _)| at);
    let end = line[column..]
        .char_indices()
        .find(|(_, c)| !is_symbol(*c))
        .map_or(line.len(), |(at, _)| column + at);
    (start < end).then(|| &line[start..end])
}

/// The editor a picker may search: a full editor over a single buffer.
fn searchable(workspace: &Workspace, cx: &App) -> Option<(Entity<Editor>, Entity<Buffer>)> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let ed = editor.read(cx);
    if !ed.mode().is_full() {
        return None;
    }
    let buffer = ed.buffer().read(cx).as_singleton()?;
    Some((editor, buffer))
}

/// Starts the picker. Echoes and returns `None` when the active item is not a searchable editor.
pub(crate) fn start(
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Option<SessionHandle> {
    let Some((editor, buffer)) = searchable(workspace.read(cx), cx) else {
        workspace.update(cx, |_, cx| {
            MinibufferRouting::message(window, cx, NO_EDITOR, MinibufferMessageKind::Message);
        });
        return None;
    };
    let (text, cursor) = {
        let text = buffer.read(cx).text();
        let cursor = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        (text, cursor)
    };
    let seed = text
        .split('\n')
        .nth(cursor.row as usize)
        .and_then(|line| symbol_in(line, cursor.column as usize))
        .map(str::to_string)
        .unwrap_or_default();
    let rows = candidates(&text);
    drop(text);

    let path = buffer
        .read(cx)
        .file()
        .map(|file| ProjectPath::from_file(file.as_ref(), cx));
    let preview = cx.new(|cx| PreviewSession::new(workspace, cx));
    let pane = workspace.read(cx).pane_for(&editor);
    let weak_workspace = workspace.downgrade();
    let preview_buffer = buffer.clone();
    let preview_path = path.clone();
    let origin = editor.downgrade();
    let choice_workspace = weak_workspace;

    let mut session = Session::completing_read(HOST, "Search in buffer: ")
        .key_context(KEY_CONTEXT)
        .source(Source::Fixed(rows))
        .initial_input(seed.clone())
        .on_preview(move |event, window, cx| {
            let event = match event {
                SessionPreview::Highlight(value) => {
                    let row = value.downcast_ref::<LineRow>().map(|row| row.0);
                    match (row, &preview_path, &pane) {
                        (Some(row), Some(path), Some(pane)) => {
                            let length = preview_buffer.read(cx).line_len(row);
                            Preview::Highlight(PreviewTarget::Path {
                                path: path.clone(),
                                row: Some(row),
                                ranges: vec![Point::new(row, 0)..Point::new(row, length)],
                                pane: pane.clone(),
                            })
                        }
                        // A buffer with no file has no preview.
                        _ => Preview::NoCandidate,
                    }
                }
                SessionPreview::Confirmed => Preview::Confirmed,
                SessionPreview::Cancelled => Preview::Cancelled,
            };
            preview.update(cx, |preview, cx| preview.update(event, window, cx));
        })
        .on_confirm({
            let buffer = buffer.clone();
            move |outcome, window, cx| {
                let Outcome::Candidate(value) = outcome else {
                    return;
                };
                let (Some(row), Some(editor)) = (value.downcast_ref::<LineRow>(), origin.upgrade())
                else {
                    return;
                };
                bongos_emacs::mark::push_mark(&editor, None, false, window, cx);
                move_to_row(&editor, &buffer, row.0, window, cx);
            }
        });
    if seed.is_empty() {
        session = session.initial_selection(cursor.row as usize);
    } else {
        // Typing replaces the seed, as in find in files.
        session = session.select_initial_input();
    }
    if let Some(path) = path {
        session = add_pane_choices(
            session,
            choice_workspace.clone(),
            move |value, pane, window, cx| {
                let Some(row) = value.downcast_ref::<LineRow>().map(|row| row.0) else {
                    return;
                };
                let path = path.clone();
                let buffer = buffer.clone();
                choice_workspace
                    .update(cx, |workspace, cx| {
                        let open = workspace.open_path_preview(
                            path,
                            Some(pane.downgrade()),
                            true,
                            false,
                            true,
                            window,
                            cx,
                        );
                        cx.spawn_in(window, async move |_, cx| {
                            let item = open.await?;
                            cx.update(|window, cx| {
                                if let Some(editor) = item.act_as::<Editor>(cx) {
                                    move_to_row(&editor, &buffer, row, window, cx);
                                }
                            })
                        })
                        .detach_and_log_err(cx);
                    })
                    .ok();
            },
        );
    }
    Some(workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx)
    }))
}

/// Moves the one cursor of `editor` to the first non-blank character of `row` and centers it.
fn move_to_row(
    editor: &Entity<Editor>,
    buffer: &Entity<Buffer>,
    row: u32,
    window: &mut Window,
    cx: &mut App,
) {
    let snapshot = buffer.read(cx).snapshot();
    let indent = snapshot
        .indent_size_for_line(row.min(snapshot.max_point().row))
        .len;
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let row = row.min(snapshot.max_point().row);
        let at = snapshot.clip_point(Point::new(row, indent), text::Bias::Left);
        let effects = SelectionEffects::scroll(Autoscroll::center()).nav_history(true);
        editor.change_selections(effects, window, cx, |selections| {
            selections.select_ranges([at..at]);
        });
        editor.set_selection_mark_mode(false, cx);
    });
}

#[cfg(test)]
#[path = "lines_tests.rs"]
mod tests;
