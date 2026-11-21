//! Data model for the curated list of language extensions and for extension sources.

mod layout;
mod list;
mod preflight;
mod reconcile;
mod record;
mod source;

pub use layout::{IdError, Layout, is_reserved_id, validate_id};
pub use list::{LanguageList, ListEntry};
pub use preflight::{
    ApiVersion, GIT_MISSING_MESSAGE, MANIFEST_MISSING_MESSAGE, ManifestFacts, NO_RUST_MESSAGE,
    OLD_MANIFEST_MESSAGE, Probe, Refusal, TARGET_MISSING_WITH_RUSTUP_MESSAGE,
    TARGET_MISSING_WITHOUT_RUSTUP_MESSAGE, Tool, ToolProbe, api_requirement, check_after_clone,
    check_before_clone, check_built_api, repository_host,
};
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
