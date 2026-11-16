use super::{
    Highlights,
    fold_map::{self, Chunk, FoldChunks, FoldEdit, FoldOffset, FoldPoint, FoldSnapshot},
};

use language::{LanguageAwareStyling, Point};
use multi_buffer::MultiBufferSnapshot;
use std::{cmp, num::NonZeroU32, ops::Range};
use sum_tree::Bias;

const MAX_EXPANSION_COLUMN: u32 = 256;

// Handles a tab width <= 128
const SPACES: &[u8; rope::Chunk::MASK_BITS] = &[b' '; _];
const MAX_TABS: NonZeroU32 = NonZeroU32::new(SPACES.len() as u32).unwrap();

/// Keeps track of hard tabs in a text buffer.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct TabMap(TabSnapshot);

impl TabMap {
    #[ztracing::instrument(skip_all)]
    pub fn new(fold_snapshot: FoldSnapshot, tab_size: NonZeroU32) -> (Self, TabSnapshot) {
        let snapshot = TabSnapshot {
            fold_snapshot,
            tab_size: tab_size.min(MAX_TABS),
            max_expansion_column: MAX_EXPANSION_COLUMN,
            version: 0,
        };
        (Self(snapshot.clone()), snapshot)
    }

    #[cfg(test)]
    pub fn set_max_expansion_column(&mut self, column: u32) -> TabSnapshot {
        self.0.max_expansion_column = column;
        self.0.clone()
    }

    #[ztracing::instrument(skip_all)]
    pub fn sync(
        &mut self,
        fold_snapshot: FoldSnapshot,
        mut fold_edits: Vec<FoldEdit>,
        tab_size: NonZeroU32,
    ) -> (TabSnapshot, Vec<TabEdit>) {
        let tab_size = tab_size.min(MAX_TABS);

        if self.0.tab_size != tab_size {
            let old_max_point = self.0.max_point();
            self.0.version += 1;
            self.0.fold_snapshot = fold_snapshot;
            self.0.tab_size = tab_size;
            return (
                self.0.clone(),
                vec![TabEdit {
                    old: TabPoint::zero()..old_max_point,
                    new: TabPoint::zero()..self.0.max_point(),
                }],
            );
        }

        let old_snapshot = &mut self.0;
        let mut new_version = old_snapshot.version;
        if old_snapshot.fold_snapshot.version != fold_snapshot.version {
            new_version += 1;
        }

        if fold_edits.is_empty() {
            old_snapshot.version = new_version;
            old_snapshot.fold_snapshot = fold_snapshot;
            old_snapshot.tab_size = tab_size;
            return (old_snapshot.clone(), vec![]);
        }

        let old_fold_max_point = old_snapshot.fold_snapshot.max_point();
        let new_snapshot = TabSnapshot {
            fold_snapshot,
            tab_size,
            max_expansion_column: old_snapshot.max_expansion_column,
            version: new_version,
        };

        // Expand each edit to include the next tab on the same line as the edit,
        // and any subsequent tabs on that line that moved across the tab expansion
        // boundary.
        //
        // This is necessary because a tab's display width depends on its column
        // position: it expands to fill up to the next tab stop. When an edit
        // shifts text on a line, any tab character after the edit may now render
        // at a different width even though the tab byte itself wasn't touched.
        // Additionally, tabs beyond `max_expansion_column` are rendered as a
        // single space instead of expanding to the next tab stop. An edit that
        // shifts a tab across that boundary changes its display width, so the
        // edit must cover it. We scan forward from the edit end to the end of
        // the line, extending the edit to include the first subsequent tab (whose
        // rendered width may have changed) and the last tab that crossed the
        // expansion boundary (transitioning between expanded and non-expanded).
        //
        // Each edit is converted to tab points in the same pass. A row is read once
        // for both ends of an edit on it, the scan continues that read, and the
        // edit's end points and new start follow from offsets where they can, so a
        // batch of many edits (a replace-all) costs a few reads per edit instead of
        // a dozen seeks through every layer.
        let mut tab_edits: Vec<TabEdit> = Vec::with_capacity(fold_edits.len());
        let mut last_old_end = None;
        let mut previous_ends = (
            (FoldOffset::default(), FoldPoint::default()),
            (FoldOffset::default(), FoldPoint::default()),
        );
        for fold_edit in &mut fold_edits {
            let old_start = fold_edit.old.start.to_point(&old_snapshot.fold_snapshot);
            let mut old_row = old_snapshot.row_expansion(fold_edit.old.start, old_start);
            let old_start_column = old_snapshot.expand_tabs_to(&mut old_row, old_start.column());
            let (mut old_end, mut old_end_column) = old_snapshot.expand_tabs_to_edit_end(
                &mut old_row,
                (fold_edit.old.start, old_start),
                fold_edit.old.end,
            );

            // The text between this edit and the previous one is the same in both snapshots, so
            // the new start follows from the old start and the previous edit's ends.
            let (previous_old_end, previous_new_end) = previous_ends;
            let new_start = if fold_edit.old.start >= previous_old_end.0
                && fold_edit.new.start >= previous_new_end.0
                && fold_edit.old.start - previous_old_end.0
                    == fold_edit.new.start - previous_new_end.0
            {
                let (previous_old_end, previous_new_end) = (previous_old_end.1, previous_new_end.1);
                let new_start = if old_start.row() > previous_old_end.row() {
                    FoldPoint::new(
                        previous_new_end.row() + (old_start.row() - previous_old_end.row()),
                        old_start.column(),
                    )
                } else {
                    FoldPoint::new(
                        previous_new_end.row(),
                        previous_new_end.column()
                            + (old_start.column() - previous_old_end.column()),
                    )
                };
                debug_assert_eq!(
                    new_start,
                    fold_edit.new.start.to_point(&new_snapshot.fold_snapshot)
                );
                new_start
            } else {
                fold_edit.new.start.to_point(&new_snapshot.fold_snapshot)
            };
            let mut new_row = new_snapshot.row_expansion(fold_edit.new.start, new_start);
            let new_start_column = new_snapshot.expand_tabs_to(&mut new_row, new_start.column());
            let (mut new_end, mut new_end_column) = new_snapshot.expand_tabs_to_edit_end(
                &mut new_row,
                (fold_edit.new.start, new_start),
                fold_edit.new.end,
            );

            // The scan continues the old row's cursor when it stopped at the edit's end, and
            // reads the rest of the row afresh otherwise.
            let tab_offset = if old_row.cursor.byte_offset == old_end.column() {
                scan_tabs_after_edit(
                    old_row.cursor.into_rest_of_row(),
                    old_end,
                    new_end,
                    old_snapshot.max_expansion_column,
                )
            } else {
                let old_end_row_successor_offset =
                    cmp::min(FoldPoint::new(old_end.row() + 1, 0), old_fold_max_point)
                        .to_offset(&old_snapshot.fold_snapshot);
                scan_tabs_after_edit(
                    old_snapshot
                        .fold_snapshot
                        .chunks(
                            fold_edit.old.end..old_end_row_successor_offset,
                            LanguageAwareStyling {
                                tree_sitter: false,
                                diagnostics: false,
                            },
                            Highlights::default(),
                        )
                        .map(|chunk| (chunk.text.len() as u32, chunk.tabs)),
                    old_end,
                    new_end,
                    old_snapshot.max_expansion_column,
                )
            };

            previous_ends = ((fold_edit.old.end, old_end), (fold_edit.new.end, new_end));

            if let Some(offset) = tab_offset {
                fold_edit.old.end.0 += offset as usize + 1;
                fold_edit.new.end.0 += offset as usize + 1;
                old_end = fold_edit.old.end.to_point(&old_snapshot.fold_snapshot);
                new_end = fold_edit.new.end.to_point(&new_snapshot.fold_snapshot);
                old_end_column = old_snapshot.fold_point_to_tab_point(old_end).column();
                new_end_column = new_snapshot.fold_point_to_tab_point(new_end).column();
            }

            let old_start = TabPoint::new(old_start.row(), old_start_column);
            let old_end = TabPoint::new(old_end.row(), old_end_column);
            let new_start = TabPoint::new(new_start.row(), new_start_column);
            let new_end = TabPoint::new(new_end.row(), new_end_column);

            // Combine any edits that overlap due to the expansion.
            if let Some(last_tab_edit) = tab_edits.last_mut()
                && last_old_end.is_some_and(|last_old_end| last_old_end >= fold_edit.old.start)
            {
                last_tab_edit.old.end = old_end;
                last_tab_edit.new.end = new_end;
            } else {
                tab_edits.push(TabEdit {
                    old: old_start..old_end,
                    new: new_start..new_end,
                });
            }
            last_old_end = Some(fold_edit.old.end);
        }

        *old_snapshot = new_snapshot;
        (old_snapshot.clone(), tab_edits)
    }
}

