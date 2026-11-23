use std::ops::Range;

use crate::text::TextSource;

/// Which way to look for the next occurrence of a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// Where a jump to another occurrence of a symbol lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolJump {
    /// The occurrence jumped to.
    pub range: Range<usize>,
    /// The caret offset inside that occurrence.
    pub caret: usize,
    /// The search passed the start or end of the text and came around.
    pub wrapped: bool,
}

/// A symbol character is a letter, a digit or `_`. The rule is fixed and does
/// not depend on the text source.
fn is_symbol_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn read_all<T: TextSource + ?Sized>(text: &T) -> String {
    text.chars_at(0).collect()
}

/// Every maximal run of symbol characters, ascending.
fn runs(text: &str) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = None;
    for (offset, c) in text.char_indices() {
        match (is_symbol_char(c), start) {
            (true, None) => start = Some(offset),
            (false, Some(s)) => {
                runs.push(s..offset);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push(s..text.len());
    }
    runs
}

/// The symbol containing `offset` (inside it or at its start), else the symbol
/// ending at `offset`. `None` when neither side of the offset is a symbol
/// character.
pub fn symbol_at<T: TextSource + ?Sized>(text: &T, offset: usize) -> Option<Range<usize>> {
    let offset = offset.min(text.len());
    let mut end = offset;
    for c in text.chars_at(offset) {
        if !is_symbol_char(c) {
            break;
        }
        end += c.len_utf8();
    }
    let mut start = offset;
    for c in text.reversed_chars_at(offset) {
        if !is_symbol_char(c) {
            break;
        }
        start -= c.len_utf8();
    }
    (start < end || start < offset).then_some(start..end)
}

/// The caret offset inside `target` that sits `relative` bytes into the word,
/// kept within the occurrence's length.
pub(crate) fn caret_within(target: &Range<usize>, relative: usize) -> usize {
    target.start + relative.min(target.end - target.start)
}

/// The next whole-word occurrence of the symbol at `offset`, strictly after the
/// current one (forward) or strictly before it (backward). Reaching either end
/// of the text wraps around and sets `wrapped`; a symbol that occurs only once
/// jumps onto itself. `None` when there is no symbol at `offset`.
pub fn next_occurrence<T: TextSource + ?Sized>(
    text: &T,
    offset: usize,
    direction: Direction,
) -> Option<SymbolJump> {
    let current = symbol_at(text, offset)?;
    let all = read_all(text);
    let word = &all[current.clone()];
    let found: Vec<Range<usize>> = runs(&all)
        .into_iter()
        .filter(|run| &all[run.clone()] == word)
        .collect();
    let relative = offset.min(current.end) - current.start;
    let (range, wrapped) = match direction {
        Direction::Forward => match found.iter().find(|run| run.start >= current.end) {
            Some(run) => (run.clone(), false),
            None => (found.first()?.clone(), true),
        },
        Direction::Backward => match found.iter().rev().find(|run| run.end <= current.start) {
            Some(run) => (run.clone(), false),
            None => (found.last()?.clone(), true),
        },
    };
    let caret = caret_within(&range, relative);
    Some(SymbolJump {
        range,
        caret,
        wrapped,
    })
}

/// Whole-word occurrences of `word` lying inside `within` (clamped to the
/// text), ascending, at most `limit`.
pub fn occurrences<T: TextSource + ?Sized>(
    text: &T,
    word: &str,
    within: Range<usize>,
    limit: usize,
) -> Vec<Range<usize>> {
    if word.is_empty() || limit == 0 {
        return Vec::new();
    }
    let all = read_all(text);
    let end = within.end.min(all.len());
    let start = within.start.min(end);
    runs(&all)
        .into_iter()
        .filter(|run| run.start >= start && run.end <= end && &all[run.clone()] == word)
        .take(limit)
        .collect()
}
