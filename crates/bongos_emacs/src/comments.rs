//! Comment commands: comment-dwim and duplicate-and-comment. Both lean on Zed's own comment
//! toggles, so the comment syntax comes from the language at the caret. A single-line editor
//! passes the key on: it has no lines to comment.

use std::ops::Range;

use bongos_emacs_model::comment::covers_whole_lines;
use bongos_emacs_model::{Caret, whole_line_span};
use editor::actions::{ToggleBlockComments, ToggleComments};
use editor::{Editor, SelectionEffects};
use gpui::{App, Entity, Window};
use multi_buffer::{MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot};
use text::Point;
use workspace::{MinibufferMessageKind, MinibufferRouting};

use crate::actions::{CommentDwim, DuplicateAndComment};
use crate::lines::propagate_in_single_line;
use crate::region;
use crate::snapshot_text::SnapshotText;

/// What the echo area says when a language has no comment syntax at all.
const NO_COMMENT_SYNTAX: &str = "No comment syntax for this language";

/// Zed's line-comment toggle, as comment-dwim uses it: the caret stays where it is.
fn toggle_line_comments(editor: &mut Editor, window: &mut Window, cx: &mut gpui::Context<Editor>) {
    let action = ToggleComments {
        advance_downwards: false,
        ignore_indent: false,
    };
    editor.toggle_comments(&action, window, cx);
}

fn has_block_comment(snapshot: &MultiBufferSnapshot, offset: usize) -> bool {
    snapshot
        .language_scope_at(MultiBufferOffset(offset))
        .is_some_and(|scope| scope.block_comment().is_some())
}

fn has_comment_syntax(snapshot: &MultiBufferSnapshot, offset: usize) -> bool {
    snapshot
        .language_scope_at(MultiBufferOffset(offset))
        .is_some_and(|scope| {
            !scope.line_comment_prefixes().is_empty() || scope.block_comment().is_some()
        })
}

/// Every selection of `editor` as a caret: the region counts only while it is active.
fn carets(editor: &mut Editor, cx: &mut App) -> Vec<Caret> {
    let display = editor.display_snapshot(cx);
    let selections = editor.selections.all::<MultiBufferOffset>(&display);
    selections
        .iter()
        .map(|selection| {
            let head = selection.head().0;
            let tail = if region::is_active(editor, selection, cx) {
                selection.tail().0
            } else {
                head
            };
            Caret { head, tail }
        })
        .collect()
}

/// After a line toggle the region's start sits behind the comment marker the toggle put in
/// front of its first line, which would make the next call see a partial region and block
/// comment it instead of uncommenting. Pulls each region's start back to the indent of its line.
fn keep_regions_on_whole_lines(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut gpui::Context<Editor>,
) {
    let display = editor.display_snapshot(cx);
    let mut selections = editor.selections.all::<MultiBufferOffset>(&display);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    for selection in selections.iter_mut().filter(|s| !s.is_empty()) {
        let row = snapshot.offset_to_point(selection.start).row;
        let indent = snapshot.indent_size_for_line(MultiBufferRow(row)).len;
        let content = snapshot.point_to_offset(Point::new(row, indent));
        selection.start = selection.start.min(content);
    }
    editor.change_selections(SelectionEffects::default(), window, cx, |s| {
        s.select(selections)
    });
}