#[derive(Clone)]
pub struct TabSnapshot {
    pub fold_snapshot: FoldSnapshot,
    pub tab_size: NonZeroU32,
    /// The maximum column up to which a tab can expand.
    /// Any tab after this column will not expand.
    pub max_expansion_column: u32,
    pub version: usize,
}

impl std::ops::Deref for TabSnapshot {
    type Target = FoldSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.fold_snapshot
    }
}

impl TabSnapshot {
    #[inline]
    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self.fold_snapshot.inlay_snapshot.buffer
    }

    #[ztracing::instrument(skip_all)]
    pub fn line_len(&self, row: u32) -> u32 {
        let max_point = self.max_point();
        if row < max_point.row() {
            self.fold_point_to_tab_point(FoldPoint::new(row, self.fold_snapshot.line_len(row)))
                .0
                .column
        } else {
            max_point.column()
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn text_summary(&self) -> TextSummary {
        self.text_summary_for_range(TabPoint::zero()..self.max_point())
    }

    #[ztracing::instrument(skip_all, fields(rows))]
    pub fn text_summary_for_range(&self, range: Range<TabPoint>) -> TextSummary {
        let input_start = self.tab_point_to_fold_point(range.start, Bias::Left).0;
        let input_end = self.tab_point_to_fold_point(range.end, Bias::Right).0;
        let input_summary = self
            .fold_snapshot
            .text_summary_for_range(input_start..input_end);

        let line_end = if range.start.row() == range.end.row() {
            range.end
        } else {
            self.max_point()
        };
        let first_line_chars = self
            .chunks(
                range.start..line_end,
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                Highlights::default(),
            )
            .flat_map(|chunk| chunk.text.chars())
            .take_while(|&c| c != '\n')
            .count() as u32;

        let last_line_chars = if range.start.row() == range.end.row() {
            first_line_chars
        } else {
            self.chunks(
                TabPoint::new(range.end.row(), 0)..range.end,
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                Highlights::default(),
            )
            .flat_map(|chunk| chunk.text.chars())
            .count() as u32
        };

        TextSummary {
            lines: range.end.0 - range.start.0,
            first_line_chars,
            last_line_chars,
            longest_row: input_summary.longest_row,
            longest_row_chars: input_summary.longest_row_chars,
        }
    }

    #[ztracing::instrument(skip_all)]
    pub(crate) fn chunks<'a>(
        &'a self,
        range: Range<TabPoint>,
        language_aware: LanguageAwareStyling,
        highlights: Highlights<'a>,
    ) -> TabChunks<'a> {
        let (input_start, expanded_char_column, to_next_stop) =
            self.tab_point_to_fold_point(range.start, Bias::Left);
        let input_column = input_start.column();
        let input_start = input_start.to_offset(&self.fold_snapshot);
        let input_end = self
            .tab_point_to_fold_point(range.end, Bias::Right)
            .0
            .to_offset(&self.fold_snapshot);
        let to_next_stop = if range.start.0 + Point::new(0, to_next_stop) > range.end.0 {
            range.end.column() - range.start.column()
        } else {
            to_next_stop
        };

        TabChunks {
            snapshot: self,
            fold_chunks: self.fold_snapshot.chunks(
                input_start..input_end,
                language_aware,
                highlights,
            ),
            input_column,
            column: expanded_char_column,
            max_expansion_column: self.max_expansion_column,
            output_position: range.start.0,
            max_output_position: range.end.0,
            tab_size: self.tab_size,
            chunk: Chunk {
                text: unsafe { std::str::from_utf8_unchecked(&SPACES[..to_next_stop as usize]) },
                is_tab: true,
                chars: 1u128.unbounded_shl(to_next_stop) - 1,
                ..Default::default()
            },
            inside_leading_tab: to_next_stop > 0,
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn rows(&self, row: u32) -> fold_map::FoldRows<'_> {
        self.fold_snapshot.row_infos(row)
    }

    #[cfg(test)]
    #[ztracing::instrument(skip_all)]
    pub fn text(&self) -> String {
        self.chunks(
            TabPoint::zero()..self.max_point(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Highlights::default(),
        )
        .map(|chunk| chunk.text)
        .collect()
    }

    #[ztracing::instrument(skip_all)]
    pub fn max_point(&self) -> TabPoint {
        self.fold_point_to_tab_point(self.fold_snapshot.max_point())
    }

    #[ztracing::instrument(skip_all)]
    pub fn clip_point(&self, point: TabPoint, bias: Bias) -> TabPoint {
        self.fold_point_to_tab_point(
            self.fold_snapshot
                .clip_point(self.tab_point_to_fold_point(point, bias).0, bias),
        )
    }

    #[ztracing::instrument(skip_all)]
    pub fn fold_point_to_tab_point(&self, input: FoldPoint) -> TabPoint {
        let chunks = self.fold_snapshot.chunks_at(FoldPoint::new(input.row(), 0));
        let tab_cursor = TabStopCursor::new(chunks);
        let expanded = self.expand_tabs(tab_cursor, input.column());
        TabPoint::new(input.row(), expanded)
    }

    /// Starts expanding the row of `point`, whose fold offset is `offset`, from its first column.
    /// Reads the same chunks as [`Self::fold_point_to_tab_point`] does for that row.
    fn row_expansion(&self, offset: FoldOffset, point: FoldPoint) -> TabExpansion<'_> {
        let row_start = FoldOffset(offset.0 - point.column() as usize);
        debug_assert_eq!(
            row_start,
            FoldPoint::new(point.row(), 0).to_offset(&self.fold_snapshot)
        );
        TabExpansion::new(TabStopCursor::new(
            self.fold_snapshot.chunks_at_offset(row_start),
        ))
    }

    /// Finds the fold point of an edit's end and its expanded column, continuing `expansion`,
    /// which stopped at the edit's start. When the edit stays on its start's row (no newline in
    /// its text, within the expansion column) the point follows from the edit's length without a
    /// seek; otherwise the end's row is expanded afresh. Either way `expansion` ends at the end.
    fn expand_tabs_to_edit_end<'a>(
        &'a self,
        expansion: &mut TabExpansion<'a>,
        (start_offset, start): (FoldOffset, FoldPoint),
        end_offset: FoldOffset,
    ) -> (FoldPoint, u32) {
        let column = start.column() as usize + (end_offset.0 - start_offset.0);
        if column <= self.max_expansion_column as usize {
            let column = column as u32;
            expansion.cursor.crossed_newline = Some(false);
            let expanded = self.expand_tabs_to(expansion, column);
            let crossed_newline = expansion.cursor.crossed_newline.take();
            if expansion.cursor.byte_offset == column && crossed_newline == Some(false) {
                let end = FoldPoint::new(start.row(), column);
                debug_assert_eq!(end, end_offset.to_point(&self.fold_snapshot));
                return (end, expanded);
            }
        }
        let end = end_offset.to_point(&self.fold_snapshot);
        *expansion = self.row_expansion(end_offset, end);
        let expanded = self.expand_tabs_to(expansion, end.column());
        (end, expanded)
    }

    #[ztracing::instrument(skip_all)]
    pub fn tab_point_cursor(&self) -> TabPointCursor<'_> {
        TabPointCursor { this: self }
    }

    #[ztracing::instrument(skip_all)]
    pub fn tab_point_to_fold_point(&self, output: TabPoint, bias: Bias) -> (FoldPoint, u32, u32) {
        let chunks = self
            .fold_snapshot
            .chunks_at(FoldPoint::new(output.row(), 0));

        let tab_cursor = TabStopCursor::new(chunks);
        let expanded = output.column();
        let (collapsed, expanded_char_column, to_next_stop) =
            self.collapse_tabs(tab_cursor, expanded, bias);

        (
            FoldPoint::new(output.row(), collapsed),
            expanded_char_column,
            to_next_stop,
        )
    }

    #[ztracing::instrument(skip_all)]
    pub fn point_to_tab_point(&self, point: Point, bias: Bias) -> TabPoint {
        let inlay_point = self.fold_snapshot.inlay_snapshot.to_inlay_point(point);
        let fold_point = self.fold_snapshot.to_fold_point(inlay_point, bias);
        self.fold_point_to_tab_point(fold_point)
    }

    #[ztracing::instrument(skip_all)]
    pub fn tab_point_to_point(&self, point: TabPoint, bias: Bias) -> Point {
        let fold_point = self.tab_point_to_fold_point(point, bias).0;
        let inlay_point = fold_point.to_inlay_point(&self.fold_snapshot);
        self.fold_snapshot
            .inlay_snapshot
            .to_buffer_point(inlay_point)
    }

    #[ztracing::instrument(skip_all)]
    fn expand_tabs<'a>(&self, cursor: TabStopCursor<'a>, column: u32) -> u32 {
        self.expand_tabs_to(&mut TabExpansion::new(cursor), column)
    }

    /// Expands the row up to `column`, continuing from where `expansion` stopped, so a later
    /// column on the same row costs only the bytes between the two. Each call must pass a column
    /// no smaller than the previous one.
    fn expand_tabs_to(&self, expansion: &mut TabExpansion<'_>, column: u32) -> u32 {
        // we only ever act on a single row at a time
        // so the main difference is that other layers build a transform sumtree, and can then just run through that
        // we cant quite do this here, as we need to work with the previous layer chunk to understand the tabs of the corresponding row
        // we can still do forward searches for this though, we search for a row, then traverse the column up to where we need to be
        let tab_size = self.tab_size.get();
        let TabExpansion {
            cursor,
            tab_count,
            expanded_tab_len,
        } = expansion;

        let end_column = column.min(self.max_expansion_column);
        debug_assert!(cursor.byte_offset <= end_column);
        let mut seek_target = end_column - cursor.byte_offset;

        while let Some(tab_stop) = cursor.seek_forward(seek_target) {
            let expanded_chars_old = tab_stop.char_offset + *expanded_tab_len - *tab_count;
            let tab_len = tab_size - ((expanded_chars_old - 1) % tab_size);
            *tab_count += 1;
            *expanded_tab_len += tab_len;

            seek_target = end_column - cursor.byte_offset;
        }

        let left_over_char_bytes = if !cursor.is_char_boundary() {
            cursor.bytes_until_next_char().unwrap_or(0) as u32
        } else {
            0
        };

        let collapsed_bytes = cursor.byte_offset() + left_over_char_bytes;
        let expanded_bytes =
            cursor.byte_offset() + *expanded_tab_len - *tab_count + left_over_char_bytes;

        expanded_bytes + column.saturating_sub(collapsed_bytes)
    }

    #[ztracing::instrument(skip_all)]
    fn collapse_tabs<'a>(
        &self,
        mut cursor: TabStopCursor<'a>,
        column: u32,
        bias: Bias,
    ) -> (u32, u32, u32) {
        let tab_size = self.tab_size.get();
        let mut collapsed_column = column;
        let mut seek_target = column.min(self.max_expansion_column);
        let mut tab_count = 0;
        let mut expanded_tab_len = 0;

        while let Some(tab_stop) = cursor.seek_forward(seek_target) {
            // Calculate how much we want to expand this tab stop (into spaces)
            let expanded_chars_old = tab_stop.char_offset + expanded_tab_len - tab_count;
            let tab_len = tab_size - ((expanded_chars_old - 1) % tab_size);
            // Increment tab count
            tab_count += 1;
            // The count of how many spaces we've added to this line in place of tab bytes
            expanded_tab_len += tab_len;

            // The count of bytes at this point in the iteration while considering tab_count and previous expansions
            let expanded_bytes = tab_stop.byte_offset + expanded_tab_len - tab_count;

            // Did we expand past the search target?
            if expanded_bytes > column {
                let mut expanded_chars = tab_stop.char_offset + expanded_tab_len - tab_count;
                // We expanded past the search target, so need to account for the offshoot
                expanded_chars -= expanded_bytes - column;
                return match bias {
                    Bias::Left => (
                        cursor.byte_offset() - 1,
                        expanded_chars,
                        expanded_bytes - column,
                    ),
                    Bias::Right => (cursor.byte_offset(), expanded_chars, 0),
                };
            } else {
                // otherwise we only want to move the cursor collapse column forward
                collapsed_column = collapsed_column - tab_len + 1;
                seek_target = (collapsed_column - cursor.byte_offset)
                    .min(self.max_expansion_column - cursor.byte_offset);
            }
        }

        let collapsed_bytes = cursor.byte_offset();
        let expanded_bytes = cursor.byte_offset() + expanded_tab_len - tab_count;
        let expanded_chars = cursor.char_offset() + expanded_tab_len - tab_count;
        (
            collapsed_bytes + column.saturating_sub(expanded_bytes),
            expanded_chars,
            0,
        )
    }
}

