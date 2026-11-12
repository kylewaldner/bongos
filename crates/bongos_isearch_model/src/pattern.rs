use crate::spec::SearchSpec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Escapes the regex metacharacters, the same set as `regex::escape`.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub fn compile(spec: &SearchSpec) -> Compiled {
    let case_sensitive = spec.case_sensitive();
    let string = spec.string.as_str();
    if spec.symbol {
        return Compiled {
            pattern: escape(string),
            regex: true,
            case_sensitive,
            whole_word: true,
        };
    }
    if spec.regexp {
        return Compiled {
            pattern: string.to_string(),
            regex: true,
            case_sensitive,
            whole_word: false,
        };
    }
    let (pattern, regex) = if spec.lax {
        lax_pattern(string)
    } else {
        (string.to_string(), false)
    };
    Compiled {
        pattern,
        regex,
        case_sensitive,
        whole_word: false,
    }
}

/// Space runs between two terms become a lazy gap; leading and trailing runs
/// stay literal. The gap is `.*?` rather than `[^\n]*?` so the pattern does
/// not contain a newline escape and is not searched as multiline.
fn lax_pattern(string: &str) -> (String, bool) {
    let trimmed_start = string.trim_start_matches(' ');
    let leading = &string[..string.len() - trimmed_start.len()];
    let core = trimmed_start.trim_end_matches(' ');
    let trailing = &trimmed_start[core.len()..];
    if !core.contains(' ') {
        return (string.to_string(), false);
    }
    let terms = core
        .split(' ')
        .filter(|term| !term.is_empty())
        .map(escape)
        .collect::<Vec<_>>();
    let mut pattern = escape(leading);
    pattern.push_str(&terms.join(".*?"));
    pattern.push_str(&escape(trailing));
    (pattern, true)
}
