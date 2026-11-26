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
    pub(crate) elements: Vec<Element>,
}

impl MarkdownIndex {
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// Elements whose range intersects `range`.
    pub fn elements_in(&self, range: Range<usize>) -> impl Iterator<Item = &Element> {
        self.elements
            .iter()
            .take_while(move |element| element.range.start <= range.end)
            .filter(move |element| {
                if range.is_empty() {
                    element.range.start <= range.start && range.start <= element.range.end
                } else {
                    element.range.start < range.end && element.range.end > range.start
                }
            })
    }

    /// The same index with every offset moved: each range's start through `start` and its end
    /// through `end`. Both must be monotonic, so the order of the elements is kept; a range whose
    /// mapped end falls before its mapped start becomes empty at the start.
    pub fn map_offsets(
        &self,
        start: impl Fn(usize) -> usize,
        end: impl Fn(usize) -> usize,
    ) -> MarkdownIndex {
        let range = |range: &Range<usize>| {
            let new_start = start(range.start);
            new_start..end(range.end).max(new_start)
        };
        let elements = self
            .elements
            .iter()
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
        MarkdownIndex { elements }
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
