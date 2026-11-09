use std::ops::Range;

use crate::marker::{MarkerLine, MarkerSyntax};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexedMarker {
    pub id: u64,
    pub row: u32,
    pub marker: MarkerLine,
}

/// Counters tests read: navigation parses zero lines and a one-line edit parses one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub lines_parsed: u64,
    pub rebuilds: u64,
}

/// What an `edit` changed. A marker that came back identical on the same row keeps its id and is
/// in neither list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexChange {
    pub removed: Vec<u64>,
    pub added: Vec<u64>,
    pub changed: bool,
}

/// One block as the index sees it. `index` counts from 0 over all blocks, the marker-less one
/// before the first marker included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub index: usize,
    pub id: Option<u64>,
    pub marker_row: Option<u32>,
    pub marker: Option<MarkerLine>,
    pub body: Range<u32>,
}

impl BlockInfo {
    pub fn is_pending(&self) -> bool {
        !self.marker.is_some_and(|marker| marker.is_sent())
    }
}

/// The marker rows of a file, sorted by row, kept up to date edit by edit instead of reparsing.
#[derive(Clone, Debug)]
pub struct PromptIndex {
    syntax: MarkerSyntax,
    markers: Vec<IndexedMarker>,
    line_count: u32,
    next_id: u64,
    stats: IndexStats,
}

impl PromptIndex {
    pub fn build(syntax: MarkerSyntax, lines: &[&str]) -> Self {
        let mut index = Self {
            syntax,
            markers: Vec::new(),
            line_count: 0,
            next_id: 0,
            stats: IndexStats::default(),
        };
        index.fill(lines);
        index
    }

    /// Parses every line again, keeping the counters and ids moving forward.
    pub fn rebuild(&mut self, lines: &[&str]) {
        self.markers.clear();
        self.stats.rebuilds += 1;
        self.fill(lines);
    }

