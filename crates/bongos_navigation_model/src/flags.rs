//! Search flags shown in the find prompt: regex, case mode, whole word and open buffers only.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CaseMode {
    /// Case-sensitive only when the text has an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Flags {
    pub regex: bool,
    pub case: CaseMode,
    pub whole_word: bool,
    pub open_buffers: bool,
}

impl Flags {
    pub fn case_sensitive(&self, text: &str) -> bool {
        match self.case {
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
            CaseMode::Smart => text.chars().any(char::is_uppercase),
        }
    }

    /// Smart, then Sensitive, then Insensitive, then back to Smart.
    pub fn cycle_case(&mut self) {
        self.case = match self.case {
            CaseMode::Smart => CaseMode::Sensitive,
            CaseMode::Sensitive => CaseMode::Insensitive,
            CaseMode::Insensitive => CaseMode::Smart,
        };
    }

    /// The marks for the prompt, always in the order regex, case, word, buffers. A lit flag is
    /// bracketed with its letters (`[.*]`, `[Aa]` for smart, `[A]` sensitive, `[a]` insensitive,
    /// `[w]`, `[buf]`); an unlit flag is omitted.
    pub fn prompt_marks(&self) -> String {
        let mut marks = String::new();
        if self.regex {
            marks.push_str("[.*]");
        }
        marks.push_str(match self.case {
            CaseMode::Smart => "[Aa]",
            CaseMode::Sensitive => "[A]",
            CaseMode::Insensitive => "[a]",
        });
        if self.whole_word {
            marks.push_str("[w]");
        }
        if self.open_buffers {
            marks.push_str("[buf]");
        }
        marks
    }
}

#[cfg(test)]
#[path = "flags_tests.rs"]
mod tests;
