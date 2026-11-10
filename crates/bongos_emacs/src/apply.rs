//! The one place a command's plan reaches the editor: every caret at once, killed text saved
//! before anything is deleted, one undo step per command, read-only buffers left alone.

use bongos_emacs_model::{Caret, Plan, TextSource as _};
use editor::{ClipboardSelection, Editor, SelectionEffects};
use gpui::{App, Entity, Window};
use multi_buffer::{MultiBufferOffset, MultiBufferSnapshot};

use crate::kill_sink::{self, Kill, KillDirection, KillKind};
use crate::region;
use crate::snapshot_text::SnapshotText;

/// What a command does with the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// Moves or selects; edits nothing, so it runs in read-only buffers too.
    Motion,
    /// Edits the text (and kills what it deletes); nothing runs in a read-only buffer.
    Edit,
    /// Copies text to the kill sink without editing; runs in read-only buffers too.
    Copy,
}

/// A command as `run` sees it.
#[derive(Clone, Copy, Debug)]
pub struct Command {
    /// The action name, such as `emacs::KillWord`; the kill sink receives it.
    pub name: &'static str,
    pub kind: CommandKind,
    /// The killed pieces that end in a newline and did not come from a region are whole lines
    /// (kill-region-or-line and copy-region-or-line without a region; a mixed set of carets
    /// labels each piece on its own), so `editor::Paste` puts them back as lines.
    pub whole_lines: bool,
}

impl Command {
    pub fn motion(name: &'static str) -> Self {
        Self {
            name,
            kind: CommandKind::Motion,
            whole_lines: false,
        }
    }

    pub fn edit(name: &'static str) -> Self {
        Self {
            name,
            kind: CommandKind::Edit,
            whole_lines: false,
        }
    }

    pub fn copy(name: &'static str) -> Self {
        Self {
            name,
            kind: CommandKind::Copy,
            whole_lines: false,
        }
    }

    pub fn with_whole_lines(self) -> Self {
        Self {
            whole_lines: true,
            ..self
        }
    }
}

/// Runs a command on every caret of `editor`:
/// 1. reads the snapshot and every selection and computes the plan once for all carets (a
///    selection's region counts only when `region::is_active`);
/// 2. does nothing when the plan is empty (a zap that found nothing keeps the region), or when
///    the command edits and the editor is read-only (nothing is killed either);
/// 3. hands the killed text to the kill sink before touching the buffer;
/// 4. finalizes the open undo group (so earlier typing stays its own step), applies all edits and
///    the new carets in one transaction, ends mark mode, and finalizes the transaction so the
///    next command is a separate undo step.
///
/// Returns whether the plan was applied.
pub fn run(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
    command: Command,
    compute: impl FnOnce(&SnapshotText, &[Caret]) -> Plan,
) -> bool {
    let prepared = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selections = editor.selections.all::<MultiBufferOffset>(&display);
        let carets: Vec<Caret> = selections
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
            .collect();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let plan = compute(&SnapshotText::new(&snapshot), &carets);
        if plan.edits.is_empty() && plan.carets.is_empty() && plan.killed.is_empty() {
            return None;
        }
        let edits = command.kind == CommandKind::Edit || !plan.edits.is_empty();
        if edits && editor.read_only(cx) {
            return None;
        }
        let kill = (!plan.killed.is_empty()).then(|| {
            let metadata = kill_metadata(&plan, &carets, &snapshot, command)
                .into_iter()
                .map(|(len, is_entire_line, range)| {
                    ClipboardSelection::for_buffer(
                        len,
                        is_entire_line,
                        range,
                        &snapshot,
                        editor.project(),
                        cx,
                    )
                })
                .collect();
            Kill {
                pieces: plan.killed.iter().map(|piece| piece.text.clone()).collect(),
                metadata,
                kind: if edits {
                    KillKind::Kill
                } else {
                    KillKind::Copy
                },
                direction: if plan.killed.iter().all(|piece| piece.backward) {
                    KillDirection::Backward
                } else {
                    KillDirection::Forward
                },
                command: command.name,
            }
        });
        Some((plan, kill, edits))
    });
    let Some((plan, kill, edits)) = prepared else {
        return false;
    };

    // The killed text is saved before the buffer changes, so nothing deleted is lost.
    if let Some(kill) = kill {
        kill_sink::send(kill, window, cx);
    }

    editor.update(cx, |editor, cx| {
        let carets = plan
            .carets
            .iter()
            .map(|caret| MultiBufferOffset(caret.tail)..MultiBufferOffset(caret.head))
            .collect::<Vec<_>>();
        if plan.edits.is_empty() {
            if !carets.is_empty() {
                editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                    s.select_ranges(carets)
                });
            }
        } else {
            let buffer_edits = plan
                .edits
                .iter()
                .map(|(range, text)| {
                    (
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        text.clone(),
                    )
                })
                .collect::<Vec<_>>();
            // Text typed just before the command must not merge into the command's undo step.
            editor.finalize_last_transaction(cx);
            editor.transact(window, cx, |editor, window, cx| {
                editor.buffer().update(cx, |buffer, cx| {
                    buffer.edit(buffer_edits, None, cx);
                });
                if !carets.is_empty() {
                    editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                        s.select_ranges(carets)
                    });
                }
            });
            editor.finalize_last_transaction(cx);
        }
        if edits {
            editor.set_selection_mark_mode(false, cx);
        }
    });
    true
}

