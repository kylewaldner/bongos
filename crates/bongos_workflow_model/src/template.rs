//! Prompt templates: plain `{{name}}` substitution.
//!
//! The names a prompt can use are `inputs.<name>`, `nodes.<node>.<field>`, `item` in a per-item
//! branch and `branches.<fan node>` after a join. A name with no value is an error, never empty
//! text. Substituted values are not scanned again.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("line {line}: unknown name '{name}'")]
    UnknownName { name: String, line: usize },
    #[error("line {line}: empty name in '{{{{}}}}'")]
    EmptyName { line: usize },
    #[error("line {line}: '{{{{' has no closing '}}}}'")]
    Unterminated { line: usize },
}

/// The values a template can name, by dotted name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    values: BTreeMap<String, String>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.values.insert(name.into(), value.into());
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

/// A `{{name}}` found in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub line: usize,
    start: usize,
    end: usize,
}

/// Lists every name a template uses, so a definition can be checked without a run.
pub fn references(text: &str) -> Result<Vec<Reference>, TemplateError> {
    let mut found = Vec::new();
    let mut rest = 0;
    while let Some(open) = text[rest..].find("{{").map(|offset| rest + offset) {
        let line = text[..open].matches('\n').count() + 1;
        let body = open + 2;
        let Some(close) = text[body..].find("}}").map(|offset| body + offset) else {
            return Err(TemplateError::Unterminated { line });
        };
        let name = text[body..close].trim();
        if name.is_empty() {
            return Err(TemplateError::EmptyName { line });
        }
        found.push(Reference {
            name: name.to_string(),
            line,
            start: open,
            end: close + 2,
        });
        rest = close + 2;
    }
    Ok(found)
}

/// Renders a template, failing on the first name the scope does not have.
pub fn render(text: &str, scope: &Scope) -> Result<String, TemplateError> {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for reference in references(text)? {
        let Some(value) = scope.get(&reference.name) else {
            return Err(TemplateError::UnknownName {
                name: reference.name,
                line: reference.line,
            });
        };
        output.push_str(&text[copied..reference.start]);
        output.push_str(value);
        copied = reference.end;
    }
    output.push_str(&text[copied..]);
    Ok(output)
}
