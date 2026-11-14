//! Replacement templates: literal text mixed with group references.

use std::fmt;

/// One piece of a parsed replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Text copied as is.
    Literal(String),
    /// A numbered capture group, `\1` to `\9`.
    Group(usize),
    /// The whole match, `\&`.
    WholeMatch,
    /// The replacement counter, `\#`.
    Counter,
}

/// A parsed replacement string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

/// Why a replacement string could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateError {
    /// A backslash that starts no known escape; `at` is its byte offset.
    BadEscape { at: usize },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::BadEscape { .. } => {
                f.write_str("Invalid use of `\\` in replacement text; use \\1..\\9, \\&, \\#, \\\\")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

/// Parses `to`. In text mode (`regexp` false) the whole string is one
/// literal, backslashes included. In regexp mode `\1`..`\9`, `\&`, `\#` and
/// `\\` are recognized and any other backslash is an error.
pub fn parse(to: &str, regexp: bool) -> Result<Template, TemplateError> {
    if !regexp {
        let parts = if to.is_empty() {
            Vec::new()
        } else {
            vec![Part::Literal(to.to_string())]
        };
        return Ok(Template { parts });
    }
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut chars = to.char_indices();
    while let Some((at, c)) = chars.next() {
        if c != '\\' {
            literal.push(c);
            continue;
        }
        let part = match chars.next().map(|(_, e)| e) {
            Some('\\') => {
                literal.push('\\');
                continue;
            }
            Some(d @ '1'..='9') => Part::Group(d as usize - '0' as usize),
            Some('&') => Part::WholeMatch,
            Some('#') => Part::Counter,
            _ => return Err(TemplateError::BadEscape { at }),
        };
        if !literal.is_empty() {
            parts.push(Part::Literal(std::mem::take(&mut literal)));
        }
        parts.push(part);
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Ok(Template { parts })
}

impl Template {
    /// The highest group number the template names; 0 if none. `\&` is not
    /// a group number.
    pub fn max_group(&self) -> usize {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Group(n) => Some(*n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Expands the template. `groups[0]` is the whole match and `groups[i]`
    /// is group `i`; a group that did not participate or is out of range
    /// expands to nothing. `counter` is the number of replacements already
    /// made in the session.
    pub fn expand(&self, groups: &[Option<&str>], counter: usize) -> String {
        let group = |i: usize| groups.get(i).copied().flatten().unwrap_or("");
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Group(n) => out.push_str(group(*n)),
                Part::WholeMatch => out.push_str(group(0)),
                Part::Counter => out.push_str(&counter.to_string()),
            }
        }
        out
    }
}