// todo(lw): Implement TabPointCursor properly
pub struct TabPointCursor<'this> {
    this: &'this TabSnapshot,
}

impl TabPointCursor<'_> {
    /// No-op; this cursor is stateless. Provided for symmetry with the other
    /// display-map layer cursors.
    pub fn reset(&mut self) {}

    #[ztracing::instrument(skip_all)]
    pub fn map(&mut self, point: FoldPoint) -> TabPoint {
        self.this.fold_point_to_tab_point(point)
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct TabPoint(pub Point);

impl TabPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }

    pub fn zero() -> Self {
        Self::new(0, 0)
    }

    pub fn row(self) -> u32 {
        self.0.row
    }

    pub fn column(self) -> u32 {
        self.0.column
    }
}

impl From<Point> for TabPoint {
    fn from(point: Point) -> Self {
        Self(point)
    }
}

pub type TabEdit = text::Edit<TabPoint>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextSummary {
    pub lines: Point,
    pub first_line_chars: u32,
    pub last_line_chars: u32,
    pub longest_row: u32,
    pub longest_row_chars: u32,
}

impl<'a> From<&'a str> for TextSummary {
    #[ztracing::instrument(skip_all)]
    fn from(text: &'a str) -> Self {
        let sum = text::TextSummary::from(text);

        TextSummary {
            lines: sum.lines,
            first_line_chars: sum.first_line_chars,
            last_line_chars: sum.last_line_chars,
            longest_row: sum.longest_row,
            longest_row_chars: sum.longest_row_chars,
        }
    }
}

impl<'a> std::ops::AddAssign<&'a Self> for TextSummary {
    #[ztracing::instrument(skip_all)]
    fn add_assign(&mut self, other: &'a Self) {
        let joined_chars = self.last_line_chars + other.first_line_chars;
        if joined_chars > self.longest_row_chars {
            self.longest_row = self.lines.row;
            self.longest_row_chars = joined_chars;
        }
        if other.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.lines.row + other.longest_row;
            self.longest_row_chars = other.longest_row_chars;
        }

        if self.lines.row == 0 {
            self.first_line_chars += other.first_line_chars;
        }

        if other.lines.row == 0 {
            self.last_line_chars += other.first_line_chars;
        } else {
            self.last_line_chars = other.last_line_chars;
        }

        self.lines += &other.lines;
    }
}

pub struct TabChunks<'a> {
    snapshot: &'a TabSnapshot,
    max_expansion_column: u32,
    max_output_position: Point,
    tab_size: NonZeroU32,
    // region: iteration state
    fold_chunks: FoldChunks<'a>,
    chunk: Chunk<'a>,
    column: u32,
    output_position: Point,
    input_column: u32,
    inside_leading_tab: bool,
    // endregion: iteration state
}

