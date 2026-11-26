//! Where conceal planning and reveal read the text from: the whole text as a `str`, or
//! [`TextSlices`], a few held pieces of a larger text (the window and the selections' lines), so a
//! refresh costs the size of what is on screen rather than the size of the buffer.

use std::ops::Range;

/// Read access to a text by byte offsets, which may hold only some of it.
pub trait TextSource {
    /// The length in bytes of the whole text, held or not.
    fn total_len(&self) -> usize;

    /// The text of `range`, when the whole range is held and both ends are char boundaries.
    fn get(&self, range: Range<usize>) -> Option<&str>;

    /// The line holding `offset`, between newlines and without them. Where the line runs past
    /// the held text, it ends where the held text ends; an offset that is not held gives an empty
    /// range at it.
    fn line_around(&self, offset: usize) -> Range<usize>;

    /// Whether `offset` is held and is a char boundary.
    fn is_char_boundary(&self, offset: usize) -> bool {
        self.get(offset..offset).is_some()
    }
}

impl TextSource for str {
    fn total_len(&self) -> usize {
        self.len()
    }

    fn get(&self, range: Range<usize>) -> Option<&str> {
        str::get(self, range)
    }

    fn line_around(&self, offset: usize) -> Range<usize> {
        line_in(self, 0, offset.min(self.len()))
    }
}

/// The line holding `offset` inside `text`, which starts at `base` in the whole text.
fn line_in(text: &str, base: usize, offset: usize) -> Range<usize> {
    let bytes = text.as_bytes();
    let at = offset - base;
    let start = bytes[..at]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let end = bytes[at..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(text.len(), |i| at + i);
    base + start..base + end
}

/// Pieces of a text of `total_len` bytes, sorted and apart, each with its start offset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextSlices {
    total_len: usize,
    pieces: Vec<(usize, String)>,
}

impl TextSlices {
    /// Holds `ranges` of a text of `total_len` bytes, read through `read`. Overlapping and
    /// touching ranges are read once as one piece; ranges are clipped to the text.
    pub fn new(
        total_len: usize,
        ranges: impl IntoIterator<Item = Range<usize>>,
        mut read: impl FnMut(Range<usize>) -> String,
    ) -> Self {
        let mut ranges: Vec<Range<usize>> = ranges
            .into_iter()
            .map(|range| range.start.min(total_len)..range.end.min(total_len))
            .filter(|range| range.start <= range.end)
            .collect();
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        let pieces = merged
            .into_iter()
            .map(|range| (range.start, read(range)))
            .collect();
        Self { total_len, pieces }
    }

    /// The held piece that contains `range`, ends included.
    fn piece(&self, range: &Range<usize>) -> Option<&(usize, String)> {
        let i = self
            .pieces
            .partition_point(|(start, _)| *start <= range.start);
        let piece = self.pieces.get(i.checked_sub(1)?)?;
        (range.end <= piece.0 + piece.1.len()).then_some(piece)
    }
}

impl TextSource for TextSlices {
    fn total_len(&self) -> usize {
        self.total_len
    }

    fn get(&self, range: Range<usize>) -> Option<&str> {
        if range.start > range.end {
            return None;
        }
        let (start, text) = self.piece(&range)?;
        text.get(range.start - start..range.end - start)
    }

    fn line_around(&self, offset: usize) -> Range<usize> {
        match self.piece(&(offset..offset)) {
            Some((start, text)) => line_in(text, *start, offset),
            None => offset..offset,
        }
    }
}
