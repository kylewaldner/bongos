//! Reading one declaration file: schema, the array of entries with their lines, and unknown keys.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::Deserialize;
use toml::Spanned;

pub const SCHEMA_VERSION: u32 = 1;

/// A value with the line it was written on.
#[derive(Debug, Clone, PartialEq)]
pub struct Located<T> {
    pub line: usize,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub message: String,
}

pub(crate) struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let newlines = text
            .bytes()
            .enumerate()
            .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset))
            .collect();
        Self { newlines }
    }

    pub(crate) fn line(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&newline| newline < offset) + 1
    }

    pub(crate) fn error(
        &self,
        span: Option<Range<usize>>,
        message: impl Into<String>,
    ) -> ParseError {
        ParseError {
            line: span.map_or(1, |span| self.line(span.start)),
            message: message.into(),
        }
    }
}

/// One `[[pool]]`, `[[queue]]` or `[[trigger]]` entry.
pub(crate) struct RawEntry {
    pub line: usize,
    pub table: toml::Table,
    /// The line of each key written directly in the entry.
    pub key_lines: BTreeMap<String, usize>,
}

impl RawEntry {
    /// The line of `key`, or of the entry when it is absent.
    pub fn key_line(&self, key: &str) -> usize {
        self.key_lines.get(key).copied().unwrap_or(self.line)
    }
}

pub(crate) struct RawFile {
    pub entries: Vec<RawEntry>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct RawTop {
    schema: Spanned<u32>,
    #[serde(default)]
    pool: Vec<Spanned<toml::Table>>,
    #[serde(default)]
    queue: Vec<Spanned<toml::Table>>,
    #[serde(default)]
    trigger: Vec<Spanned<toml::Table>>,
}

/// Reads a file whose entries are the array `array_key`. Blank text (a missing file) has no
/// entries. Syntax, type and schema errors are a [`ParseError`]; unknown keys are diagnostics.
pub(crate) fn read_file(
    text: &str,
    array_key: &str,
    entry_keys: &[&str],
    place: &str,
) -> Result<RawFile, ParseError> {
    if text.trim().is_empty() {
        return Ok(RawFile {
            entries: Vec::new(),
            diagnostics: Vec::new(),
        });
    }
    let lines = LineIndex::new(text);
    let raw: RawTop =
        toml::from_str(text).map_err(|error| lines.error(error.span(), error.message()))?;
    if raw.schema.get_ref() != &SCHEMA_VERSION {
        return Err(lines.error(
            Some(raw.schema.span()),
            format!(
                "unsupported schema {}; this version reads schema {SCHEMA_VERSION}",
                raw.schema.get_ref()
            ),
        ));
    }
    let spanned = match array_key {
        "pool" => raw.pool,
        "queue" => raw.queue,
        _ => raw.trigger,
    };

    let mut diagnostics = Vec::new();
    let document = toml_edit::ImDocument::parse(text)
        .map_err(|error| lines.error(error.span(), error.message()))?;
    let root = document.as_table();
    check_keys(
        root,
        &["schema", array_key],
        "the file",
        &lines,
        &mut diagnostics,
    );
    let edits = children(root, array_key);

    let mut entries = Vec::new();
    for (index, entry) in spanned.into_iter().enumerate() {
        let line = lines.line(entry.span().start);
        let mut key_lines = BTreeMap::new();
        if let Some(edit) = edits.get(index) {
            check_keys(*edit, entry_keys, place, &lines, &mut diagnostics);
            for (key, _) in edit.iter() {
                let key_line = edit
                    .get_key_value(key)
                    .and_then(|(key, _)| key.span())
                    .map_or(line, |span| lines.line(span.start));
                key_lines.insert(key.to_string(), key_line);
            }
        }
        entries.push(RawEntry {
            line,
            table: entry.into_inner(),
            key_lines,
        });
    }
    Ok(RawFile {
        entries,
        diagnostics,
    })
}

fn tables_in(item: &toml_edit::Item) -> Vec<&dyn toml_edit::TableLike> {
    match item {
        toml_edit::Item::Table(table) => vec![table as &dyn toml_edit::TableLike],
        toml_edit::Item::ArrayOfTables(tables) => tables
            .iter()
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        toml_edit::Item::Value(toml_edit::Value::InlineTable(table)) => {
            vec![table as &dyn toml_edit::TableLike]
        }
        toml_edit::Item::Value(toml_edit::Value::Array(array)) => array
            .iter()
            .filter_map(toml_edit::Value::as_inline_table)
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn children<'a>(
    table: &'a dyn toml_edit::TableLike,
    key: &str,
) -> Vec<&'a dyn toml_edit::TableLike> {
    table.get(key).map(tables_in).unwrap_or_default()
}

pub(crate) fn check_keys(
    table: &dyn toml_edit::TableLike,
    known: &[&str],
    place: &str,
    lines: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    for (key, item) in table.iter() {
        if known.contains(&key) {
            continue;
        }
        let span = table
            .get_key_value(key)
            .and_then(|(key, _)| key.span())
            .or_else(|| item.span());
        out.push(Diagnostic {
            line: span.map_or(1, |span| lines.line(span.start)),
            message: format!("unknown key \"{key}\" in {place}"),
        });
    }
}

/// Reports every key of `table` not in `known`, at `line`.
pub(crate) fn check_nested_keys(
    table: &toml::Table,
    known: &[&str],
    place: &str,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            out.push(Diagnostic {
                line,
                message: format!("unknown key \"{key}\" in {place}"),
            });
        }
    }
}
