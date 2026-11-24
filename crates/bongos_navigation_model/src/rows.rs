//! Result rows: snippets trimmed around a match, pane names and find rows.

use std::ops::Range;

/// The ellipsis added to a trimmed side of a snippet.
pub const ELLIPSIS: char = '…';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The trimmed line. A side that was cut starts or ends with [`ELLIPSIS`].
    pub text: String,
    /// Match ranges as byte offsets into `text`, shifted for the trimming and the ellipsis.
    pub ranges: Vec<Range<usize>>,
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Trims `line` to at most about `max_chars` characters around `match_range` (byte offsets). The
/// match stays whole when it fits; the room left is split evenly before and after it.
pub fn snippet(line: &str, match_range: Range<usize>, max_chars: usize) -> Snippet {
    let start = floor_boundary(line, match_range.start);
    let end = ceil_boundary(line, match_range.end).max(start);
    if line.chars().count() <= max_chars {
        return Snippet {
            text: line.to_string(),
            ranges: vec![start..end],
        };
    }
    let match_chars = line[start..end].chars().count();
    let room = max_chars.saturating_sub(match_chars) / 2;
    let before: Vec<usize> = line[..start].char_indices().map(|(i, _)| i).collect();
    let from = if room == 0 {
        start
    } else if before.len() > room {
        before[before.len() - room]
    } else {
        0
    };
    let mut to = end;
    for (taken, (i, c)) in line[end..].char_indices().enumerate() {
        if taken >= room {
            break;
        }
        to = end + i + c.len_utf8();
    }
    let mut text = String::new();
    let mut shift = from;
    if from > 0 {
        text.push(ELLIPSIS);
        shift = from.saturating_sub(ELLIPSIS.len_utf8());
    }
    text.push_str(&line[from..to]);
    if to < line.len() {
        text.push(ELLIPSIS);
    }
    Snippet {
        text,
        ranges: vec![start - shift..end - shift],
    }
}

/// Where a pane sits, for naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePosition {
    /// Index among the panes of the center tree, and how the tree splits.
    Center {
        horizontal_index: usize,
        horizontal_count: usize,
        vertical_index: usize,
        vertical_count: usize,
    },
    Terminal,
}

/// `left`/`right` by horizontal position, `top`/`bottom` by vertical position (horizontal wins in
/// a grid), `terminal` for the terminal dock, and `main` for a lone pane.
pub fn pane_name(position: PanePosition) -> &'static str {
    match position {
        PanePosition::Terminal => "dock: terminal",
        PanePosition::Center {
            horizontal_index,
            horizontal_count,
            vertical_index,
            vertical_count,
        } => {
            if horizontal_count > 1 {
                if horizontal_index == 0 {
                    "left"
                } else {
                    "right"
                }
            } else if vertical_count > 1 {
                if vertical_index == 0 { "top" } else { "bottom" }
            } else {
                "main"
            }
        }
    }
}

/// `path:line  snippet`, with two spaces before the snippet.
pub fn find_row(path: &str, line: u32, snippet: &str) -> String {
    format!("{path}:{line}  {snippet}")
}

#[cfg(test)]
#[path = "rows_tests.rs"]
mod tests;