impl TabChunks<'_> {
    #[ztracing::instrument(skip_all)]
    pub(crate) fn seek(&mut self, range: Range<TabPoint>) {
        let (input_start, expanded_char_column, to_next_stop) = self
            .snapshot
            .tab_point_to_fold_point(range.start, Bias::Left);
        let input_column = input_start.column();
        let input_start = input_start.to_offset(&self.snapshot.fold_snapshot);
        let input_end = self
            .snapshot
            .tab_point_to_fold_point(range.end, Bias::Right)
            .0
            .to_offset(&self.snapshot.fold_snapshot);
        let to_next_stop = if range.start.0 + Point::new(0, to_next_stop) > range.end.0 {
            range.end.column() - range.start.column()
        } else {
            to_next_stop
        };

        self.fold_chunks.seek(input_start..input_end);
        self.input_column = input_column;
        self.column = expanded_char_column;
        self.output_position = range.start.0;
        self.max_output_position = range.end.0;
        self.chunk = Chunk {
            text: unsafe { std::str::from_utf8_unchecked(&SPACES[..to_next_stop as usize]) },
            is_tab: true,
            chars: 1u128.unbounded_shl(to_next_stop) - 1,
            ..Default::default()
        };
        self.inside_leading_tab = to_next_stop > 0;
    }
}

impl<'a> Iterator for TabChunks<'a> {
    type Item = Chunk<'a>;

    #[ztracing::instrument(skip_all)]
    fn next(&mut self) -> Option<Self::Item> {
        while self.chunk.text.is_empty() {
            let chunk = self.fold_chunks.next()?;
            self.chunk = chunk;
            if self.inside_leading_tab {
                self.chunk.text = &self.chunk.text[1..];
                self.chunk.tabs >>= 1;
                self.chunk.chars >>= 1;
                self.chunk.newlines >>= 1;
                self.inside_leading_tab = false;
                self.input_column += 1;
            }
        }

        if self.chunk.tabs & 1 != 0 {
            self.chunk.text = &self.chunk.text[1..];
            self.chunk.tabs >>= 1;
            self.chunk.chars >>= 1;
            self.chunk.newlines >>= 1;

            let tab_size = if self.input_column < self.max_expansion_column {
                self.tab_size.get()
            } else {
                1
            };
            let mut len = tab_size - self.column % tab_size;
            let next_output_position = cmp::min(
                self.output_position + Point::new(0, len),
                self.max_output_position,
            );
            len = next_output_position.column - self.output_position.column;
            self.column += len;
            self.input_column += 1;
            self.output_position = next_output_position;

            return Some(Chunk {
                text: unsafe { std::str::from_utf8_unchecked(&SPACES[..len as usize]) },
                is_tab: true,
                chars: 1u128.unbounded_shl(len) - 1,
                tabs: 0,
                newlines: 0,
                ..self.chunk.clone()
            });
        }

        // Fast path: no tabs in the remaining chunk, return it directly
        if self.chunk.tabs == 0 {
            let chunk = self.chunk.clone();
            self.chunk.text = "";
            self.chunk.tabs = 0;
            self.chunk.chars = 0;
            self.chunk.newlines = 0;
            let chunk_len = chunk.text.len() as u32;

            let newline_count = chunk.newlines.count_ones();
            if newline_count > 0 {
                let last_newline_bit = 128 - chunk.newlines.leading_zeros();
                let chars_after_last_newline =
                    chunk.chars.unbounded_shr(last_newline_bit).count_ones();
                let bytes_after_last_newline = chunk_len - last_newline_bit;

                self.column = chars_after_last_newline;
                self.input_column = bytes_after_last_newline;
                self.output_position = Point::new(
                    self.output_position.row + newline_count,
                    bytes_after_last_newline,
                );
            } else {
                let char_count = chunk.chars.count_ones();
                self.column += char_count;
                if !self.inside_leading_tab {
                    self.input_column += chunk_len;
                }
                self.output_position.column += chunk_len;
            }

            return Some(chunk);
        }

        // Split at the next tab position
        let prefix_len = self.chunk.tabs.trailing_zeros() as usize;
        let (prefix, suffix) = self.chunk.text.split_at(prefix_len);

        let mask = 1u128.unbounded_shl(prefix_len as u32).wrapping_sub(1);
        let prefix_chars = self.chunk.chars & mask;
        let prefix_newlines = self.chunk.newlines & mask;

        self.chunk.text = suffix;
        self.chunk.tabs = self.chunk.tabs.unbounded_shr(prefix_len as u32);
        self.chunk.chars = self.chunk.chars.unbounded_shr(prefix_len as u32);
        self.chunk.newlines = self.chunk.newlines.unbounded_shr(prefix_len as u32);

        let newline_count = prefix_newlines.count_ones();
        if newline_count > 0 {
            let last_newline_bit = 128 - prefix_newlines.leading_zeros();
            let chars_after_last_newline =
                prefix_chars.unbounded_shr(last_newline_bit).count_ones();
            let bytes_after_last_newline = prefix_len as u32 - last_newline_bit;

            self.column = chars_after_last_newline;
            self.input_column = bytes_after_last_newline;
            self.output_position = Point::new(
                self.output_position.row + newline_count,
                bytes_after_last_newline,
            );
        } else {
            let char_count = prefix_chars.count_ones();
            self.column += char_count;
            if !self.inside_leading_tab {
                self.input_column += prefix_len as u32;
            }
            self.output_position.column += prefix_len as u32;
        }

        Some(Chunk {
            text: prefix,
            chars: prefix_chars,
            tabs: 0,
            newlines: prefix_newlines,
            ..self.chunk.clone()
        })
    }
}

/// A row's tab expansion in progress: the cursor and the tabs it has passed.
struct TabExpansion<'a> {
    cursor: TabStopCursor<'a>,
    tab_count: u32,
    expanded_tab_len: u32,
}

impl<'a> TabExpansion<'a> {
    fn new(cursor: TabStopCursor<'a>) -> Self {
        Self {
            cursor,
            tab_count: 0,
            expanded_tab_len: 0,
        }
    }
}

struct TabStopCursor<'a> {
    chunks: FoldChunks<'a>,
    byte_offset: u32,
    char_offset: u32,
    /// Whether a newline was passed, tracked only while `Some`.
    crossed_newline: Option<bool>,
    /// Chunk
    /// last tab position iterated through
    current_chunk: Option<(TabStopChunk<'a>, u32)>,
}

struct TabStopChunk<'a> {
    chars: u128,
    text: &'a str,
    tabs: u128,
}

impl<'a> TabStopCursor<'a> {
    fn new(chunks: FoldChunks<'a>) -> Self {
        Self {
            chunks,
            byte_offset: 0,
            char_offset: 0,
            crossed_newline: None,
            current_chunk: None,
        }
    }

    /// Records whether `text[range]` of the chunk being passed holds a newline.
    #[inline(always)]
    fn pass_bytes(&mut self, text: &str, range: Range<u32>) {
        if let Some(crossed_newline) = &mut self.crossed_newline
            && !*crossed_newline
        {
            *crossed_newline =
                text.as_bytes()[range.start as usize..range.end as usize].contains(&b'\n');
        }
    }