    fn fill(&mut self, lines: &[&str]) {
        self.line_count = lines.len() as u32;
        self.stats.lines_parsed += lines.len() as u64;
        for (row, line) in lines.iter().enumerate() {
            if let Some(marker) = self.syntax.parse(line) {
                let id = self.take_id();
                self.markers.push(IndexedMarker {
                    id,
                    row: row as u32,
                    marker,
                });
            }
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn syntax(&self) -> &MarkerSyntax {
        &self.syntax
    }

    pub fn markers(&self) -> &[IndexedMarker] {
        &self.markers
    }

    pub fn line_count(&self) -> u32 {
        self.line_count
    }

    pub fn stats(&self) -> IndexStats {
        self.stats
    }

    /// Replaces `old_rows` (rows of the text before the edit, clamped to it) with `new_lines`.
    /// Markers in `old_rows` are dropped, the new lines are parsed, and later rows shift by the
    /// change in line count. Apply several edits last to first, each in old coordinates.
    pub fn edit(&mut self, old_rows: Range<u32>, new_lines: &[&str]) -> IndexChange {
        let start = old_rows.start.min(self.line_count);
        let end = old_rows.end.clamp(start, self.line_count);
        let first = self.markers.partition_point(|m| m.row < start);
        let last = self.markers.partition_point(|m| m.row < end);
        self.stats.lines_parsed += new_lines.len() as u64;

        let mut old: Vec<Option<IndexedMarker>> = self.markers[first..last]
            .iter()
            .copied()
            .map(Some)
            .collect();
        let mut inserted: Vec<IndexedMarker> = Vec::new();
        let mut added = Vec::new();
        for (offset, line) in new_lines.iter().enumerate() {
            let Some(marker) = self.syntax.parse(line) else {
                continue;
            };
            let row = start + offset as u32;
            let same = old
                .iter_mut()
                .find(|slot| slot.is_some_and(|m| m.row == row && m.marker == marker));
            if let Some(slot) = same {
                inserted.push(slot.take().expect("matched a present marker"));
                continue;
            }
            let id = self.take_id();
            added.push(id);
            inserted.push(IndexedMarker { id, row, marker });
        }
        let removed: Vec<u64> = old.into_iter().flatten().map(|m| m.id).collect();

        let delta = new_lines.len() as i64 - (end - start) as i64;
        for marker in &mut self.markers[last..] {
            marker.row = (marker.row as i64 + delta) as u32;
        }
        self.markers.splice(first..last, inserted);
        self.line_count = (self.line_count as i64 + delta) as u32;
        let changed = !removed.is_empty() || !added.is_empty();
        IndexChange {
            removed,
            added,
            changed,
        }
    }

    fn has_preamble(&self) -> bool {
        self.markers.first().is_none_or(|m| m.row != 0)
    }

    pub fn block_count(&self) -> usize {
        self.markers.len() + usize::from(self.has_preamble())
    }

    pub fn block(&self, index: usize) -> Option<BlockInfo> {
        if index >= self.block_count() {
            return None;
        }
        let preamble = usize::from(self.has_preamble());
        if index < preamble {
            let end = self.markers.first().map_or(self.line_count, |m| m.row);
            return Some(BlockInfo {
                index,
                id: None,
                marker_row: None,
                marker: None,
                body: 0..end,
            });
        }
        let marker = self.markers[index - preamble];
        let end = self
            .markers
            .get(index - preamble + 1)
            .map_or(self.line_count, |next| next.row);
        Some(BlockInfo {
            index,
            id: Some(marker.id),
            marker_row: Some(marker.row),
            marker: Some(marker.marker),
            body: (marker.row + 1).min(end)..end,
        })
    }

    /// The index of the block holding `row` (rows past the end belong to the last block).
    pub fn block_index_at(&self, row: u32) -> usize {
        let at_or_before = self.markers.partition_point(|m| m.row <= row);
        if self.has_preamble() {
            at_or_before
        } else {
            at_or_before - 1
        }
    }

    pub fn block_at(&self, row: u32) -> BlockInfo {
        self.block(self.block_index_at(row))
            .expect("an index always has a block")
    }

    /// `(n, total)` with `n` counted from 1, for "prompt 7/12".
    pub fn position(&self, row: u32) -> (usize, usize) {
        (self.block_index_at(row) + 1, self.block_count())
    }

    pub fn body_rows(&self, index: usize) -> Option<Range<u32>> {
        self.block(index).map(|block| block.body)
    }

    /// The block `count` before the one holding `row`, stopping at the first; `None` when already
    /// on the first block or `count` is 0.
    pub fn previous(&self, row: u32, count: usize) -> Option<BlockInfo> {
        let current = self.block_index_at(row);
        let target = current.saturating_sub(count);
        (target != current && count > 0)
            .then(|| self.block(target))
            .flatten()
    }

    /// The block `count` after the one holding `row`, stopping at the last; `None` when already on
    /// the last block or `count` is 0.
    pub fn next(&self, row: u32, count: usize) -> Option<BlockInfo> {
        let current = self.block_index_at(row);
        let target = current.saturating_add(count).min(self.block_count() - 1);
        (target != current && count > 0)
            .then(|| self.block(target))
            .flatten()
    }

    /// The next pending block after the one holding `row`; wraps to the top once, and the flag
    /// says it did. `None` when no other block is pending.
    pub fn next_pending(&self, row: u32) -> Option<(BlockInfo, bool)> {
        let current = self.block_index_at(row);
        let total = self.block_count();
        (current + 1..total)
            .map(|index| (index, false))
            .chain((0..current).map(|index| (index, true)))
            .filter_map(|(index, wrapped)| Some((self.block(index)?, wrapped)))
            .find(|(block, _)| block.is_pending())
    }
}

/// One edit as the editor reports it: rows of the text before the edit and rows of the text after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowEdit {
    pub old_rows: Range<u32>,
    pub new_rows: Range<u32>,
}

/// Merges edits that touch the same rows, given in ascending order, into one edit each, so a row is
/// parsed and shifted once (several cursors typing on one line). Edits that only neighbor each
/// other stay separate.
pub fn merge_row_edits(edits: &[RowEdit]) -> Vec<RowEdit> {
    let mut merged: Vec<RowEdit> = Vec::new();
    for edit in edits {
        let Some(last) = merged.last_mut() else {
            merged.push(edit.clone());
            continue;
        };
        if edit.old_rows.start >= last.old_rows.end && edit.old_rows.start != last.old_rows.start {
            merged.push(edit.clone());
            continue;
        }
        let old_end = last.old_rows.end.max(edit.old_rows.end);
        let new_end = (last.new_rows.end + (old_end - last.old_rows.end))
            .max(edit.new_rows.end + (old_end - edit.old_rows.end));
        last.old_rows.end = old_end;
        last.new_rows.end = new_end;
    }
    merged
}
