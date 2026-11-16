//! Data model for the curated list of language extensions and for extension sources.

mod layout;
mod list;
mod reconcile;
mod record;
mod source;

pub use layout::{IdError, Layout, is_reserved_id, validate_id};
pub use list::{LanguageList, ListEntry};
pub use reconcile::{
    CheckoutSnapshot, ExtensionSnapshot, Fix, RecordSnapshot, Snapshot, SymlinkSnapshot,
    SymlinkTarget, plan,
};
pub use record::{
    InstallRecord, ReadOutcome, RecordState, read, remove_torn_temp_files, write_atomic,
};
pub use source::{RevSpec, SourceUrl, SourceUrlError, validate_relative_path};

#[cfg(test)]
mod tests;