    /// The chunks from the cursor to the end of its row, as each chunk's length in bytes and
    /// its tabs, the last one ending with the row's newline. These are the chunks
    /// [`FoldSnapshot::chunks`] yields from the cursor's offset to the next row's start.
    fn into_rest_of_row(self) -> impl Iterator<Item = (u32, u128)> + use<'a> {
        let current = self.current_chunk.and_then(|(chunk, position)| {
            let text = &chunk.text.as_bytes()[position as usize..];
            (!text.is_empty()).then(|| {
                (
                    text,
                    chunk.tabs.unbounded_shr(position) & low_bits(text.len() as u32),
                )
            })
        });
        current
            .into_iter()
            .chain(self.chunks.map(|chunk| (chunk.text.as_bytes(), chunk.tabs)))
            .scan(false, |row_ended, (text, tabs)| {
                if *row_ended {
                    return None;
                }
                Some(match text.iter().position(|&byte| byte == b'\n') {
                    Some(newline) => {
                        *row_ended = true;
                        (newline as u32 + 1, tabs & low_bits(newline as u32))
                    }
                    None => (text.len() as u32, tabs),
                })
            })
    }

    fn bytes_until_next_char(&self) -> Option<usize> {
        self.current_chunk.as_ref().map(|&(ref chunk, idx)| {
            let higher_chars = chunk.chars.unbounded_shr(idx + 1);

            if higher_chars != 0 {
                higher_chars.trailing_zeros() as usize + 1
            } else {
                chunk.text.len() - idx as usize
            }
        })
    }

    fn is_char_boundary(&self) -> bool {
        self.current_chunk
            .as_ref()
            .is_some_and(|&(ref chunk, idx)| {
                (1u128.unbounded_shl(idx) & chunk.chars) != 0 || idx as usize == chunk.text.len()
            })
    }

    /// distance: length to move forward while searching for the next tab stop
    #[ztracing::instrument(skip_all)]
    fn seek_forward(&mut self, distance: u32) -> Option<TabStop> {
        if distance == 0 {
            return None;
        }

        let mut distance_remaining = distance;

        while let Some((mut chunk, chunk_position)) = self.current_chunk.take().or_else(|| {
            self.chunks.next().map(|chunk| {
                (
                    TabStopChunk {
                        chars: chunk.chars,
                        text: chunk.text,
                        tabs: chunk.tabs,
                    },
                    0,
                )
            })
        }) {
            let chunk_len = chunk.text.len() as u32;

            if chunk.tabs == 0 {
                let chunk_remaining = chunk_len - chunk_position;
                if chunk_remaining >= distance_remaining {
                    let end = chunk_position + distance_remaining;
                    self.pass_bytes(chunk.text, chunk_position..end);
                    self.byte_offset += distance_remaining;
                    self.char_offset +=
                        count_chars_in_byte_range(chunk_position..(end - 1), chunk.chars);
                    if end < 128 {
                        self.current_chunk = Some((chunk, end));
                    }
                    return None;
                }

                self.pass_bytes(chunk.text, chunk_position..chunk_len);
                self.byte_offset += chunk_remaining;
                self.char_offset +=
                    count_chars_in_byte_range(chunk_position..(chunk_len - 1), chunk.chars);
                distance_remaining -= chunk_remaining;
                continue;
            }

            let tab_end = chunk.tabs.trailing_zeros() + 1;
            let bytes_to_tab = tab_end - chunk_position;

            if bytes_to_tab > distance_remaining {
                let end = chunk_position + distance_remaining;
                self.pass_bytes(chunk.text, chunk_position..end);
                self.byte_offset += distance_remaining;
                self.char_offset +=
                    count_chars_in_byte_range(chunk_position..(end - 1), chunk.chars);
                self.current_chunk = Some((chunk, end));
                return None;
            }

            self.pass_bytes(chunk.text, chunk_position..tab_end);
            self.byte_offset += bytes_to_tab;
            self.char_offset +=
                count_chars_in_byte_range(chunk_position..(tab_end - 1), chunk.chars);

            let tabstop = TabStop {
                char_offset: self.char_offset,
                byte_offset: self.byte_offset,
            };

            chunk.tabs = (chunk.tabs - 1) & chunk.tabs;

            if tab_end != chunk_len {
                self.current_chunk = Some((chunk, tab_end));
            }

            return Some(tabstop);
        }

        None
    }

    fn byte_offset(&self) -> u32 {
        self.byte_offset
    }

    fn char_offset(&self) -> u32 {
        self.char_offset
    }
}

/// A mask of the bits below `len`.
#[inline(always)]
fn low_bits(len: u32) -> u128 {
    1u128.unbounded_shl(len).wrapping_sub(1)
}

/// Scans the rest of an edit's row, given as each chunk's length and tabs from the edit's old
/// end, for the tab the edit must be extended to: the first tab after the edit, or the last one
/// whose expansion changes because the edit moved it across the expansion column. Returns its
/// offset from the edit's end.
fn scan_tabs_after_edit(
    chunks: impl Iterator<Item = (u32, u128)>,
    old_end: FoldPoint,
    new_end: FoldPoint,
    max_expansion_column: u32,
) -> Option<u32> {
    let mut offset_from_edit = 0;
    let mut first_tab_offset = None;
    let mut last_tab_with_changed_expansion_offset = None;
    'outer: for (len, tabs) in chunks {
        let mut remaining_tabs = tabs;
        while remaining_tabs != 0 {
            let ix = remaining_tabs.trailing_zeros();
            let offset_from_edit = offset_from_edit + ix;
            if first_tab_offset.is_none() {
                first_tab_offset = Some(offset_from_edit);
            }

            let old_column = old_end.column() + offset_from_edit;
            let new_column = new_end.column() + offset_from_edit;
            let was_expanded = old_column < max_expansion_column;
            let is_expanded = new_column < max_expansion_column;
            if was_expanded != is_expanded {
                last_tab_with_changed_expansion_offset = Some(offset_from_edit);
            } else if !was_expanded && !is_expanded {
                break 'outer;
            }

            remaining_tabs &= remaining_tabs - 1;
        }

        offset_from_edit += len;
        if old_end.column() + offset_from_edit >= max_expansion_column
            && new_end.column() + offset_from_edit >= max_expansion_column
        {
            break;
        }
    }
    last_tab_with_changed_expansion_offset.or(first_tab_offset)
}

