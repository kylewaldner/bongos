//! Reveal at point: which markup shows as typed around the selections.
//!
//! What a caller hides for any set of selections is `plan(index, text,
//! window, &revealed(index, text, selections), image_ok)`. The rules:
//!
//! - An inline element (strong, emphasis, strike, code, link, autolink,
//!   image) whose range, ends included, holds a selection head shows all of
//!   its markup.
//! - A line-level element (heading marker, list marker and task box, quote
//!   marker, rule) shows only the markup on a line holding a head, so a
//!   heading reveals `# ` on the head's line and not on the next line.
//! - A fenced code block shows both fence lines, and a table its delimiter
//!   row, while a head is anywhere inside it, ends included.
//! - Every element a non-empty selection intersects shows all of its markup.
//!
//! Each selection reveals its own elements and the results merge.

use std::ops::Range;

use crate::{Element, ElementKind, MarkdownIndex};

/// A selection as the reveal rules see it: the selected byte range plus the
/// head, the end the cursor sits at. A plain cursor has an empty `range` at
/// `head`. Offsets past the end of the text count as the end of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevealSelection {
    pub range: Range<usize>,
    pub head: usize,
}

impl RevealSelection {
    /// An empty selection (a cursor) at `offset`.
    pub fn cursor(offset: usize) -> Self {
        Self {
            range: offset..offset,
            head: offset,
        }
    }

    /// A selection over `range` whose head is `head`.
    pub fn new(range: Range<usize>, head: usize) -> Self {
        Self { range, head }
    }
}

/// The markup that shows as typed: sorted, merged byte ranges of revealed
/// markup, plus the sorted heads (no conceal may contain one). Two sets
/// built from the same index and selections are equal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevealSet {
    markup: Vec<Range<usize>>,
    heads: Vec<usize>,
}

impl RevealSet {
    /// Sorted, non-overlapping, non-touching byte ranges of revealed markup.
    pub fn markup(&self) -> &[Range<usize>] {
        &self.markup
    }

    /// Sorted, deduplicated selection heads.
    pub fn heads(&self) -> &[usize] {
        &self.heads
    }

    /// Whether any byte of `range` is revealed markup.
    pub fn is_revealed(&self, range: &Range<usize>) -> bool {
        let i = self.markup.partition_point(|m| m.end <= range.start);
        self.markup
            .get(i)
            .is_some_and(|m| m.start < range.end && range.start < range.end)
    }

    /// Whether a head lies in `range`, ends included.
    pub fn has_head_within(&self, range: &Range<usize>) -> bool {
        let i = self.heads.partition_point(|h| *h < range.start);
        self.heads.get(i).is_some_and(|h| *h <= range.end)
    }

    /// Whether a head lies strictly inside `range`, ends excluded.
    pub fn has_head_strictly_inside(&self, range: &Range<usize>) -> bool {
        let i = self.heads.partition_point(|h| *h <= range.start);
        self.heads.get(i).is_some_and(|h| *h < range.end)
    }
}

/// The markup revealed by `selections` (see the module docs for the rules).
pub fn revealed(index: &MarkdownIndex, text: &str, selections: &[RevealSelection]) -> RevealSet {
    let mut markup: Vec<Range<usize>> = Vec::new();
    let mut heads = Vec::with_capacity(selections.len());
    for selection in selections {
        let head = selection.head.min(text.len());
        heads.push(head);
        let line = line_around(text, head);
        for element in index.elements() {
            if element.range.start > line.end {
                break;
            }
            if element.range.end < line.start {
                continue;
            }
            reveal_at_head(element, head, &line, &mut markup);
        }
        let range = selection.range.start.min(text.len())..selection.range.end.min(text.len());
        if !range.is_empty() {
            for element in index.elements_in(range) {
                markup.extend(element.markup.iter().cloned());
            }
        }
    }
    markup.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(markup.len());
    for m in markup {
        match merged.last_mut() {
            Some(last) if m.start <= last.end => last.end = last.end.max(m.end),
            _ => merged.push(m),
        }
    }
    heads.sort_unstable();
    heads.dedup();
    RevealSet {
        markup: merged,
        heads,
    }
}

fn reveal_at_head(
    element: &Element,
    head: usize,
    line: &Range<usize>,
    out: &mut Vec<Range<usize>>,
) {
    let holds_head = element.range.start <= head && head <= element.range.end;
    let whole = match element.kind {
        ElementKind::CodeBlock { .. } | ElementKind::Table { .. } => true,
        _ => !element.line_level,
    };
    if whole {
        if holds_head {
            out.extend(element.markup.iter().cloned());
        }
    } else {
        out.extend(
            element
                .markup
                .iter()
                .filter(|m| line.start <= m.start && m.end <= line.end)
                .cloned(),
        );
    }
}

/// The line holding `offset`, between newlines, without them.
fn line_around(text: &str, offset: usize) -> Range<usize> {
    let bytes = text.as_bytes();
    let start = bytes[..offset]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let end = bytes[offset..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(text.len(), |i| offset + i);
    start..end
}
