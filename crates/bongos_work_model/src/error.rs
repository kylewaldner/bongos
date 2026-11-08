use std::fmt;
use std::path::{Path, PathBuf};

/// A failure to read, parse or write one of the work model files. It names the file when the
/// failure came from a path, and the line and column when the TOML parser gives them.
#[derive(Debug)]
pub struct Error {
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub kind: ErrorKind,
}

#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Syntax(String),
    #[error("{}", unsupported_schema_message(.found))]
    UnsupportedSchema { found: Option<String> },
    #[error("{0}")]
    Serialize(String),
    #[error("{0}")]
    InvalidJournal(String),
}

fn unsupported_schema_message(found: &Option<String>) -> String {
    match found {
        Some(found) => format!("unsupported schema {found}, this version reads `schema = 1`"),
        None => "missing `schema = 1` at the top of the file".to_string(),
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn new(kind: ErrorKind) -> Self {
        Self {
            path: None,
            line: None,
            column: None,
            kind,
        }
    }

    pub(crate) fn io(path: &Path, error: std::io::Error) -> Self {
        Self::new(ErrorKind::Io(error)).with_path(Some(path))
    }

    pub(crate) fn with_path(mut self, path: Option<&Path>) -> Self {
        self.path = path.map(Path::to_path_buf);
        self
    }

    pub(crate) fn syntax(text: &str, error: &toml::de::Error) -> Self {
        let mut this = Self::new(ErrorKind::Syntax(error.message().to_string()));
        if let Some(span) = error.span() {
            let before = &text[..span.start.min(text.len())];
            this.line = Some(before.matches('\n').count() + 1);
            let line_start = before.rfind('\n').map_or(0, |index| index + 1);
            this.column = Some(before[line_start..].chars().count() + 1);
        }
        this
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{}", path.display())?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
                if let Some(column) = self.column {
                    write!(f, ":{column}")?;
                }
            }
            write!(f, ": ")?;
        } else if let Some(line) = self.line {
            write!(f, "line {line}")?;
            if let Some(column) = self.column {
                write!(f, ", column {column}")?;
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Io(error) => Some(error),
            _ => None,
        }
    }
}
