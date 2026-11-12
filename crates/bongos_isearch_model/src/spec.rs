#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub fn opposite(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseMode {
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchSpec {
    pub string: String,
    pub regexp: bool,
    pub symbol: bool,
    pub case: CaseMode,
    pub lax: bool,
}

impl SearchSpec {
    pub fn new(regexp: bool) -> Self {
        Self {
            string: String::new(),
            regexp,
            symbol: false,
            case: CaseMode::Smart,
            lax: true,
        }
    }

    pub fn case_sensitive(&self) -> bool {
        match self.case {
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
            CaseMode::Smart => has_uppercase(&self.string, self.regexp),
        }
    }

    /// Sets the opposite of the current effective value, so the mode is
    /// explicit from then on.
    pub fn toggle_case(&mut self) {
        self.case = if self.case_sensitive() {
            CaseMode::Insensitive
        } else {
            CaseMode::Sensitive
        };
    }
}

/// In a regexp a character right after a backslash does not count (`\S`,
/// `\W`, `\B` are not uppercase letters).
fn has_uppercase(string: &str, regexp: bool) -> bool {
    let mut characters = string.chars();
    while let Some(character) = characters.next() {
        if regexp && character == '\\' {
            characters.next();
        } else if character.is_uppercase() {
            return true;
        }
    }
    false
}
