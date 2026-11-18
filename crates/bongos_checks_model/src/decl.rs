//! The declaration of one check: a TOML table, the same in a task's set, a named set and a
//! closing set. Parsing, with line numbers where the text is at hand ([`parse_file`]), validation
//! ([`validate`]) and the digest of a check and the files it names ([`digest`]).
//!
//! A task's own table is read from `TaskDefinition.extra["checks"]`, which carries no spans
//! ([`parse`]), so a declaration read that way has no line. Findings then say "line unknown".

use std::fmt;

use serde::{Deserialize, Serialize};
use toml::Spanned;

mod digest;
mod validate;

pub use digest::{Digest, DigestError, digest};
pub use validate::validate;

/// How serious a [`Finding`] is. Errors keep a set from landing; warnings are kept and shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A problem in a check declaration, with the file, the line when known and the check's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Relative to `definitions/`.
    pub file: String,
    /// Only where the text was parsed with [`parse_file`].
    pub line: Option<usize>,
    /// `None` for a problem of the whole table (for example a bad `inherit`).
    pub check: Option<String>,
    pub message: String,
}

impl Finding {
    pub(crate) fn error(
        file: &str,
        line: Option<usize>,
        check: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Error, file, line, check, message)
    }

    pub(crate) fn warning(
        file: &str,
        line: Option<usize>,
        check: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Warning, file, line, check, message)
    }

    fn new(
        severity: Severity,
        file: &str,
        line: Option<usize>,
        check: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            file: file.to_string(),
            line,
            check: check.map(str::to_string),
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, ", self.file)?;
        match self.line {
            Some(line) => write!(f, "line {line}")?,
            None => f.write_str("line unknown")?,
        }
        if let Some(check) = &self.check {
            write!(f, ", check {check}")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// One entry of `needs`: a pool name or a table with a `pool`. Everything but the pool name is
/// kept unparsed for the pool feature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NeedEntry {
    Pool(String),
    Request {
        pool: String,
        #[serde(flatten)]
        rest: toml::Table,
    },
    /// Neither form; [`validate`] reports it.
    Invalid(toml::Value),
}

/// `parts`: a count, or a plan script.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Parts {
    Count(i64),
    Plan {
        plan: String,
    },
    /// Neither form; [`validate`] reports it.
    Invalid(toml::Value),
}

/// `scope`: a scope script, or the word `"all"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scope {
    Script {
        script: String,
    },
    Word(String),
    /// Neither form; [`validate`] reports it.
    Invalid(toml::Value),
}

/// The declaration of one check. Optional keys keep what was written; the accessors give the
/// defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckDecl {
    /// Unique in its set.
    pub name: String,
    /// A script path in the definitions; exactly one of `run` and `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// A command found on `PATH`; exactly one of `run` and `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The task's trees in these repositories; default its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<Vec<i32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<Vec<i32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Vec<i32>>,
    /// A regex; a pass needs a matching output line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_file: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<NeedEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Parts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_at_first_failure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<String>,
    /// Free data handed to the scripts as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<toml::Table>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub or_accepted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_minutes: Option<u32>,
    /// Keys this model does not know; kept, and warned about by [`validate`].
    #[serde(flatten)]
    pub unknown: toml::Table,
    /// Where the declaration starts in its file; only when parsed by [`parse_file`].
    #[serde(skip)]
    pub line: Option<usize>,
}

impl CheckDecl {
    /// Exit codes that pass; default `[0]`.
    pub fn pass_codes(&self) -> Vec<i32> {
        self.pass.clone().unwrap_or_else(|| vec![0])
    }

    /// Exit codes that fail; default `[1]`.
    pub fn fail_codes(&self) -> Vec<i32> {
        self.fail.clone().unwrap_or_else(|| vec![1])
    }

    /// Exit codes that mean "environment"; default none.
    pub fn environment_codes(&self) -> Vec<i32> {
        self.environment.clone().unwrap_or_default()
    }

    /// Default true; false is advisory.
    pub fn is_required(&self) -> bool {
        self.required.unwrap_or(true)
    }

    pub fn is_parallel(&self) -> bool {
        self.parallel.unwrap_or(false)
    }

    pub fn stops_at_first_failure(&self) -> bool {
        self.stop_at_first_failure.unwrap_or(false)
    }

    pub fn writes_result_file(&self) -> bool {
        self.result_file.unwrap_or(false)
    }

    /// The paths in the definitions the declaration names, as written: `run`, `scope.script`,
    /// `parts.plan` and every `pinned` file.
    pub fn named_files(&self) -> Vec<&str> {
        let mut files = Vec::new();
        files.extend(self.run.as_deref());
        if let Some(Scope::Script { script }) = &self.scope {
            files.push(script);
        }
        if let Some(Parts::Plan { plan }) = &self.parts {
            files.push(plan);
        }
        files.extend(self.pinned.iter().map(String::as_str));
        files
    }

