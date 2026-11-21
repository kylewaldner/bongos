//! The curated list of language extensions, embedded in the binary.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::source::{SourceUrl, validate_relative_path};

const EMBEDDED: &str = include_str!("languages.toml");

/// One curated extension: a repository, optional subdirectory and a pinned commit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub repository: String,
    #[serde(default)]
    pub path: Option<String>,
    pub commit: String,
    #[serde(default)]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub api: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFile {
    #[serde(default)]
    extension: Vec<ListEntry>,
}

/// The parsed list of curated language extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageList {
    entries: Vec<ListEntry>,
}

impl LanguageList {
    /// Parses the list data compiled into the binary.
    pub fn embedded() -> Result<LanguageList> {
        Self::parse(EMBEDDED).context("parsing the embedded language extension list")
    }

    /// Parses list data and checks every entry's repository URL, subdirectory and commit.
    pub fn parse(text: &str) -> Result<LanguageList> {
        let file: ListFile = toml::from_str(text).context("invalid language extension list")?;
        for entry in &file.extension {
            SourceUrl::parse(&entry.repository)
                .with_context(|| format!("extension `{}`: bad repository", entry.id))?;
            if let Some(path) = &entry.path {
                validate_relative_path(path)
                    .with_context(|| format!("extension `{}`: bad path", entry.id))?;
            }
            let commit_ok = matches!(entry.commit.len(), 40 | 64)
                && entry.commit.bytes().all(|b| b.is_ascii_hexdigit());
            anyhow::ensure!(
                commit_ok,
                "extension `{}`: commit must be a full hex commit",
                entry.id
            );
        }
        Ok(LanguageList {
            entries: file.extension,
        })
    }

    /// Reads a list from `path` (debug builds only call this through [`LanguageList::load`]).
    pub fn load_from_file(path: &Path) -> Result<LanguageList> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// The embedded list. Debug builds read the file named by `BONGOS_EXTENSION_LIST` instead
    /// when that variable is set; release builds ignore it.
    pub fn load() -> Result<LanguageList> {
        if cfg!(debug_assertions)
            && let Some(path) = std::env::var_os("BONGOS_EXTENSION_LIST")
        {
            return Self::load_from_file(Path::new(&path));
        }
        Self::embedded()
    }

    pub fn entries(&self) -> &[ListEntry] {
        &self.entries
    }

    /// Finds an entry by display name, ignoring case.
    pub fn entry_by_name(&self, name: &str) -> Option<&ListEntry> {
        self.entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
    }

    /// Finds the entry that lists the file type of `path`. Like Zed's language registry, a file
    /// type matches a file name that equals it or ends in `.<file type>`, and the comparison is
    /// case-sensitive.
    pub fn entry_for_path(&self, path: &Path) -> Option<&ListEntry> {
        let file_name = path.file_name()?.to_str()?;
        self.entries.iter().find(|entry| {
            entry.file_types.iter().any(|file_type| {
                file_name == file_type
                    || file_name
                        .strip_suffix(file_type.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
        })
    }

    /// True only when an entry has the same normalized repository, the same subdirectory and the
    /// same commit (compared ignoring hex case).
    pub fn is_listed(&self, source: &SourceUrl, path: Option<&str>, commit: &str) -> bool {
        self.entries.iter().any(|entry| {
            SourceUrl::parse(&entry.repository).is_ok_and(|url| &url == source)
                && normalize_path(entry.path.as_deref()) == normalize_path(path)
                && entry.commit.eq_ignore_ascii_case(commit)
        })
    }
}

fn normalize_path(path: Option<&str>) -> Option<&str> {
    path.map(|path| path.trim_matches('/'))
        .filter(|path| !path.is_empty())
}
