//! The one kill ring of the app, shared by the editor's cut and copy
//! commands, the Emacs kill and yank commands and every other writer.
//!
//! The ring's rules live in `bongos_emacs_model::kill_ring`; this module
//! holds its state as a gpui global and converts between ring entries and
//! clipboard items.

use bongos_emacs_model::kill_ring::{Entry, Piece, PieceOrigin, Ring};
use gpui::{App, ClipboardEntry, ClipboardItem, ClipboardString, Global};

use crate::ClipboardSelection;

/// The kill ring global. It is created empty on first use.
#[derive(Default)]
pub struct KillRing {
    ring: Ring,
    generation: u64,
    /// Hash of the clipboard text that must never be captured into the ring.
    ignored_clipboard: Option<u64>,
}

impl Global for KillRing {}

/// The ring, or `None` when nothing has created it yet.
pub fn read(cx: &App) -> Option<&Ring> {
    cx.try_global::<KillRing>().map(|kill_ring| &kill_ring.ring)
}

/// Changes the ring and bumps its generation.
pub fn update<R>(cx: &mut App, f: impl FnOnce(&mut Ring) -> R) -> R {
    let kill_ring = cx.default_global::<KillRing>();
    kill_ring.generation = kill_ring.generation.wrapping_add(1);
    f(&mut kill_ring.ring)
}

/// A counter bumped by every [`update`], so observers can skip
/// notifications that did not change the ring.
pub fn generation(cx: &App) -> u64 {
    cx.try_global::<KillRing>()
        .map_or(0, |kill_ring| kill_ring.generation)
}

/// The entry for a clipboard item's text, with one piece per selection when
/// the item carries consistent editor metadata, else one plain piece.
/// `None` when the item has no text.
pub fn entry_from_clipboard(item: &ClipboardItem) -> Option<Entry> {
    let string = item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::String(string) => Some(string),
        _ => None,
    })?;
    let text = string.text();
    let Some(selections) = string.metadata_json::<Vec<ClipboardSelection>>() else {
        return Some(Entry::plain(text.clone()));
    };
    let pieces = selections
        .iter()
        .map(|selection| (selection.len, selection.is_entire_line))
        .collect::<Vec<_>>();
    let Some(texts) = split_pieces(text, &pieces) else {
        return Some(Entry::plain(text.clone()));
    };
    let texts = texts.into_iter().map(str::to_string).collect::<Vec<_>>();
    let pieces = selections
        .into_iter()
        .map(|selection| Piece {
            len: selection.len,
            is_entire_line: selection.is_entire_line,
            first_line_indent: selection.first_line_indent,
            origin: selection
                .file_path
                .zip(selection.line_range)
                .map(|(path, lines)| PieceOrigin { path, lines }),
        })
        .collect();
    Some(Entry::from_pieces(&texts, pieces))
}

/// The clipboard item for an entry, in the format `editor::Copy` writes, so
/// `editor::Paste` and other readers of that format get its pieces back.
pub fn clipboard_item(entry: &Entry) -> ClipboardItem {
    ClipboardItem::new_string_with_json_metadata(entry.text.clone(), clipboard_selections(entry))
}

/// The per-piece selections of an entry, as `Editor::do_paste` reads them.
/// An entry whose pieces do not fit its text gives one plain selection.
pub(crate) fn clipboard_selections(entry: &Entry) -> Vec<ClipboardSelection> {
    let pieces = entry
        .pieces
        .iter()
        .map(|piece| (piece.len, piece.is_entire_line))
        .collect::<Vec<_>>();
    if split_pieces(&entry.text, &pieces).is_none() {
        return vec![ClipboardSelection {
            len: entry.text.len(),
            is_entire_line: false,
            first_line_indent: 0,
            file_path: None,
            line_range: None,
        }];
    }
    entry
        .pieces
        .iter()
        .map(|piece| ClipboardSelection {
            len: piece.len,
            is_entire_line: piece.is_entire_line,
            first_line_indent: piece.first_line_indent,
            file_path: piece.origin.as_ref().map(|origin| origin.path.clone()),
            line_range: piece.origin.as_ref().map(|origin| origin.lines.clone()),
        })
        .collect()
}

/// Writes the newest entry to the system clipboard, if there is one.
pub fn write_newest_to_clipboard(cx: &mut App) {
    let Some(item) = read(cx).and_then(Ring::newest).map(clipboard_item) else {
        return;
    };
    cx.write_to_clipboard(item);
}

/// Pushes the system clipboard's text as a new entry when it is not empty,
/// differs from the newest entry's text and is not the ignored text.
/// Returns whether it pushed.
pub fn capture_clipboard(cx: &mut App) -> bool {
    let Some(item) = cx.read_from_clipboard() else {
        return false;
    };
    let Some(entry) = entry_from_clipboard(&item) else {
        return false;
    };
    if entry.text.is_empty() {
        return false;
    }
    if read(cx)
        .and_then(Ring::newest)
        .is_some_and(|newest| newest.text == entry.text)
    {
        return false;
    }
    let ignored = cx
        .try_global::<KillRing>()
        .and_then(|kill_ring| kill_ring.ignored_clipboard);
    if ignored == Some(ClipboardString::text_hash(&entry.text)) {
        return false;
    }
    update(cx, |ring| ring.push(entry));
    true
}

/// Remembers `text` so that [`capture_clipboard`] never pushes it. Only the
/// latest ignored text is kept.
pub fn ignore_clipboard_text(text: &str, cx: &mut App) {
    cx.default_global::<KillRing>().ignored_clipboard = Some(ClipboardString::text_hash(text));
}

/// The newest entry's text.
pub fn yank_text(cx: &App) -> Option<String> {
    read(cx)?.newest().map(|entry| entry.text.clone())
}

/// Splits `text` into its pieces by `do_paste`'s rule: each piece is `len`
/// bytes and is followed by a newline unless it is a whole line or the last.
/// `None` when the pieces do not fit the text exactly on char boundaries.
fn split_pieces<'a>(text: &'a str, pieces: &[(usize, bool)]) -> Option<Vec<&'a str>> {
    if pieces.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(pieces.len());
    let mut start: usize = 0;
    let last = pieces.len() - 1;
    for (ix, &(len, is_entire_line)) in pieces.iter().enumerate() {
        let end = start.checked_add(len)?;
        out.push(text.get(start..end)?);
        start = end;
        if ix < last && !is_entire_line {
            if text.as_bytes().get(start) != Some(&b'\n') {
                return None;
            }
            start += 1;
        }
    }
    (start == text.len()).then_some(out)
}
