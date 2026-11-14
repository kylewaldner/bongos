//! Case transfer from a match to its replacement, after Emacs
//! `replace-match` with `case-replace`.
//!
//! A word initial is a letter or digit that is not preceded by one. Only
//! letters count toward "some lowercase" and "some uppercase"; a digit that
//! starts a word is neutral but is not an uppercase initial, so `1` and
//! `Foo 1` are [`Shape::AsIs`] while `A1` is [`Shape::Upper`].

/// The case pattern of a matched text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// All letters uppercase (`FOO`, or a single capital such as `X`).
    Upper,
    /// Every word starts uppercase and some word has several letters.
    Capitalized,
    /// No pattern to carry over.
    AsIs,
}

/// Classifies the case of `matched`.
pub fn shape(matched: &str) -> Shape {
    let mut some_lower = false;
    let mut some_upper = false;
    let mut some_multiletter_word = false;
    let mut some_nonupper_initial = false;
    let mut prev_word = false;
    for c in matched.chars() {
        if c.is_lowercase() {
            some_lower = true;
            if prev_word {
                some_multiletter_word = true;
            } else {
                some_nonupper_initial = true;
            }
        } else if c.is_uppercase() {
            some_upper = true;
            if prev_word {
                some_multiletter_word = true;
            }
        } else if c.is_alphanumeric() && !prev_word {
            some_nonupper_initial = true;
        }
        prev_word = c.is_alphabetic() || c.is_numeric();
    }
    if !some_lower && some_multiletter_word {
        Shape::Upper
    } else if !some_nonupper_initial && some_multiletter_word {
        Shape::Capitalized
    } else if !some_nonupper_initial && some_upper {
        Shape::Upper
    } else {
        Shape::AsIs
    }
}

/// Applies `shape` to `replacement`: `Upper` upcases everything,
/// `Capitalized` upcases each word initial and leaves the rest alone.
pub fn apply(shape: Shape, replacement: &str) -> String {
    match shape {
        Shape::Upper => replacement.to_uppercase(),
        Shape::AsIs => replacement.to_string(),
        Shape::Capitalized => {
            let mut out = String::with_capacity(replacement.len());
            let mut prev_word = false;
            for c in replacement.chars() {
                if prev_word {
                    out.push(c);
                } else {
                    out.extend(c.to_uppercase());
                }
                prev_word = c.is_alphabetic() || c.is_numeric();
            }
            out
        }
    }
}

/// Returns `apply(shape(matched), replacement)` when `enabled`, otherwise
/// `replacement` unchanged. Meant to run on the whole expanded replacement.
pub fn transfer(enabled: bool, matched: &str, replacement: &str) -> String {
    if enabled {
        apply(shape(matched), replacement)
    } else {
        replacement.to_string()
    }
}

/// Whether case should be transferred: the search ignores case and `from`
/// has no uppercase letter.
pub fn preserves_case(case_sensitive: bool, from: &str) -> bool {
    !case_sensitive && !from.chars().any(char::is_uppercase)
}
