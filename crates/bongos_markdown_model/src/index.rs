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
