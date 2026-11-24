//! Parses a find request such as `TODO -- src/main *.kt !*Test.kt` into search text and scope
//! tokens. Everything before the first ` -- ` is the text; the rest is space-separated tokens.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeToken {
    /// A directory, which becomes the include glob `<dir>/**`.
    Directory(String),
    /// A glob such as `*.kt`; matches are included.
    Include(String),
    /// A glob written `!*Test.kt`; matches are excluded. The stored glob has no `!`.
    Exclude(String),
}

impl ScopeToken {
    /// The glob this token stands for.
    pub fn glob(&self) -> String {
        match self {
            ScopeToken::Directory(dir) => format!("{}/**", dir.trim_end_matches('/')),
            ScopeToken::Include(glob) | ScopeToken::Exclude(glob) => glob.clone(),
        }
    }

    pub fn is_exclude(&self) -> bool {
        matches!(self, ScopeToken::Exclude(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FindRequestParts {
    pub text: String,
    pub scope: Vec<ScopeToken>,
}

impl FindRequestParts {
    pub fn include_globs(&self) -> Vec<String> {
        self.scope
            .iter()
            .filter(|t| !t.is_exclude())
            .map(ScopeToken::glob)
            .collect()
    }

    pub fn exclude_globs(&self) -> Vec<String> {
        self.scope
            .iter()
            .filter(|t| t.is_exclude())
            .map(ScopeToken::glob)
            .collect()
    }
}

fn has_glob_chars(token: &str) -> bool {
    token.contains(['*', '?', '[', '{'])
}

pub fn parse(input: &str) -> FindRequestParts {
    let Some((text, rest)) = input.split_once(" -- ") else {
        return FindRequestParts {
            text: input.to_string(),
            scope: Vec::new(),
        };
    };
    let scope = rest
        .split_whitespace()
        .map(|token| {
            if let Some(glob) = token.strip_prefix('!') {
                ScopeToken::Exclude(glob.to_string())
            } else if has_glob_chars(token) {
                ScopeToken::Include(token.to_string())
            } else {
                ScopeToken::Directory(token.to_string())
            }
        })
        .collect();
    FindRequestParts {
        text: text.to_string(),
        scope,
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
