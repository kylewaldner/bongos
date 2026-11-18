//! Display-column geometry for rectangle commands. A row is one buffer line
//! (text without its newline); columns are display columns, where a tab runs to
//! the next multiple of the tab size and other characters take their Unicode
//! width.

use unicode_width::UnicodeWidthChar;

/// One character of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Byte offset of the character in the line.
    pub byte: usize,
    /// Display column where the character starts.
    pub start_col: usize,
    /// Display columns the character covers.
    pub width: usize,
}

/// Where a display column falls in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locate {
    /// The byte offset of the first cell that starts exactly at the column, or
    /// the line length when the column equals the line width. A zero-width
    /// character starts at the end column of its base character, so at that
    /// column the zero-width character comes first.
    At(usize),
    /// The column is strictly inside a tab: the tab's byte offset, the tab
    /// columns left of the column, and the tab columns right of it.
    InsideTab {
        byte: usize,
        before: usize,
        after: usize,
    },
    /// The column is strictly inside a character at least two columns wide.
    InsideWide { byte: usize },
    /// The column is beyond the end of the line by this many columns.
    PastEnd { short_by: usize },
}

/// The left and right display columns of a rectangle (right is exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    pub left: usize,
    pub right: usize,
}

impl Columns {
    /// Columns spanned by two corner columns given in either order.
    pub fn from_corners(a: usize, b: usize) -> Columns {
        Columns {
            left: a.min(b),
            right: a.max(b),
        }
    }

    /// Number of columns covered.
    pub fn width(&self) -> usize {
        self.right - self.left
    }

    /// True when the rectangle covers no columns.
    pub fn is_empty(&self) -> bool {
        self.left == self.right
    }
}

/// One cell per character of `line`, with tabs sized by `tab` (at least 1).
pub fn cells(line: &str, tab: usize) -> Vec<Cell> {
    let tab = tab.max(1);
    let mut col = 0;
    line.char_indices()
        .map(|(byte, ch)| {
            let width = if ch == '\t' {
                tab - col % tab
            } else {
                ch.width().unwrap_or(1)
            };
            let cell = Cell {
                byte,
                start_col: col,
                width,
            };
            col += width;
            cell
        })
        .collect()
}

/// Total display width of `line`.
pub fn line_width(line: &str, tab: usize) -> usize {
    cells(line, tab)
        .last()
        .map_or(0, |cell| cell.start_col + cell.width)
}

/// Finds display column `col` in `line`.
pub fn locate(line: &str, col: usize, tab: usize) -> Locate {
    let all = cells(line, tab);
    if let Some(cell) = all.iter().find(|cell| cell.start_col == col) {
        return Locate::At(cell.byte);
    }
    let total = all.last().map_or(0, |cell| cell.start_col + cell.width);
    if col == total {
        return Locate::At(line.len());
    }
    if col > total {
        return Locate::PastEnd {
            short_by: col - total,
        };
    }
    let cell = all
        .iter()
        .find(|cell| cell.start_col < col && col < cell.start_col + cell.width)
        .expect("a column inside the line is inside some cell");
    if line[cell.byte..].starts_with('\t') {
        Locate::InsideTab {
            byte: cell.byte,
            before: col - cell.start_col,
            after: cell.start_col + cell.width - col,
        }
    } else {
        Locate::InsideWide { byte: cell.byte }
    }
}
