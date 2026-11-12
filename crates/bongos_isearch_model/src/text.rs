use crate::spec::{CaseMode, Direction, SearchSpec};
use crate::state::Step;

pub const COUNT_CAP: usize = 10_000;

const NEWLINE_MARK: char = '\u{23CE}';

/// `[Failing ][Overwrapped |Wrapped ][case-sensitive |case-insensitive ]
/// [Symbol |Regexp |]I-search[ backward]: `
pub fn prompt(step: &Step, spec: &SearchSpec) -> String {
    let mut prompt = String::new();
    if step.failing {
        prompt.push_str("Failing ");
    }
    if step.overwrapped {
        prompt.push_str("Overwrapped ");
    } else if step.wrapped {
        prompt.push_str("Wrapped ");
    }
    match spec.case {
        CaseMode::Sensitive => prompt.push_str("case-sensitive "),
        CaseMode::Insensitive => prompt.push_str("case-insensitive "),
        CaseMode::Smart => {}
    }
    if !spec.lax && !spec.regexp && !spec.symbol {
        prompt.push_str("strict-space ");
    }
    if spec.symbol {
        prompt.push_str("Symbol ");
    } else if step.regexp {
        prompt.push_str("Regexp ");
    }
    prompt.push_str("I-search");
    if step.direction == Direction::Backward {
        prompt.push_str(" backward");
    }
    prompt.push_str(": ");
    prompt
}

/// `index` is one-based. A `capped` count stopped at the cap: it reads `10000+`; exactly
/// 10000 matches read `n/10000`. Without matches the counter is blank.
pub fn counter(index: Option<usize>, total: usize, capped: bool) -> String {
    if total == 0 {
        return String::new();
    }
    let index = index.map_or_else(|| "?".to_string(), |index| index.to_string());
    if capped {
        format!("{index}/{COUNT_CAP}+")
    } else {
        format!("{index}/{total}")
    }
}

pub fn to_input(string: &str) -> String {
    string.replace('\n', &NEWLINE_MARK.to_string())
}

pub fn from_input(input: &str) -> String {
    input.replace(NEWLINE_MARK, "\n")
}
