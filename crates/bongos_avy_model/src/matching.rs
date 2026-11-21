use std::ops::Range;

fn chars_match(wanted: char, actual: char) -> bool {
    if wanted.is_lowercase() {
        actual == wanted || actual.to_lowercase().eq(wanted.to_lowercase())
    } else {
        actual == wanted
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte ranges of every occurrence of `query` in `text`, overlapping ones
/// included. A lowercase query letter matches both cases; an uppercase letter
/// or any non-letter matches only itself.
pub fn char_matches(query: &str, text: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (start, _) in text.char_indices() {
        let mut rest = text[start..].char_indices();
        let mut end = start;
        let mut all = true;
        for wanted in query.chars() {
            match rest.next() {
                Some((offset, actual)) if chars_match(wanted, actual) => {
                    end = start + offset + actual.len_utf8();
                }
                _ => {
                    all = false;
                    break;
                }
            }
        }
        if all {
            found.push(start..end);
        }
    }
    found
}

/// Byte offsets of word starts in `text` that match `ch`. `before` is the
/// character preceding `text`; `None` counts the start of `text` as a word
/// start.
pub fn word_starts(ch: char, text: &str, before: Option<char>) -> Vec<usize> {
    let mut previous = before;
    let mut starts = Vec::new();
    for (offset, actual) in text.char_indices() {
        let at_boundary = previous.is_none_or(|p| !is_word_char(p));
        if at_boundary && chars_match(ch, actual) {
            starts.push(offset);
        }
        previous = Some(actual);
    }
    starts
}
