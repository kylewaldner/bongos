//! The kill ring: a bounded list of killed texts, newest first, with the
//! rules for joining consecutive kills, choosing the entry to yank, listing
//! entries for a picker, and saving to and loading from JSON.

use std::collections::{HashSet, VecDeque};
use std::ops::RangeInclusive;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The most entries the ring keeps.
pub const KILL_RING_MAX: usize = 60;

/// Entries larger than this many bytes are not saved.
pub const PERSIST_ENTRY_MAX_BYTES: usize = 1 << 20;

const LABEL_MAX_CHARS: usize = 200;
const PREVIEW_MAX_LINES: usize = 10;
const JSON_VERSION: u32 = 1;

/// Where a piece was copied from. Never saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceOrigin {
    pub path: PathBuf,
    pub lines: RangeInclusive<u32>,
}

/// One caret's share of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Byte length of the piece's text.
    pub len: usize,
    /// The piece is whole lines, so it already ends in a newline.
    pub is_entire_line: bool,
    pub first_line_indent: u32,
    pub origin: Option<PieceOrigin>,
}

impl Piece {
    fn plain(len: usize) -> Piece {
        Piece {
            len,
            is_entire_line: false,
            first_line_indent: 0,
            origin: None,
        }
    }
}

/// A killed text. `text` is the pieces joined with a newline between a piece
/// and the next unless the earlier piece is a whole line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub text: String,
    pub pieces: Vec<Piece>,
    pub secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinSide {
    Append,
    Prepend,
}

/// One row of the picker list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    /// Position in the ring, 0 for the newest.
    pub index: usize,
    pub label: String,
    pub preview: String,
    pub secret: bool,
}

impl Entry {
    /// An entry of one piece that is not a whole line.
    pub fn plain(text: String) -> Entry {
        let len = text.len();
        Entry {
            text,
            pieces: vec![Piece::plain(len)],
            secret: false,
        }
    }

    /// Joins `texts` by the entry convention and sets each piece's length from
    /// its text. When the counts differ the texts become one plain piece.
    pub fn from_pieces(texts: &[String], pieces: Vec<Piece>) -> Entry {
        if texts.len() != pieces.len() || texts.is_empty() {
            return Entry::plain(texts.join("\n"));
        }
        let mut text = String::new();
        let mut out = Vec::with_capacity(pieces.len());
        let last = pieces.len() - 1;
        for (ix, (piece_text, mut piece)) in texts.iter().zip(pieces).enumerate() {
            piece.len = piece_text.len();
            text.push_str(piece_text);
            if ix < last && !piece.is_entire_line {
                text.push('\n');
            }
            out.push(piece);
        }
        Entry {
            text,
            pieces: out,
            secret: false,
        }
    }

    /// The text of each piece, or the whole text when the lengths do not fit.
    pub fn piece_texts(&self) -> Vec<&str> {
        match piece_ranges(&self.text, &self.pieces) {
            Some(ranges) => ranges.into_iter().map(|r| &self.text[r]).collect(),
            None => vec![self.text.as_str()],
        }
    }

    fn repaired(mut self) -> Entry {
        if piece_ranges(&self.text, &self.pieces).is_none() {
            self.pieces = vec![Piece::plain(self.text.len())];
        }
        self
    }

    fn all_whole_lines(&self) -> bool {
        self.pieces.iter().all(|piece| piece.is_entire_line)
    }

    fn first_indent(&self) -> u32 {
        self.pieces
            .first()
            .map_or(0, |piece| piece.first_line_indent)
    }
}

fn piece_ranges(text: &str, pieces: &[Piece]) -> Option<Vec<std::ops::Range<usize>>> {
    if pieces.is_empty() {
        return None;
    }
    let mut ranges = Vec::with_capacity(pieces.len());
    let mut pos = 0usize;
    for (ix, piece) in pieces.iter().enumerate() {
        let end = pos.checked_add(piece.len)?;
        if end > text.len() || !text.is_char_boundary(pos) || !text.is_char_boundary(end) {
            return None;
        }
        ranges.push(pos..end);
        pos = end;
        if ix + 1 < pieces.len() && !piece.is_entire_line {
            if text.as_bytes().get(pos) != Some(&b'\n') {
                return None;
            }
            pos += 1;
        }
    }
    (pos == text.len()).then_some(ranges)
}

fn joined(first: &Entry, second: &Entry) -> Entry {
    let secret = first.secret || second.secret;
    let mut entry = if first.pieces.len() == second.pieces.len() && first.pieces.len() >= 2 {
        let texts: Vec<String> = first
            .piece_texts()
            .into_iter()
            .zip(second.piece_texts())
            .map(|(a, b)| format!("{a}{b}"))
            .collect();
        let pieces = first
            .pieces
            .iter()
            .zip(&second.pieces)
            .map(|(a, b)| Piece {
                len: 0,
                is_entire_line: a.is_entire_line && b.is_entire_line,
                first_line_indent: a.first_line_indent,
                origin: None,
            })
            .collect();
        Entry::from_pieces(&texts, pieces)
    } else {
        Entry {
            text: format!("{}{}", first.text, second.text),
            pieces: vec![Piece {
                len: first.text.len() + second.text.len(),
                is_entire_line: first.all_whole_lines() && second.all_whole_lines(),
                first_line_indent: first.first_indent(),
                origin: None,
            }],
            secret: false,
        }
    };
    entry.secret = secret;
    entry
}

