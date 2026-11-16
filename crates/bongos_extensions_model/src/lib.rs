//! Data model for the curated list of language extensions and for extension sources.

mod list;
mod source;

pub use list::{LanguageList, ListEntry};
pub use source::{RevSpec, SourceUrl, SourceUrlError, validate_relative_path};

#[cfg(test)]
mod tests;
