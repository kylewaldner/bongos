/// What the goto-line prompt's text asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GotoQuery {
    /// Nothing typed yet.
    Empty,
    /// A 1-based line, and a 1-based column when `line:column` was typed. The
    /// numbers are as typed (0 included); callers clamp them to the buffer.
    Position { line: usize, column: Option<usize> },
    /// Not a line number.
    Invalid,
}

fn number(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse().unwrap_or(usize::MAX))
}

/// Parses goto-line input: `42`, `42:7` (a trailing `:` is allowed), blank, or
/// anything else, which is invalid.
pub fn parse(input: &str) -> GotoQuery {
    let input = input.trim();
    if input.is_empty() {
        return GotoQuery::Empty;
    }
    let (line, column) = match input.split_once(':') {
        Some((line, column)) => (line.trim(), column.trim()),
        None => (input, ""),
    };
    let Some(line) = number(line) else {
        return GotoQuery::Invalid;
    };
    if column.is_empty() {
        return GotoQuery::Position { line, column: None };
    }
    match number(column) {
        Some(column) => GotoQuery::Position {
            line,
            column: Some(column),
        },
        None => GotoQuery::Invalid,
    }
}