/// comment-dwim: a caret without a region, or a region covering whole lines, toggles line
/// comments on its lines; a region that starts or ends inside a line toggles a block comment
/// around it. A language without block comments falls back to line comments. Both toggle, so
/// a second call uncomments.
pub(crate) fn comment_dwim_command(
    _: &CommentDwim,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    editor.update(cx, |editor, cx| {
        if editor.read_only(cx) {
            return;
        }
        let carets = carets(editor, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let text = SnapshotText::new(&snapshot);
        let partial = carets
            .iter()
            .find(|caret| !covers_whole_lines(&text, caret.region()));
        let block = partial.is_some_and(|caret| has_block_comment(&snapshot, caret.region().start));
        // Text typed just before the command must not merge into the command's undo step.
        editor.finalize_last_transaction(cx);
        if block {
            editor.toggle_block_comments(&ToggleBlockComments, window, cx);
        } else {
            toggle_line_comments(editor, window, cx);
            if partial.is_none() {
                keep_regions_on_whole_lines(editor, window, cx);
            }
        }
        editor.set_selection_mark_mode(false, cx);
        editor.finalize_last_transaction(cx);
    });
}

/// The lines one caret asks to duplicate, and where its caret goes.
struct Block {
    /// The whole lines, without the newline after them.
    lines: Range<usize>,
    head: usize,
}

/// The whole lines every caret touches, sorted, with carets on the same lines merged into one
/// block (the first caret's column wins), and only where the language can comment.
fn blocks(snapshot: &MultiBufferSnapshot, carets: &[Caret]) -> Vec<Block> {
    let text = SnapshotText::new(snapshot);
    let mut blocks: Vec<Block> = carets
        .iter()
        .map(|caret| {
            let lines = whole_line_span(&text, caret);
            Block {
                head: caret.head.clamp(lines.start, lines.end),
                lines,
            }
        })
        .collect();
    blocks.sort_by_key(|block| block.lines.start);
    let mut merged: Vec<Block> = Vec::new();
    for block in blocks {
        match merged.last_mut() {
            Some(last) if block.lines.start <= last.lines.end => {
                last.lines.end = last.lines.end.max(block.lines.end);
            }
            _ => merged.push(block),
        }
    }
    merged.retain(|block| has_comment_syntax(snapshot, block.lines.start));
    merged
}

/// duplicate-and-comment: copies the lines the region touches (the caret's line without a
/// region) below themselves and comments the originals out with the same toggle as comment-dwim
/// (a block comment around them in a language with no line comments), in one undo step. The
/// caret lands on the copy at the same column. A language with no comment syntax changes nothing
/// and says so in the echo area.
pub(crate) fn duplicate_and_comment_command(
    _: &DuplicateAndComment,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let done = editor.update(cx, |editor, cx| {
        if editor.read_only(cx) {
            return true;
        }
        let carets = carets(editor, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let blocks = blocks(&snapshot, &carets);
        if blocks.is_empty() {
            return false;
        }
        let copies: Vec<String> = blocks
            .iter()
            .map(|block| {
                let lines =
                    MultiBufferOffset(block.lines.start)..MultiBufferOffset(block.lines.end);
                format!("\n{}", snapshot.text_for_range(lines).collect::<String>())
            })
            .collect();
        // Text typed just before the command must not merge into the command's undo step.
        editor.finalize_last_transaction(cx);
        editor.transact(window, cx, |editor, window, cx| {
            let inserts = blocks
                .iter()
                .zip(&copies)
                .map(|(block, copy)| {
                    let at = MultiBufferOffset(block.lines.end);
                    (at..at, copy.clone())
                })
                .collect::<Vec<_>>();
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(inserts, None, cx);
            });
            // Where the originals and the carets on the copies are once the copies are in.
            let mut shift = 0;
            let mut originals = Vec::new();
            let mut targets = Vec::new();
            for (block, copy) in blocks.iter().zip(&copies) {
                let start = block.lines.start + shift;
                let end = block.lines.end + shift;
                originals.push(MultiBufferOffset(start)..MultiBufferOffset(end));
                targets.push(MultiBufferOffset(end + 1 + block.head - block.lines.start));
                shift += copy.len();
            }
            // Anchors follow the comment markers the toggle puts in front of the copies.
            let after_copies = editor.buffer().read(cx).snapshot(cx);
            let targets = targets
                .into_iter()
                .map(|offset| {
                    let anchor = after_copies.anchor_after(offset);
                    anchor..anchor
                })
                .collect::<Vec<_>>();
            editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                s.select_ranges(originals)
            });
            toggle_line_comments(editor, window, cx);
            editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                s.select_anchor_ranges(targets)
            });
        });
        editor.set_selection_mark_mode(false, cx);
        editor.finalize_last_transaction(cx);
        true
    });
    if !done {
        MinibufferRouting::message(
            window,
            cx,
            NO_COMMENT_SYNTAX,
            MinibufferMessageKind::Message,
        );
    }
}
