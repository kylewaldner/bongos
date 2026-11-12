use crate::spec::Direction;
use std::ops::Range;

pub const FIRST_WINDOW: usize = 64 * 1024;
pub const MAX_WINDOW: usize = 4 * 1024 * 1024;
pub const OVERLAP: usize = 4 * 1024;

/// Windows the engine walks, nearest first: `[from, len)` forward or
/// `[0, from)` backward, split once with every inner boundary on a line start.
/// Sizes start at 64 KB and double to 4 MB. `from` is clamped to `len`, so
/// `usize::MAX` means the end. `line_start(offset)` is the start of the line
/// containing `offset`. A boundary inside a line longer than the window is
/// skipped, so a window grows to take the whole line.
pub fn plan(
    from: usize,
    len: usize,
    direction: Direction,
    line_start: impl Fn(usize) -> usize,
) -> Vec<Range<usize>> {
    let from = from.min(len);
    let mut windows = Vec::new();
    let mut size = FIRST_WINDOW;
    match direction {
        Direction::Forward => {
            let mut start = from;
            while start < len {
                let mut end = start.saturating_add(size);
                size = (size * 2).min(MAX_WINDOW);
                while end < len && line_start(end) <= start {
                    end = end.saturating_add(size);
                }
                let end = if end >= len { len } else { line_start(end) };
                windows.push(start..end);
                start = end;
            }
        }
        Direction::Backward => {
            let mut end = from;
            while end > 0 {
                let start = if end <= size {
                    0
                } else {
                    line_start(end - size)
                };
                size = (size * 2).min(MAX_WINDOW);
                windows.push(start..end);
                end = start;
            }
        }
    }
    windows
}

/// A window extended by the overlap past its end, for patterns with a
/// newline, which search a window as one string.
pub fn overlapped(window: &Range<usize>, len: usize) -> Range<usize> {
    window.start..window.end.saturating_add(OVERLAP).min(len)
}
