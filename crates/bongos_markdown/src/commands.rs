//! The handlers of the `markdown` block actions, registered on every workspace. Each one acts on
//! the active editor when it is a markdown editor (it has the [`MarkdownAddon`]) and otherwise
//! echoes [`NOT_MARKDOWN`]. Every text change is one `Editor::transact`, so one undo takes it back.
//!
//! [`MarkdownAddon`]: crate::MarkdownAddon

use std::ops::Range;

use bongos_emacs::prefix_arg;
use bongos_markdown_model::{
    BlockKind, BlockSelection, BlockTree, CellDirection, Direction, LABELS, insert_block,
    move_block, parse, table_cell, toggle_checkbox,
};
use bongos_minibuffer::{Candidate, Outcome, Session, Source};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{App, Context, Entity, Window};
use language::Buffer;
use workspace::Workspace;

use crate::actions::markdown;
use crate::is_attached;

/// The minibuffer host of the insert block prompt.
pub const HOST: &str = "markdown_block";

/// The prompt of the insert block completing read.
pub const INSERT_PROMPT: &str = "Insert block: ";

/// What a command says outside a markdown editor.
pub const NOT_MARKDOWN: &str = "Not a markdown buffer";
/// What a move up says when the block is the first of its siblings.
pub const NO_BLOCK_ABOVE: &str = "No block above";
/// What a move down says when the block is the last of its siblings.
pub const NO_BLOCK_BELOW: &str = "No block below";
/// What the checkbox toggle says off a task item.
pub const NOT_A_TASK: &str = "Not a task item";
/// What the cell moves say past the last cell, before the first, or off a table.
pub const END_OF_TABLE: &str = "End of table";

