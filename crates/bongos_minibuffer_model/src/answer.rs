/// Parses an answer to a yes/no question: `y` or `yes` is `Some(true)`, `n` or `no` is
/// `Some(false)`, ignoring case and surrounding whitespace. Anything else, including an empty
/// answer, is `None` (the caller applies its default or asks again).
pub fn parse_yes_no(input: &str) -> Option<bool> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if input.eq_ignore_ascii_case("n") || input.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}

/// Parses the answer to a strict `yes or no` question: only the whole words `yes` and `no`,
/// ignoring case and surrounding whitespace. `y`, `n`, an empty answer and anything else are
/// `None`, so the prompt stays open.
pub fn parse_strict(input: &str) -> Option<bool> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if input.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}