#[inline(always)]
fn count_chars_in_byte_range(range: Range<u32>, bitmap: u128) -> u32 {
    let low_mask = u128::MAX << range.start;
    let high_mask = u128::MAX >> (127 - range.end);
    (bitmap & low_mask & high_mask).count_ones()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabStop {
    char_offset: u32,
    byte_offset: u32,
}

#[cfg(test)]
mod tests {
    use std::mem;

    use super::*;
    use crate::{
        MultiBuffer,
        display_map::{
            fold_map::{FoldMap, FoldOffset, FoldPlaceholder},
            inlay_map::InlayMap,
        },
    };
    use multi_buffer::MultiBufferOffset;
    use rand::{Rng, prelude::StdRng};
    use util;

    impl TabSnapshot {
        fn expected_collapse_tabs(
            &self,
            chars: impl Iterator<Item = char>,
            column: u32,
            bias: Bias,
        ) -> (u32, u32, u32) {
            let tab_size = self.tab_size.get();

            let mut expanded_bytes = 0;
            let mut expanded_chars = 0;
            let mut collapsed_bytes = 0;
            for c in chars {
                if expanded_bytes >= column {
                    break;
                }
                if collapsed_bytes >= self.max_expansion_column {
                    break;
                }

                if c == '\t' {
                    let tab_len = tab_size - (expanded_chars % tab_size);
                    expanded_chars += tab_len;
                    expanded_bytes += tab_len;
                    if expanded_bytes > column {
                        expanded_chars -= expanded_bytes - column;
                        return match bias {
                            Bias::Left => {
                                (collapsed_bytes, expanded_chars, expanded_bytes - column)
                            }
                            Bias::Right => (collapsed_bytes + 1, expanded_chars, 0),
                        };
                    }
                } else {
                    expanded_chars += 1;
                    expanded_bytes += c.len_utf8() as u32;
                }

                if expanded_bytes > column && matches!(bias, Bias::Left) {
                    expanded_chars -= 1;
                    break;
                }

                collapsed_bytes += c.len_utf8() as u32;
            }

            (
                collapsed_bytes + column.saturating_sub(expanded_bytes),
                expanded_chars,
                0,
            )
        }

        pub fn expected_to_tab_point(&self, input: FoldPoint) -> TabPoint {
            let chars = self.fold_snapshot.chars_at(FoldPoint::new(input.row(), 0));
            let expanded = self.expected_expand_tabs(chars, input.column());
            TabPoint::new(input.row(), expanded)
        }

        fn expected_expand_tabs(&self, chars: impl Iterator<Item = char>, column: u32) -> u32 {
            let tab_size = self.tab_size.get();

            let mut expanded_chars = 0;
            let mut expanded_bytes = 0;
            let mut collapsed_bytes = 0;
            let end_column = column.min(self.max_expansion_column);
            for c in chars {
                if collapsed_bytes >= end_column {
                    break;
                }
                if c == '\t' {
                    let tab_len = tab_size - expanded_chars % tab_size;
                    expanded_bytes += tab_len;
                    expanded_chars += tab_len;
                } else {
                    expanded_bytes += c.len_utf8() as u32;
                    expanded_chars += 1;
                }
                collapsed_bytes += c.len_utf8() as u32;
            }

            expanded_bytes + column.saturating_sub(collapsed_bytes)
        }

        fn expected_to_fold_point(&self, output: TabPoint, bias: Bias) -> (FoldPoint, u32, u32) {
            let chars = self.fold_snapshot.chars_at(FoldPoint::new(output.row(), 0));
            let expanded = output.column();
            let (collapsed, expanded_char_column, to_next_stop) =
                self.expected_collapse_tabs(chars, expanded, bias);
            (
                FoldPoint::new(output.row(), collapsed),
                expanded_char_column,
                to_next_stop,
            )
        }
    }

    #[gpui::test]
    fn test_expand_tabs(cx: &mut gpui::App) {
        let input = "A\tBC\tDEF\tG\tHI\tJ\tK\tL\tM";

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        for (ix, _) in input.char_indices() {
            let fold_point = FoldPoint::new(0, ix as u32);

            assert_eq!(
                tab_snapshot.expected_to_tab_point(fold_point),
                tab_snapshot.fold_point_to_tab_point(fold_point),
                "Failed with fold_point at column {ix}"
            );
        }
    }

    #[gpui::test]
    fn test_collapse_tabs(cx: &mut gpui::App) {
        let input = "A\tBC\tDEF\tG\tHI\tJ\tK\tL\tM";

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        for (ix, _) in input.char_indices() {
            let range = TabPoint::new(0, ix as u32)..tab_snapshot.max_point();

            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.start, Bias::Left),
                tab_snapshot.tab_point_to_fold_point(range.start, Bias::Left),
                "Failed with tab_point at column {ix}"
            );
            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.start, Bias::Right),
                tab_snapshot.tab_point_to_fold_point(range.start, Bias::Right),
                "Failed with tab_point at column {ix}"
            );

            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.end, Bias::Left),
                tab_snapshot.tab_point_to_fold_point(range.end, Bias::Left),
                "Failed with tab_point at column {ix}"
            );
            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.end, Bias::Right),
                tab_snapshot.tab_point_to_fold_point(range.end, Bias::Right),
                "Failed with tab_point at column {ix}"
            );
        }
    }

    #[gpui::test]
    fn test_to_fold_point_panic_reproduction(cx: &mut gpui::App) {
        // This test reproduces a specific panic where to_fold_point returns incorrect results
        let _text = "use macro_rules_attribute::apply;\nuse serde_json::Value;\nuse smol::{\n    io::AsyncReadExt,\n    process::{Command, Stdio},\n};\nuse smol_macros::main;\nuse std::io;\n\nfn test_random() {\n    // Generate a random value\n    let random_value = std::time::SystemTime::now()\n        .duration_since(std::time::UNIX_EPOCH)\n        .unwrap()\n        .as_secs()\n        % 100;\n\n    // Create some complex nested data structures\n    let mut vector = Vec::new();\n    for i in 0..random_value {\n        vector.push(i);\n    }\n    ";

        let text = "γ\tw⭐\n🍐🍗 \t";
        let buffer = MultiBuffer::build_simple(text, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        // This should panic with the expected vs actual mismatch
        let tab_point = TabPoint::new(0, 9);
        let result = tab_snapshot.tab_point_to_fold_point(tab_point, Bias::Left);
        let expected = tab_snapshot.expected_to_fold_point(tab_point, Bias::Left);

        assert_eq!(result, expected);
    }

    #[gpui::test(iterations = 100)]
    fn test_collapse_tabs_random(cx: &mut gpui::App, mut rng: StdRng) {
        // Generate random input string with up to 200 characters including tabs
        // to stay within the MAX_EXPANSION_COLUMN limit of 256
        let len = rng.random_range(0..=2048);
        let tab_size = NonZeroU32::new(rng.random_range(1..=4)).unwrap();
        let mut input = String::with_capacity(len);

        for _ in 0..len {
            if rng.random_bool(0.1) {
                // 10% chance of inserting a tab
                input.push('\t');
            } else {
                // 90% chance of inserting a random ASCII character (excluding tab, newline, carriage return)
                let ch = loop {
                    let ascii_code = rng.random_range(32..=126); // printable ASCII range
                    let ch = ascii_code as u8 as char;
                    if ch != '\t' {
                        break ch;
                    }
                };
                input.push(ch);
            }
        }

        let buffer = MultiBuffer::build_simple(&input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, mut tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());
        tab_snapshot.max_expansion_column = rng.random_range(0..323);
        tab_snapshot.tab_size = tab_size;

        for (ix, _) in input.char_indices() {
            let range = TabPoint::new(0, ix as u32)..tab_snapshot.max_point();

            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.start, Bias::Left),
                tab_snapshot.tab_point_to_fold_point(range.start, Bias::Left),
                "Failed with input: {}, with idx: {ix}",
                input
            );
            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.start, Bias::Right),
                tab_snapshot.tab_point_to_fold_point(range.start, Bias::Right),
                "Failed with input: {}, with idx: {ix}",
                input
            );

            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.end, Bias::Left),
                tab_snapshot.tab_point_to_fold_point(range.end, Bias::Left),
                "Failed with input: {}, with idx: {ix}",
                input
            );
            assert_eq!(
                tab_snapshot.expected_to_fold_point(range.end, Bias::Right),
                tab_snapshot.tab_point_to_fold_point(range.end, Bias::Right),
                "Failed with input: {}, with idx: {ix}",
                input
            );
        }
    }

    #[gpui::test]
    fn test_long_lines(cx: &mut gpui::App) {
        let max_expansion_column = 12;
        let input = "A\tBC\tDEF\tG\tHI\tJ\tK\tL\tM";
        let output = "A   BC  DEF G   HI J K L M";

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, mut tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        tab_snapshot.max_expansion_column = max_expansion_column;
        assert_eq!(tab_snapshot.text(), output);

        for (ix, c) in input.char_indices() {
            assert_eq!(
                tab_snapshot
                    .chunks(
                        TabPoint::new(0, ix as u32)..tab_snapshot.max_point(),
                        LanguageAwareStyling {
                            tree_sitter: false,
                            diagnostics: false,
                        },
                        Highlights::default(),
                    )
                    .map(|c| c.text)
                    .collect::<String>(),
                &output[ix..],
                "text from index {ix}"
            );

            if c != '\t' {
                let input_point = Point::new(0, ix as u32);
                let output_point = Point::new(0, output.find(c).unwrap() as u32);
                assert_eq!(
                    tab_snapshot.fold_point_to_tab_point(FoldPoint(input_point)),
                    TabPoint(output_point),
                    "to_tab_point({input_point:?})"
                );
                assert_eq!(
                    tab_snapshot
                        .tab_point_to_fold_point(TabPoint(output_point), Bias::Left)
                        .0,
                    FoldPoint(input_point),
                    "to_fold_point({output_point:?})"
                );
            }
        }
    }

    #[gpui::test]
    fn test_long_lines_with_character_spanning_max_expansion_column(cx: &mut gpui::App) {
        let max_expansion_column = 8;
        let input = "abcdefg⋯hij";

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, mut tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        tab_snapshot.max_expansion_column = max_expansion_column;
        assert_eq!(tab_snapshot.text(), input);
    }

    #[gpui::test]
    fn test_marking_tabs(cx: &mut gpui::App) {
        let input = "\t \thello";

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (_, tab_snapshot) = TabMap::new(fold_snapshot, 4.try_into().unwrap());

        assert_eq!(
            chunks(&tab_snapshot, TabPoint::zero()),
            vec![
                ("    ".to_string(), true),
                (" ".to_string(), false),
                ("   ".to_string(), true),
                ("hello".to_string(), false),
            ]
        );
        assert_eq!(
            chunks(&tab_snapshot, TabPoint::new(0, 2)),
            vec![
                ("  ".to_string(), true),
                (" ".to_string(), false),
                ("   ".to_string(), true),
                ("hello".to_string(), false),
            ]
        );

        fn chunks(snapshot: &TabSnapshot, start: TabPoint) -> Vec<(String, bool)> {
            let mut chunks = Vec::new();
            let mut was_tab = false;
            let mut text = String::new();
            for chunk in snapshot.chunks(
                start..snapshot.max_point(),
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                Highlights::default(),
            ) {
                if chunk.is_tab != was_tab {
                    if !text.is_empty() {
                        chunks.push((mem::take(&mut text), was_tab));
                    }
                    was_tab = chunk.is_tab;
                }
                text.push_str(chunk.text);
            }

            if !text.is_empty() {
                chunks.push((text, was_tab));
            }
            chunks
        }
    }

    #[gpui::test]
    fn test_empty_chunk_after_leading_tab_trim(cx: &mut gpui::App) {
        // We fold "hello" (offsets 1..6) so the fold map creates a
        // transform boundary at offset 1, producing a 1-byte fold chunk
        // for the tab.
        let text = "\thello";
        let buffer = MultiBuffer::build_simple(text, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut fold_map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = fold_map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![(
            MultiBufferOffset(1)..MultiBufferOffset(6),
            FoldPlaceholder::test(),
        )]);
        let (fold_snapshot, _) = fold_map.read(inlay_snapshot, vec![]);

        let tab_size = NonZeroU32::new(4).unwrap();
        let (_, tab_snapshot) = TabMap::new(fold_snapshot, tab_size);

        // The tab at column 0 expands to 4 spaces (columns 0‥4).
        // Seek starting at column 2 (middle of that tab) so that
        // `inside_leading_tab = true` and `to_next_stop = 2`.
        // Set the end just past the tab expansion so the iterator must
        // process the tab byte from the fold chunk.
        let max = tab_snapshot.max_point();
        let start = TabPoint::new(0, 2);
        let end = max;

        // This should not panic.
        let result: String = tab_snapshot
            .chunks(
                start..end,
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                Highlights::default(),
            )
            .map(|c| c.text)
            .collect();
        assert!(!result.is_empty());
    }

    #[gpui::test(iterations = 100)]
    fn test_random_tabs(cx: &mut gpui::App, mut rng: StdRng) {
        let tab_size = NonZeroU32::new(rng.random_range(1..=4)).unwrap();
        let len = rng.random_range(0..30);
        let buffer = if rng.random() {
            let text = util::RandomCharIter::new(&mut rng)
                .take(len)
                .collect::<String>();
            MultiBuffer::build_simple(&text, cx)
        } else {
            MultiBuffer::build_random(&mut rng, cx)
        };
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        log::info!("Buffer text: {:?}", buffer_snapshot.text());

        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        log::info!("InlayMap text: {:?}", inlay_snapshot.text());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        fold_map.randomly_mutate(&mut rng);
        let (fold_snapshot, _) = fold_map.read(inlay_snapshot, vec![]);
        log::info!("FoldMap text: {:?}", fold_snapshot.text());
        let (inlay_snapshot, _) = inlay_map.randomly_mutate(&mut 0, &mut rng);
        log::info!("InlayMap text: {:?}", inlay_snapshot.text());

        let (mut tab_map, _) = TabMap::new(fold_snapshot, tab_size);
        let tabs_snapshot = tab_map.set_max_expansion_column(32);

        let text = text::Rope::from(tabs_snapshot.text().as_str());
        log::info!(
            "TabMap text (tab size: {}): {:?}",
            tab_size,
            tabs_snapshot.text(),
        );

        for _ in 0..5 {
            let end_row = rng.random_range(0..=text.max_point().row);
            let end_column = rng.random_range(0..=text.line_len(end_row));
            let mut end = TabPoint(text.clip_point(Point::new(end_row, end_column), Bias::Right));
            let start_row = rng.random_range(0..=text.max_point().row);
            let start_column = rng.random_range(0..=text.line_len(start_row));
            let mut start =
                TabPoint(text.clip_point(Point::new(start_row, start_column), Bias::Left));
            if start > end {
                mem::swap(&mut start, &mut end);
            }

            let expected_text = text
                .chunks_in_range(text.point_to_offset(start.0)..text.point_to_offset(end.0))
                .collect::<String>();
            let expected_summary = TextSummary::from(expected_text.as_str());
            assert_eq!(
                tabs_snapshot
                    .chunks(
                        start..end,
                        LanguageAwareStyling {
                            tree_sitter: false,
                            diagnostics: false,
                        },
                        Highlights::default()
                    )
                    .map(|c| c.text)
                    .collect::<String>(),
                expected_text,
                "chunks({:?}..{:?})",
                start,
                end
            );

            let mut actual_summary = tabs_snapshot.text_summary_for_range(start..end);
            if tab_size.get() > 1 && inlay_snapshot.text().contains('\t') {
                actual_summary.longest_row = expected_summary.longest_row;
                actual_summary.longest_row_chars = expected_summary.longest_row_chars;
            }
            assert_eq!(actual_summary, expected_summary);
        }

        for row in 0..=text.max_point().row {
            assert_eq!(
                tabs_snapshot.line_len(row),
                text.line_len(row),
                "line_len({row})"
            );
        }
    }

    #[gpui::test(iterations = 100)]
    fn test_to_tab_point_random(cx: &mut gpui::App, mut rng: StdRng) {
        let tab_size = NonZeroU32::new(rng.random_range(1..=16)).unwrap();
        let len = rng.random_range(0..=2000);

        // Generate random text using RandomCharIter
        let text = util::RandomCharIter::new(&mut rng)
            .take(len)
            .collect::<String>();

        // Create buffer and tab map
        let buffer = MultiBuffer::build_simple(&text, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (mut fold_map, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let (mut tab_map, _) = TabMap::new(fold_snapshot, tab_size);

        let mut next_inlay_id = 0;
        let (inlay_snapshot, inlay_edits) = inlay_map.randomly_mutate(&mut next_inlay_id, &mut rng);
        let (fold_snapshot, fold_edits) = fold_map.read(inlay_snapshot, inlay_edits);
        let max_fold_point = fold_snapshot.max_point();
        let (mut tab_snapshot, _) = tab_map.sync(fold_snapshot.clone(), fold_edits, tab_size);

        // Test random fold points
        for _ in 0..50 {
            tab_snapshot.max_expansion_column = rng.random_range(0..=256);
            // Generate random fold point
            let row = rng.random_range(0..=max_fold_point.row());
            let max_column = if row < max_fold_point.row() {
                fold_snapshot.line_len(row)
            } else {
                max_fold_point.column()
            };
            let column = rng.random_range(0..=max_column + 10);
            let fold_point = FoldPoint::new(row, column);

            let actual = tab_snapshot.fold_point_to_tab_point(fold_point);
            let expected = tab_snapshot.expected_to_tab_point(fold_point);

            assert_eq!(
                actual, expected,
                "to_tab_point mismatch for fold_point {:?} in text {:?}",
                fold_point, text
            );
        }
    }

    #[gpui::test]
    fn test_tab_stop_cursor_utf8(cx: &mut gpui::App) {
        let text = "\tfoo\tbarbarbar\t\tbaz\n";
        let buffer = MultiBuffer::build_simple(text, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let chunks = fold_snapshot.chunks(
            FoldOffset(MultiBufferOffset(0))..fold_snapshot.len(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Default::default(),
        );
        let mut cursor = TabStopCursor::new(chunks);
        assert!(cursor.seek_forward(0).is_none());
        let mut tab_stops = Vec::new();

        let mut all_tab_stops = Vec::new();
        let mut byte_offset = 0;
        for (offset, ch) in buffer.read(cx).snapshot(cx).text().char_indices() {
            byte_offset += ch.len_utf8() as u32;

            if ch == '\t' {
                all_tab_stops.push(TabStop {
                    byte_offset,
                    char_offset: offset as u32 + 1,
                });
            }
        }

        while let Some(tab_stop) = cursor.seek_forward(u32::MAX) {
            tab_stops.push(tab_stop);
        }
        pretty_assertions::assert_eq!(tab_stops.as_slice(), all_tab_stops.as_slice(),);

        assert_eq!(cursor.byte_offset(), byte_offset);
    }

    #[gpui::test]
    fn test_tab_stop_with_end_range_utf8(cx: &mut gpui::App) {
        let input = "A\tBC\t"; // DEF\tG\tHI\tJ\tK\tL\tM

        let buffer = MultiBuffer::build_simple(input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);

        let chunks = fold_snapshot.chunks_at(FoldPoint::new(0, 0));
        let mut cursor = TabStopCursor::new(chunks);

        let mut actual_tab_stops = Vec::new();

        let mut expected_tab_stops = Vec::new();
        let mut byte_offset = 0;
        for (offset, ch) in buffer.read(cx).snapshot(cx).text().char_indices() {
            byte_offset += ch.len_utf8() as u32;

            if ch == '\t' {
                expected_tab_stops.push(TabStop {
                    byte_offset,
                    char_offset: offset as u32 + 1,
                });
            }
        }

        while let Some(tab_stop) = cursor.seek_forward(u32::MAX) {
            actual_tab_stops.push(tab_stop);
        }
        pretty_assertions::assert_eq!(actual_tab_stops.as_slice(), expected_tab_stops.as_slice(),);

        assert_eq!(cursor.byte_offset(), byte_offset);
    }

    #[gpui::test(iterations = 100)]
    fn test_tab_stop_cursor_random_utf8(cx: &mut gpui::App, mut rng: StdRng) {
        // Generate random input string with up to 512 characters including tabs
        let len = rng.random_range(0..=2048);
        let mut input = String::with_capacity(len);

        let mut skip_tabs = rng.random_bool(0.10);
        for idx in 0..len {
            if idx % 128 == 0 {
                skip_tabs = rng.random_bool(0.10);
            }

            if rng.random_bool(0.15) && !skip_tabs {
                input.push('\t');
            } else {
                let ch = loop {
                    let ascii_code = rng.random_range(32..=126); // printable ASCII range
                    let ch = ascii_code as u8 as char;
                    if ch != '\t' {
                        break ch;
                    }
                };
                input.push(ch);
            }
        }

        // Build the buffer and create cursor
        let buffer = MultiBuffer::build_simple(&input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);

        // First, collect all expected tab positions
        let mut all_tab_stops = Vec::new();
        let mut byte_offset = 1;
        let mut char_offset = 1;
        #[expect(
            clippy::explicit_counter_loop,
            reason = "Lint does not account for char_offset being needed after the loop"
        )]
        for ch in buffer_snapshot.text().chars() {
            if ch == '\t' {
                all_tab_stops.push(TabStop {
                    byte_offset,
                    char_offset,
                });
            }
            byte_offset += ch.len_utf8() as u32;
            char_offset += 1;
        }

        // Test with various distances
        let distances = vec![1, 5, 10, 50, 100, u32::MAX];
        // let distances = vec![150];

        for distance in distances {
            let chunks = fold_snapshot.chunks_at(FoldPoint::new(0, 0));
            let mut cursor = TabStopCursor::new(chunks);

            let mut found_tab_stops = Vec::new();
            let mut position = distance;
            while let Some(tab_stop) = cursor.seek_forward(position) {
                found_tab_stops.push(tab_stop);
                position = distance - tab_stop.byte_offset;
            }

            let expected_found_tab_stops: Vec<_> = all_tab_stops
                .iter()
                .take_while(|tab_stop| tab_stop.byte_offset <= distance)
                .cloned()
                .collect();

            pretty_assertions::assert_eq!(
                found_tab_stops,
                expected_found_tab_stops,
                "TabStopCursor output mismatch for distance {}. Input: {:?}",
                distance,
                input
            );

            let final_position = cursor.byte_offset();
            if !found_tab_stops.is_empty() {
                let last_tab_stop = found_tab_stops.last().unwrap();
                assert!(
                    final_position >= last_tab_stop.byte_offset,
                    "Cursor final position {} is before last tab stop {}. Input: {:?}",
                    final_position,
                    last_tab_stop.byte_offset,
                    input
                );
            }
        }
    }

    #[gpui::test]
    fn test_tab_stop_cursor_utf16(cx: &mut gpui::App) {
        let text = "\r\t😁foo\tb😀arbar🤯bar\t\tbaz\n";
        let buffer = MultiBuffer::build_simple(text, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let chunks = fold_snapshot.chunks(
            FoldOffset(MultiBufferOffset(0))..fold_snapshot.len(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Default::default(),
        );
        let mut cursor = TabStopCursor::new(chunks);
        assert!(cursor.seek_forward(0).is_none());

        let mut expected_tab_stops = Vec::new();
        let mut byte_offset = 0;
        for (i, ch) in fold_snapshot.chars_at(FoldPoint::new(0, 0)).enumerate() {
            byte_offset += ch.len_utf8() as u32;

            if ch == '\t' {
                expected_tab_stops.push(TabStop {
                    byte_offset,
                    char_offset: i as u32 + 1,
                });
            }
        }

        let mut actual_tab_stops = Vec::new();
        while let Some(tab_stop) = cursor.seek_forward(u32::MAX) {
            actual_tab_stops.push(tab_stop);
        }

        pretty_assertions::assert_eq!(actual_tab_stops.as_slice(), expected_tab_stops.as_slice(),);

        assert_eq!(cursor.byte_offset(), byte_offset);
    }

    #[gpui::test(iterations = 100)]
    fn test_tab_stop_cursor_random_utf16(cx: &mut gpui::App, mut rng: StdRng) {
        // Generate random input string with up to 512 characters including tabs
        let len = rng.random_range(0..=2048);
        let input = util::RandomCharIter::new(&mut rng)
            .take(len)
            .collect::<String>();

        // Build the buffer and create cursor
        let buffer = MultiBuffer::build_simple(&input, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);

        // First, collect all expected tab positions
        let mut all_tab_stops = Vec::new();
        let mut byte_offset = 0;
        for (i, ch) in buffer_snapshot.text().chars().enumerate() {
            byte_offset += ch.len_utf8() as u32;
            if ch == '\t' {
                all_tab_stops.push(TabStop {
                    byte_offset,
                    char_offset: i as u32 + 1,
                });
            }
        }

        // Test with various distances
        // let distances = vec![1, 5, 10, 50, 100, u32::MAX];
        let distances = vec![150];

        for distance in distances {
            let chunks = fold_snapshot.chunks_at(FoldPoint::new(0, 0));
            let mut cursor = TabStopCursor::new(chunks);

            let mut found_tab_stops = Vec::new();
            let mut position = distance;
            while let Some(tab_stop) = cursor.seek_forward(position) {
                found_tab_stops.push(tab_stop);
                position = distance - tab_stop.byte_offset;
            }

            let expected_found_tab_stops: Vec<_> = all_tab_stops
                .iter()
                .take_while(|tab_stop| tab_stop.byte_offset <= distance)
                .cloned()
                .collect();

            pretty_assertions::assert_eq!(
                found_tab_stops,
                expected_found_tab_stops,
                "TabStopCursor output mismatch for distance {}. Input: {:?}",
                distance,
                input
            );

            let final_position = cursor.byte_offset();
            if !found_tab_stops.is_empty() {
                let last_tab_stop = found_tab_stops.last().unwrap();
                assert!(
                    final_position >= last_tab_stop.byte_offset,
                    "Cursor final position {} is before last tab stop {}. Input: {:?}",
                    final_position,
                    last_tab_stop.byte_offset,
                    input
                );
            }
        }
    }
}
