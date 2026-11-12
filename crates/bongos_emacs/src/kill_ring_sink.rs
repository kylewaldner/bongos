//! The kill ring as the kill sink: every kill and copy lands in the editor's kill ring before
//! the buffer changes, joined onto the newest entry while the kill sequence continues, and the
//! newest entry is written to the system clipboard.

use bongos_emacs_model::kill_ring::{Entry, JoinSide, Piece, PieceOrigin};
use editor::{Editor, EditorEvent, kill_ring};
use gpui::{App, Context, EntityId, Global, Window, WindowId};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use std::sync::Arc;
use text::Selection;

use crate::kill_sink::{Kill, KillDirection, KillKind, set_kill_sink};
use crate::last_command::last_command;

/// Every command that can send a kill or a copy to the sink. A kill or copy joins the newest
/// entry only when the window's previous command is in this set and was a kill, as in Emacs: a
/// copy after a kill appends to it, while a kill (or copy) after a copy starts a new entry.
pub const KILL_COMMANDS: &[&str] = &[
    "emacs::KillLine",
    "emacs::KillRegionOrLine",
    "emacs::CopyRegionOrLine",
    "emacs::KillWord",
    "emacs::BackwardKillWord",
    "emacs::ZapToChar",
    "emacs::ZapUpToChar",
    "emacs::ZapBackToChar",
    "emacs::ZapBackUpToChar",
];

/// Where the last kill happened. A mouse selection in that editor, or carets that moved since
/// the kill, forget it, so the next kill starts a new entry even though a click (or a jump made
/// without a key) records no command.
#[derive(Default)]
struct LastKill(Option<Last>);

struct Last {
    window: WindowId,
    editor: EntityId,
    kind: KillKind,
    /// The editor's carets once the kill was applied; `None` until then.
    carets: Option<Arc<[Selection<Anchor>]>>,
}

impl Global for LastKill {}

/// Makes the kill ring the kill sink.
pub(crate) fn init(cx: &mut App) {
    set_kill_sink(cx, kill_to_ring);
}

/// Watches `editor` for mouse selections, which end a kill sequence there.
pub(crate) fn init_editor(cx: &mut Context<Editor>) {
    cx.subscribe_self(|editor, event: &EditorEvent, cx| {
        let EditorEvent::SelectionsChanged { local: true } = event else {
            return;
        };
        if editor.selections.pending_anchor().is_none() {
            return;
        }
        forget_kill_in(cx.entity_id(), cx);
    })
    .detach();
}

/// Called by `apply::run` before it sends a kill from `editor`: forgets the last kill there when
/// the carets are not where that kill left them.
pub(crate) fn forget_kill_if_moved(
    editor: EntityId,
    carets: &[Selection<MultiBufferOffset>],
    snapshot: &MultiBufferSnapshot,
    cx: &mut App,
) {
    let Some(Last {
        editor: last_editor,
        carets: Some(after_kill),
        ..
    }) = cx.try_global::<LastKill>().and_then(|last| last.0.as_ref())
    else {
        return;
    };
    if *last_editor != editor {
        return;
    }
    let moved = after_kill.len() != carets.len()
        || after_kill.iter().zip(carets).any(|(after, now)| {
            after.head().to_offset(snapshot) != now.head()
                || after.tail().to_offset(snapshot) != now.tail()
        });
    if moved {
        forget_kill_in(editor, cx);
    }
}

/// Called by `apply::run` once a kill from `editor` is applied: remembers where its carets are.
pub(crate) fn remember_carets(editor: EntityId, carets: Arc<[Selection<Anchor>]>, cx: &mut App) {
    if let Some(last) = cx.default_global::<LastKill>().0.as_mut()
        && last.editor == editor
    {
        last.carets = Some(carets);
    }
}

fn forget_kill_in(editor: EntityId, cx: &mut App) {
    if cx
        .try_global::<LastKill>()
        .and_then(|last| last.0.as_ref())
        .is_some_and(|last| last.editor == editor)
    {
        cx.set_global(LastKill(None));
    }
}

/// The sink: captures an outside clipboard text first, then pushes the kill or joins it onto
/// the newest entry, and writes the newest entry to the clipboard.
fn kill_to_ring(kill: Kill, window: &mut Window, cx: &mut App) {
    if kill.pieces.iter().all(String::is_empty) {
        return;
    }
    let window_id = window.window_handle().window_id();
    let continues = last_command(window, cx)
        .is_some_and(|command| KILL_COMMANDS.contains(&command))
        && cx
            .try_global::<LastKill>()
            .and_then(|last| last.0.as_ref())
            .is_some_and(|last| {
                last.window == window_id
                    && last.editor == kill.editor
                    && last.kind == KillKind::Kill
            });
    // Text copied outside since the last kill becomes its own entry, and breaks the sequence.
    let captured = kill_ring::capture_clipboard(cx);
    let side = match kill.direction {
        KillDirection::Backward => JoinSide::Prepend,
        KillDirection::Forward => JoinSide::Append,
    };
    let entry = entry_for(&kill);
    kill_ring::update(cx, |ring| {
        if continues && !captured {
            ring.join_newest(entry, side);
        } else {
            ring.push(entry);
        }
    });
    cx.set_global(LastKill(Some(Last {
        window: window_id,
        editor: kill.editor,
        kind: kill.kind,
        carets: None,
    })));
    kill_ring::write_newest_to_clipboard(cx);
}

/// The ring entry for a kill: one piece per caret with its clipboard metadata.
fn entry_for(kill: &Kill) -> Entry {
    let pieces = kill
        .metadata
        .iter()
        .map(|selection| Piece {
            len: selection.len,
            is_entire_line: selection.is_entire_line,
            first_line_indent: selection.first_line_indent,
            origin: selection
                .file_path
                .clone()
                .zip(selection.line_range.clone())
                .map(|(path, lines)| PieceOrigin { path, lines }),
        })
        .collect();
    Entry::from_pieces(&kill.pieces, pieces)
}
