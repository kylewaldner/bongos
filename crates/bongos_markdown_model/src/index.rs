use std::ops::Range;

use smallvec::SmallVec;

/// Opening and closing fence lines of a fenced code block. The closing fence
/// is `None` when the block is unterminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeFences {
    pub open: Range<usize>,
    pub close: Option<Range<usize>>,
}

/// One line of a table. The delimiter row (`|---|---|`) is included, with
/// `is_delimiter` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub range: Range<usize>,
    pub cells: Vec<Range<usize>>,
    pub is_header: bool,
    pub is_delimiter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementKind {
    /// Level 1 to 6 (ATX headings only).
    Heading(u8),
    Strong,
    Emphasis,
    Strike,
    Code,
    Link {
        url: String,
    },
    Image {
        alt: String,
        url: String,
    },
    Autolink,
    ListItem {
        /// The marker text itself (`-`, `*`, `+`, `1.`, `2)`).
        marker: Range<usize>,
        task: Option<bool>,
    },
    Quote,
    Rule,
    /// Fenced code blocks only.
    CodeBlock {
        fences: CodeFences,
        lang: Option<String>,
    },
    /// The element's `markup` is the delimiter row.
    Table {
        rows: Vec<TableRow>,
    },
    Paragraph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub kind: ElementKind,
    pub range: Range<usize>,
    /// Byte ranges to hide when concealing; each is inside `range`, on char
    /// boundaries, and contains no newline.
    pub markup: SmallVec<[Range<usize>; 4]>,
    /// Markup that is revealed per line rather than per element.
    pub line_level: bool,
}

/// Elements sorted by range start (outer before inner on ties).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownIndex {
    elements: Vec<Element>,
    /// The largest range end among the elements up to each one, so a lookup skips every element
    /// that ends before it with a binary search instead of a walk from the start.
    max_end: Vec<usize>,
}

impl MarkdownIndex {
    /// An index of `elements`, which must be sorted by range start.
    pub(crate) fn from_sorted(elements: Vec<Element>) -> Self {
        let mut max_end = Vec::with_capacity(elements.len());
        let mut largest = 0;
        for element in &elements {
            largest = largest.max(element.range.end);
            max_end.push(largest);
        }
        Self { elements, max_end }
    }

    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// The position of the first element that may end at or after `offset`; every element before
    /// it ends before `offset`.
    fn first_ending_at_or_after(&self, offset: usize) -> usize {
        self.max_end.partition_point(|end| *end < offset)
    }

    /// Elements whose range intersects `range`.
    pub fn elements_in(&self, range: Range<usize>) -> impl Iterator<Item = &Element> {
        let first = self.first_ending_at_or_after(range.start);
        self.elements[first..]
            .iter()
            .take_while(move |element| element.range.start <= range.end)
            .filter(move |element| intersects(element, &range))
    }

    /// Elements whose range, ends included, meets `range`, ends included.
    pub fn elements_touching(&self, range: Range<usize>) -> impl Iterator<Item = &Element> {
        let first = self.first_ending_at_or_after(range.start);
        self.elements[first..]
            .iter()
            .take_while(move |element| element.range.start <= range.end)
            .filter(move |element| element.range.end >= range.start)
    }

    /// The same index with every offset moved: each range's start through `start` and its end
    /// through `end`. Both must be monotonic, so the order of the elements is kept; a range whose
    /// mapped end falls before its mapped start becomes empty at the start.
    pub fn map_offsets(
        &self,
        start: impl Fn(usize) -> usize,
        end: impl Fn(usize) -> usize,
    ) -> MarkdownIndex {
        map_elements(self.elements.iter(), start, end)
    }

    /// As [`Self::map_offsets`], keeping only the elements that intersect one of `ranges` (as
    /// [`Self::elements_in`] tells), so the cost follows what the ranges hold rather than the
    /// size of the whole index.
    pub fn map_offsets_in(
        &self,
        ranges: &[Range<usize>],
        start: impl Fn(usize) -> usize,
        end: impl Fn(usize) -> usize,
    ) -> MarkdownIndex {
        let mut picked: Vec<usize> = Vec::new();
        for range in ranges {
            let first = self.first_ending_at_or_after(range.start);
            picked.extend(
                self.elements
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take_while(|(_, element)| element.range.start <= range.end)
                    .filter(|(_, element)| intersects(element, range))
                    .map(|(i, _)| i),
            );
        }
        picked.sort_unstable();
        picked.dedup();
        map_elements(picked.into_iter().map(|i| &self.elements[i]), start, end)
    }

    /// The innermost element whose range contains `offset`, ends inclusive.
    pub fn element_at(&self, offset: usize) -> Option<&Element> {
        let mut best: Option<&Element> = None;
        for element in &self.elements {
            if element.range.start > offset {
                break;
            }
            if element.range.end < offset {
                continue;
            }
            let better = match best {
                Some(current) => element.range.len() <= current.range.len(),
                None => true,
            };
            if better {
                best = Some(element);
            }
        }
        best
    }
}

fn intersects(element: &Element, range: &Range<usize>) -> bool {
    if range.is_empty() {
        element.range.start <= range.start && range.start <= element.range.end
    } else {
        element.range.start < range.end && element.range.end > range.start
    }
}

fn map_elements<'a>(
    elements: impl Iterator<Item = &'a Element>,
    start: impl Fn(usize) -> usize,
    end: impl Fn(usize) -> usize,
) -> MarkdownIndex {
    let range = |range: &Range<usize>| {
        let new_start = start(range.start);
        new_start..end(range.end).max(new_start)
    };
    let elements = elements
        .map(|element| {
            let kind = match &element.kind {
                ElementKind::ListItem { marker, task } => ElementKind::ListItem {
                    marker: range(marker),
                    task: *task,
                },
                ElementKind::CodeBlock { fences, lang } => ElementKind::CodeBlock {
                    fences: CodeFences {
                        open: range(&fences.open),
                        close: fences.close.as_ref().map(&range),
                    },
                    lang: lang.clone(),
                },
                ElementKind::Table { rows } => ElementKind::Table {
                    rows: rows
                        .iter()
                        .map(|row| TableRow {
                            range: range(&row.range),
                            cells: row.cells.iter().map(&range).collect(),
                            is_header: row.is_header,
                            is_delimiter: row.is_delimiter,
                        })
                        .collect(),
                },
                kind => kind.clone(),
            };
            Element {
                kind,
                range: range(&element.range),
                markup: element.markup.iter().map(&range).collect(),
                line_level: element.line_level,
            }
        })
        .collect();
    MarkdownIndex::from_sorted(elements)
}
