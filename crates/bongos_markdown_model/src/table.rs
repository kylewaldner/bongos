use crate::index::{ElementKind, MarkdownIndex, TableRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellDirection {
    Next,
    Previous,
}

/// The byte offset where the text of the cell `cell` of `row` starts: the first non-space byte
/// of the cell, or, for an empty cell, the byte after the pipe and one space.
fn cell_start(text: &str, row: &TableRow, cell: usize) -> usize {
    let range = &row.cells[cell];
    if range.start < range.end {
        return range.start;
    }
    // An empty cell: find it from the pipes of the row. Cell `n` starts after the n-th
    // unescaped pipe (the leading one, when present, is the first).
    let bytes = text.as_bytes();
    let row_end = row.range.end.min(text.len());
    let mut pipes = Vec::new();
    let mut i = row.range.start;
    while i < row_end {
        match bytes[i] {
            b'\\' => i += 1,
            b'|' => pipes.push(i),
            _ => {}
        }
        i += 1;
    }
    let leading = bytes
        .get(row.range.start..row_end)
        .is_some_and(|line| line.trim_ascii_start().starts_with(b"|"));
    let pipe = if leading { cell } else { cell.wrapping_sub(1) };
    let Some(&pipe) = pipes.get(pipe) else {
        return range.start.min(row_end);
    };
    let mut start = pipe + 1;
    if bytes.get(start) == Some(&b' ') && start < row_end {
        start += 1;
    }
    start.min(row_end)
}

/// D20: the start of the text of the next or previous cell of the table row holding `offset`,
/// skipping the delimiter row and wrapping across rows. The current cell is the one holding
/// `offset` or the closest one before it; an `offset` before the first cell of its row counts as
/// before that cell. `None` past the last cell (`Next`), before the first cell (`Previous`), or
/// when `offset` is not on a table row.
pub fn table_cell(
    text: &str,
    index: &MarkdownIndex,
    offset: usize,
    direction: CellDirection,
) -> Option<usize> {
    let rows = index
        .elements()
        .iter()
        .find_map(|element| match &element.kind {
            ElementKind::Table { rows }
                if element.range.start <= offset && offset <= element.range.end =>
            {
                Some(rows)
            }
            _ => None,
        })?;
    let row = rows.iter().position(|row| {
        !row.is_delimiter && row.range.start <= offset && offset <= row.range.end
    })?;
    let current = (0..rows[row].cells.len())
        .rev()
        .find(|&i| cell_start(text, &rows[row], i) <= offset);
    // Every (row, cell) position of the table in reading order, without the delimiter row.
    let positions: Vec<(usize, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.is_delimiter)
        .flat_map(|(r, row)| (0..row.cells.len()).map(move |c| (r, c)))
        .collect();
    let at = current.and_then(|c| positions.iter().position(|&p| p == (row, c)));
    let target = match (direction, at) {
        (CellDirection::Next, Some(at)) => *positions.get(at + 1)?,
        (CellDirection::Next, None) => *positions.iter().find(|&&(r, _)| r == row)?,
        (CellDirection::Previous, Some(at)) => *positions.get(at.checked_sub(1)?)?,
        (CellDirection::Previous, None) => {
            let first = positions.iter().position(|&(r, _)| r == row)?;
            *positions.get(first.checked_sub(1)?)?
        }
    };
    Some(cell_start(text, &rows[target.0], target.1))
}