/// Registers the handlers of the block actions on `workspace`.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &markdown::MoveBlockUp, window, cx| {
        let count = prefix_arg::take(window, cx).map_or(1, |arg| arg.value());
        on_markdown_editor(workspace, window, cx, |editor, window, cx| {
            move_blocks(editor, Direction::Up, count, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &markdown::MoveBlockDown, window, cx| {
        let count = prefix_arg::take(window, cx).map_or(1, |arg| arg.value());
        on_markdown_editor(workspace, window, cx, |editor, window, cx| {
            move_blocks(editor, Direction::Down, count, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &markdown::ToggleCheckbox, window, cx| {
        on_markdown_editor(workspace, window, cx, toggle);
    });
    workspace.register_action(|workspace, _: &markdown::NextTableCell, window, cx| {
        on_markdown_editor(workspace, window, cx, |editor, window, cx| {
            move_to_cell(editor, CellDirection::Next, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &markdown::PreviousTableCell, window, cx| {
        on_markdown_editor(workspace, window, cx, |editor, window, cx| {
            move_to_cell(editor, CellDirection::Previous, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &markdown::InsertBlock, window, cx| {
        if let Some(editor) = markdown_editor(workspace, window, cx) {
            ask_block(workspace, &editor, window, cx);
        }
    });
}

/// The active editor of `workspace` when it is a markdown editor; otherwise echoes
/// [`NOT_MARKDOWN`].
fn markdown_editor(
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<Editor>> {
    let editor = workspace
        .active_item_as::<Editor>(cx)
        .filter(|editor| is_attached(editor.read(cx)));
    if editor.is_none() {
        bongos_minibuffer::message(window, cx, NOT_MARKDOWN);
    }
    editor
}

fn on_markdown_editor(
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>),
) {
    if let Some(editor) = markdown_editor(workspace, window, cx) {
        editor.update(cx, |editor, cx| f(editor, window, cx));
    }
}

fn buffer_of(editor: &Editor, cx: &App) -> Option<Entity<Buffer>> {
    editor.buffer().read(cx).as_singleton()
}

/// The newest selection of `editor` as buffer offsets.
fn newest(editor: &Editor, cx: &mut App) -> BlockSelection {
    let snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
    BlockSelection {
        anchor: selection.tail().0,
        head: selection.head().0,
    }
}

/// Makes `selection` the editor's only selection.
fn select(
    editor: &mut Editor,
    selection: BlockSelection,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let range = MultiBufferOffset(selection.anchor)..MultiBufferOffset(selection.head);
    editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
        selections.select_ranges([range]);
    });
}

fn replace(buffer: &Entity<Buffer>, range: Range<usize>, text: &str, cx: &mut App) {
    buffer.update(cx, |buffer, cx| buffer.edit([(range, text)], None, cx));
}

/// Moves the block at the newest selection (multiple cursors: the others are dropped) `count`
/// times among its siblings, against `direction` when `count` is negative, in one transaction.
/// The selection keeps its offsets inside the moved block. Stopping at the first or last sibling
/// keeps the moves made and echoes [`NO_BLOCK_ABOVE`] or [`NO_BLOCK_BELOW`].
fn move_blocks(
    editor: &mut Editor,
    direction: Direction,
    count: i32,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(buffer) = buffer_of(editor, cx) else {
        return;
    };
    if editor.read_only(cx) {
        return;
    }
    let direction = match (direction, count < 0) {
        (direction, false) => direction,
        (Direction::Up, true) => Direction::Down,
        (Direction::Down, true) => Direction::Up,
    };
    let mut selection = newest(editor, cx);
    let mut text = buffer.read(cx).text();
    let mut edits = Vec::new();
    let mut stopped = false;
    for _ in 0..count.unsigned_abs() {
        let tree = BlockTree::parse(&text);
        match move_block(&text, &tree, selection, direction) {
            Ok(edit) => {
                text = edit.apply(&text);
                selection = edit.selection;
                edits.push(edit);
            }
            Err(_) => {
                stopped = true;
                break;
            }
        }
    }
    if !edits.is_empty() {
        editor.transact(window, cx, |editor, window, cx| {
            for edit in &edits {
                replace(&buffer, edit.range.clone(), &edit.new_text, cx);
            }
            select(editor, selection, window, cx);
        });
    }
    if stopped {
        let text = match direction {
            Direction::Up => NO_BLOCK_ABOVE,
            Direction::Down => NO_BLOCK_BELOW,
        };
        bongos_minibuffer::message(window, cx, text);
    }
}

/// Checks or unchecks the task item at the newest selection's head (multiple cursors: only the
/// newest), in one transaction; off a task item echoes [`NOT_A_TASK`].
fn toggle(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = buffer_of(editor, cx) else {
        return;
    };
    if editor.read_only(cx) {
        return;
    }
    let head = newest(editor, cx).head;
    let text = buffer.read(cx).text();
    let Some(edit) = toggle_checkbox(&text, head) else {
        bongos_minibuffer::message(window, cx, NOT_A_TASK);
        return;
    };
    editor.transact(window, cx, |_, _, cx| {
        replace(&buffer, edit.range, &edit.new_text, cx);
    });
}

/// Moves point to the next or previous cell of the table row at the newest selection's head, in
/// the live view and in source; past either end of the table echoes [`END_OF_TABLE`].
fn move_to_cell(
    editor: &mut Editor,
    direction: CellDirection,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(buffer) = buffer_of(editor, cx) else {
        return;
    };
    let head = newest(editor, cx).head;
    let text = buffer.read(cx).text();
    let index = parse(&text);
    match table_cell(&text, &index, head, direction) {
        Some(offset) => select(editor, BlockSelection::point(offset), window, cx),
        None => bongos_minibuffer::message(window, cx, END_OF_TABLE),
    }
}

/// Asks which block to insert in the minibuffer and inserts it at point in `editor`. Cancelling
/// changes nothing.
fn ask_block(
    workspace: &mut Workspace,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editor = editor.downgrade();
    let candidates = LABELS.iter().map(|label| Candidate::text(*label)).collect();
    let session = Session::completing_read(HOST, INSERT_PROMPT)
        .source(Source::Fixed(candidates))
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(kind) = value
                .downcast_ref::<String>()
                .and_then(|label| BlockKind::from_label(label))
            else {
                return;
            };
            editor
                .update(cx, |editor, cx| insert(editor, kind, window, cx))
                .ok();
        });
    bongos_minibuffer::open(workspace, session, window, cx);
}

/// Inserts a `kind` block at the newest selection's head in one transaction and puts point where
/// the template wants it.
fn insert(editor: &mut Editor, kind: BlockKind, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = buffer_of(editor, cx) else {
        return;
    };
    if editor.read_only(cx) {
        return;
    }
    let head = newest(editor, cx).head;
    let text = buffer.read(cx).text();
    let (edit, point) = insert_block(&text, head, kind);
    editor.transact(window, cx, |editor, window, cx| {
        replace(&buffer, edit.range, &edit.new_text, cx);
        select(editor, BlockSelection::point(point), window, cx);
    });
}
