use std::ops::Range;

use crate::lines::touched_lines;
use crate::text::{TextSource, line_start};

fn is_horizontal(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Whether a region covers whole lines for comment-dwim: it starts at or
/// before the first non-blank character of its first line and ends at or after
/// the last non-blank character of its last line (a region that ends right
/// after a newline ends on the line before). An empty region (no region) counts
/// as covering its line.
pub fn covers_whole_lines<T: TextSource + ?Sized>(text: &T, region: Range<usize>) -> bool {
    if region.is_empty() {
        return true;
    }
    let first = line_start(text, region.start);
    let first_content = first
        + text
            .chars_at(first)
            .take_while(|&c| is_horizontal(c))
            .map(char::len_utf8)
            .sum::<usize>();
    let last_end = touched_lines(text, &region).end;
    let last_content = last_end
        - text
            .reversed_chars_at(last_end)
            .take_while(|&c| is_horizontal(c))
            .map(char::len_utf8)
            .sum::<usize>();
    region.start <= first_content && region.end >= last_content
}