/// For each killed piece: its length, whether it is a whole line, and the buffer range it came
/// from (for the first line's indent and the file location `editor::Paste` reads).
fn kill_metadata(
    plan: &Plan,
    carets: &[Caret],
    snapshot: &MultiBufferSnapshot,
    command: Command,
) -> Vec<(usize, bool, std::ops::Range<text::Point>)> {
    let text = SnapshotText::new(snapshot);
    let deletes_each_piece =
        plan.edits.len() == plan.killed.len() && plan.edits.iter().all(|(_, new)| new.is_empty());
    let mut sorted_carets = carets.to_vec();
    sorted_carets.sort_by_key(|caret| caret.region().start);
    plan.killed
        .iter()
        .enumerate()
        .map(|(ix, piece)| {
            // A piece taken from a region is never a whole line, even when it ends in a
            // newline; only carets without a region take whole lines.
            let from_region;
            let start = if deletes_each_piece {
                from_region = sorted_carets
                    .iter()
                    .any(|caret| caret.has_region() && caret.region() == plan.edits[ix].0);
                let range = &plan.edits[ix].0;
                // A last line killed with the newline before it: the piece is the line and a
                // newline, so it starts one byte into the deleted range.
                if !piece.text.starts_with('\n') && text.chars_at(range.start).next() == Some('\n')
                {
                    range.start + 1
                } else {
                    range.start
                }
            } else {
                let caret = sorted_carets.get(ix).or(sorted_carets.last());
                from_region = caret.is_some_and(|caret| caret.has_region());
                caret.map_or(0, |caret| {
                    if command.whole_lines && !caret.has_region() {
                        // A whole-line copy starts at its line start.
                        let column = text
                            .reversed_chars_at(caret.head)
                            .take_while(|&c| c != '\n')
                            .map(char::len_utf8)
                            .sum::<usize>();
                        caret.head - column
                    } else {
                        caret.region().start
                    }
                })
            };
            let start = start.min(text.len());
            let end = (start + piece.text.len()).min(text.len());
            let is_entire_line = command.whole_lines && !from_region && piece.text.ends_with('\n');
            let range = snapshot.offset_to_point(MultiBufferOffset(start))
                ..snapshot.offset_to_point(MultiBufferOffset(end));
            (piece.text.len(), is_entire_line, range)
        })
        .collect()
}