/// Killed entries, newest first, at most [`KILL_RING_MAX`].
#[derive(Debug, Clone, Default)]
pub struct Ring {
    entries: VecDeque<Entry>,
}

impl Ring {
    pub fn push(&mut self, entry: Entry) {
        self.entries.push_front(entry);
        self.entries.truncate(KILL_RING_MAX);
    }

    /// Joins `entry` onto the newest entry, or pushes it when the ring is empty.
    pub fn join_newest(&mut self, entry: Entry, side: JoinSide) {
        let Some(newest) = self.entries.front_mut() else {
            self.push(entry);
            return;
        };
        *newest = match side {
            JoinSide::Append => joined(newest, &entry),
            JoinSide::Prepend => joined(&entry, newest),
        };
    }

    pub fn newest(&self) -> Option<&Entry> {
        self.entries.front()
    }

    pub fn get(&self, ix: usize) -> Option<&Entry> {
        self.entries.get(ix)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry a yank takes: the newest with no count; with N, the Nth
    /// newest, where 0 and negative counts wrap to the oldest end.
    pub fn yank_index(&self, count: Option<i32>) -> Option<usize> {
        if self.is_empty() {
            return None;
        }
        match count {
            None => Some(0),
            Some(n) => Some((i64::from(n) - 1).rem_euclid(self.len() as i64) as usize),
        }
    }

    /// The entry a yank-pop moves to from `from`: one older with no count,
    /// else `n` steps older (negative toward newer), wrapping.
    pub fn pop_index(&self, from: usize, count: Option<i32>) -> Option<usize> {
        if self.is_empty() {
            return None;
        }
        let step = i64::from(count.unwrap_or(1));
        Some((from as i64 + step).rem_euclid(self.len() as i64) as usize)
    }

    /// Rows newest first. A text that also appears at a newer position is
    /// hidden. `query` filters by case-insensitive substring of the text; a
    /// secret entry only shows for an empty query.
    pub fn picker_view(&self, query: &str) -> Vec<PickerRow> {
        let query = query.to_lowercase();
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if !seen.insert(entry.text.as_str()) {
                continue;
            }
            if !query.is_empty() && (entry.secret || !entry.text.to_lowercase().contains(&query)) {
                continue;
            }
            let (label, preview) = if entry.secret {
                let concealed = format!(
                    "●●●●●●●● (concealed, {} characters)",
                    entry.text.chars().count()
                );
                (concealed.clone(), concealed)
            } else {
                (label(&entry.text), preview(&entry.text))
            };
            rows.push(PickerRow {
                index,
                label,
                preview,
                secret: entry.secret,
            });
        }
        rows
    }

    /// The saved form: secret and oversized entries are left out, and so is
    /// every piece origin.
    pub fn to_json(&self) -> String {
        let file = FileRef {
            version: JSON_VERSION,
            entries: self
                .entries
                .iter()
                .filter(|entry| !entry.secret && entry.text.len() <= PERSIST_ENTRY_MAX_BYTES)
                .map(|entry| EntryRef {
                    text: &entry.text,
                    pieces: entry
                        .pieces
                        .iter()
                        .map(|piece| PieceJson {
                            len: piece.len,
                            is_entire_line: piece.is_entire_line,
                            first_line_indent: piece.first_line_indent,
                        })
                        .collect(),
                })
                .collect(),
        };
        serde_json::to_string(&file).unwrap_or_default()
    }

    /// Loads saved entries. Malformed or unknown-version input is an empty
    /// ring; an entry whose lengths do not fit its text becomes one plain piece.
    pub fn from_json(json: &str) -> Ring {
        let Ok(file) = serde_json::from_str::<FileOwned>(json) else {
            return Ring::default();
        };
        if file.version != JSON_VERSION {
            return Ring::default();
        }
        let entries = file
            .entries
            .into_iter()
            .take(KILL_RING_MAX)
            .map(|entry| {
                Entry {
                    pieces: entry
                        .pieces
                        .into_iter()
                        .map(|piece| Piece {
                            len: piece.len,
                            is_entire_line: piece.is_entire_line,
                            first_line_indent: piece.first_line_indent,
                            origin: None,
                        })
                        .collect(),
                    text: entry.text,
                    secret: false,
                }
                .repaired()
            })
            .collect();
        Ring { entries }
    }
}

fn label(text: &str) -> String {
    let one_line = text.replace('\n', "⏎");
    if one_line.chars().count() <= LABEL_MAX_CHARS {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(LABEL_MAX_CHARS).collect();
    cut.push('…');
    cut
}

fn preview(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= PREVIEW_MAX_LINES {
        return lines.join("\n");
    }
    let mut out = lines[..PREVIEW_MAX_LINES].join("\n");
    out.push_str(&format!(
        "\n… {} more lines",
        lines.len() - PREVIEW_MAX_LINES
    ));
    out
}

#[derive(Serialize, Deserialize)]
struct PieceJson {
    len: usize,
    is_entire_line: bool,
    first_line_indent: u32,
}

#[derive(Serialize)]
struct EntryRef<'a> {
    text: &'a str,
    pieces: Vec<PieceJson>,
}

#[derive(Serialize)]
struct FileRef<'a> {
    version: u32,
    entries: Vec<EntryRef<'a>>,
}

#[derive(Deserialize)]
struct EntryOwned {
    text: String,
    #[serde(default)]
    pieces: Vec<PieceJson>,
}

#[derive(Deserialize)]
struct FileOwned {
    version: u32,
    #[serde(default)]
    entries: Vec<EntryOwned>,
}
