use std::ops::Range;

use crate::index::ElementKind;
use crate::parse::parse;

/// One replacement of `range` (old-text byte offsets) by `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub new_text: String,
}

/// The start and end (before the newline) of the line holding `offset`. `offset` is clamped to
/// the text length and moved back to a char boundary.
fn line_bounds(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    (start, end)
}

/// D19: flips the checkbox of the task list item whose first line holds `offset`. The edit
/// replaces just the three bytes of `[ ]` or `[x]`. `None` when the line is not the first line
/// of a task item (a plain line, a list item without a checkbox, a continuation line).
pub fn toggle_checkbox(text: &str, offset: usize) -> Option<Edit> {
    let (line_start, line_end) = line_bounds(text, offset);
    let index = parse(text);
    for element in index.elements() {
        let ElementKind::ListItem {
            marker,
            task: Some(checked),
        } = &element.kind
        else {
            continue;
        };
        if marker.start < line_start || marker.start > line_end {
            continue;
        }
        let bytes = text.as_bytes();
        let mut start = marker.end;
        while start < line_end && matches!(bytes[start], b' ' | b'\t') {
            start += 1;
        }
        if start + 3 > line_end || bytes[start] != b'[' || bytes[start + 2] != b']' {
            continue;
        }
        return Some(Edit {
            range: start..start + 3,
            new_text: if *checked { "[ ]" } else { "[x]" }.to_string(),
        });
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading,
    Bullet,
    Checkbox,
    CodeBlock,
    Table,
    Quote,
    Divider,
}

impl BlockKind {
    /// The labels accepted by `from_label`, in the order of the variants.
    pub const LABELS: [&'static str; 7] = [
        "heading",
        "bullet",
        "checkbox",
        "code block",
        "table",
        "quote",
        "divider",
    ];

    pub fn from_label(label: &str) -> Option<BlockKind> {
        Some(match label {
            "heading" => BlockKind::Heading,
            "bullet" => BlockKind::Bullet,
            "checkbox" => BlockKind::Checkbox,
            "code block" => BlockKind::CodeBlock,
            "table" => BlockKind::Table,
            "quote" => BlockKind::Quote,
            "divider" => BlockKind::Divider,
            _ => return None,
        })
    }

    /// The template and the point's offset inside it.
    fn template(self) -> (&'static str, usize) {
        match self {
            BlockKind::Heading => ("# ", 2),
            BlockKind::Bullet => ("- ", 2),
            BlockKind::Checkbox => ("- [ ] ", 6),
            BlockKind::CodeBlock => ("```\n\n```", 4),
            BlockKind::Table => (
                "| Column 1 | Column 2 |\n| --- | --- |\n|  |  |",
                "| Column 1 | Column 2 |\n| --- | --- |\n| ".len(),
            ),
            BlockKind::Quote => ("> ", 2),
            BlockKind::Divider => ("---", 3),
        }
    }

    /// Blocks that must be set off by blank lines (a `---` after a paragraph would otherwise
    /// make a setext heading).
    fn needs_blank_lines(self) -> bool {
        matches!(
            self,
            BlockKind::CodeBlock | BlockKind::Table | BlockKind::Divider
        )
    }
}

pub const LABELS: [&str; 7] = BlockKind::LABELS;

/// D18: inserts a block template at `offset`. When the line holding `offset` is blank (empty or
/// only whitespace) the template replaces that line; otherwise it goes on a new line after it.
/// Code blocks, tables and dividers get a blank line before (when the current line is not blank)
/// and after (when the next line exists and is not blank).
///
/// Returns the edit (its range in old-text offsets) and the point's byte offset in the text
/// after the edit is applied. An `offset` past the end of the text clamps to the end, and one
/// inside a char moves back to its start. At the end of a text without a trailing newline the
/// last line is the current line.
pub fn insert_block(text: &str, offset: usize, kind: BlockKind) -> (Edit, usize) {
    let (line_start, line_end) = line_bounds(text, offset);
    let blank = text[line_start..line_end].trim().is_empty();
    let next_non_blank = line_end < text.len() && {
        let (next_start, next_end) = line_bounds(text, line_end + 1);
        !text[next_start..next_end].trim().is_empty()
    };
    let (template, point) = kind.template();
    let spaced = kind.needs_blank_lines();

    let mut new_text = String::new();
    let range = if blank {
        line_start..line_end
    } else {
        new_text.push('\n');
        if spaced {
            new_text.push('\n');
        }
        line_end..line_end
    };
    let point_offset = range.start + new_text.len() + point;
    new_text.push_str(template);
    if spaced && next_non_blank {
        new_text.push('\n');
    }
    (Edit { range, new_text }, point_offset)
}