    /// The declaration with every default written out, so a key written with its default value
    /// digests like a key left out.
    pub(crate) fn resolved(&self) -> CheckDecl {
        CheckDecl {
            pass: Some(self.pass_codes()),
            fail: Some(self.fail_codes()),
            environment: Some(self.environment_codes()),
            result_file: Some(self.writes_result_file()),
            parallel: Some(self.is_parallel()),
            stop_at_first_failure: Some(self.stops_at_first_failure()),
            required: Some(self.is_required()),
            line: None,
            ..self.clone()
        }
    }
}

/// `inherit` of a task's `checks` table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Inherit {
    /// The parent's effective set is the start (the default).
    #[default]
    Parent,
    /// The task starts with nothing.
    None,
}

/// A `checks` table (a task's, or the top level of a named set file): its keys and declarations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChecksTable {
    pub file: String,
    pub inherit: Inherit,
    /// Named sets, in order.
    pub uses: Vec<String>,
    /// Names removed from the inherited and used sets.
    pub drop: Vec<String>,
    pub check: Vec<CheckDecl>,
    /// Closing checks, meaningful on a group.
    pub closing: Vec<CheckDecl>,
    /// Entries that could not be read, and bad table keys. The entries are left out.
    pub findings: Vec<Finding>,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "E: Deserialize<'de>"))]
struct RawTable<E> {
    #[serde(default)]
    inherit: Option<toml::Value>,
    #[serde(default, rename = "use")]
    uses: Vec<String>,
    #[serde(default)]
    drop: Vec<String>,
    #[serde(default)]
    check: Vec<E>,
    #[serde(default)]
    closing: Vec<E>,
}

/// An array entry, with its start offset in the text when it was parsed with spans.
trait Entry {
    fn split(self) -> (toml::Table, Option<usize>);
}

impl Entry for toml::Table {
    fn split(self) -> (toml::Table, Option<usize>) {
        (self, None)
    }
}

impl Entry for Spanned<toml::Table> {
    fn split(self) -> (toml::Table, Option<usize>) {
        let start = self.span().start;
        (self.into_inner(), Some(start))
    }
}

/// Reads the `checks` table of a task, `TaskDefinition.extra["checks"]`. It carries no spans, so
/// no declaration has a line.
pub fn parse(file: &str, checks: &toml::Value) -> ChecksTable {
    match checks.clone().try_into::<RawTable<toml::Table>>() {
        Ok(raw) => build(file, raw, None),
        Err(error) => failed(file, None, format!("checks: {}", error.message())),
    }
}

/// The text of a file with check declarations: a named set (`[[check]]` and `[[closing]]` at the
/// top level) or a `task.toml` (`[[checks.check]]` and so on). Declarations carry their line.
/// `Err` only when the text is not TOML.
pub fn parse_file(path: &str, text: &str) -> Result<ChecksTable, Finding> {
    #[derive(Deserialize)]
    struct RawFile {
        #[serde(default)]
        checks: Option<RawTable<Spanned<toml::Table>>>,
        #[serde(default)]
        check: Vec<Spanned<toml::Table>>,
        #[serde(default)]
        closing: Vec<Spanned<toml::Table>>,
    }
    let raw: RawFile = toml::from_str(text).map_err(|error| {
        let line = error.span().map(|span| line_of(text, span.start));
        Finding::error(path, line, None, error.message().to_string())
    })?;
    let table = raw.checks.unwrap_or(RawTable {
        inherit: None,
        uses: Vec::new(),
        drop: Vec::new(),
        check: raw.check,
        closing: raw.closing,
    });
    Ok(build(path, table, Some(text)))
}

fn failed(file: &str, line: Option<usize>, message: String) -> ChecksTable {
    ChecksTable {
        file: file.to_string(),
        findings: vec![Finding::error(file, line, None, message)],
        ..ChecksTable::default()
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn build<E: Entry>(file: &str, raw: RawTable<E>, text: Option<&str>) -> ChecksTable {
    let mut table = ChecksTable {
        file: file.to_string(),
        uses: raw.uses,
        drop: raw.drop,
        ..ChecksTable::default()
    };
    match raw.inherit.as_ref().map(|value| value.as_str()) {
        None | Some(Some("parent")) => {}
        Some(Some("none")) => table.inherit = Inherit::None,
        Some(_) => table.findings.push(Finding::error(
            file,
            None,
            None,
            "inherit must be \"parent\" or \"none\"",
        )),
    }
    for (entries, into_closing) in [(raw.check, false), (raw.closing, true)] {
        for entry in entries {
            let (entry, start) = entry.split();
            let line = start.zip(text).map(|(start, text)| line_of(text, start));
            match decl_of(file, entry, line) {
                Ok(decl) if into_closing => table.closing.push(decl),
                Ok(decl) => table.check.push(decl),
                Err(finding) => table.findings.push(finding),
            }
        }
    }
    table
}

fn decl_of(file: &str, entry: toml::Table, line: Option<usize>) -> Result<CheckDecl, Finding> {
    let name = entry
        .get("name")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    match toml::Value::Table(entry).try_into::<CheckDecl>() {
        Ok(mut decl) => {
            decl.line = line;
            Ok(decl)
        }
        Err(error) => Err(Finding::error(
            file,
            line,
            Some(name.as_deref().unwrap_or("(unnamed)")),
            error.message().to_string(),
        )),
    }
}
