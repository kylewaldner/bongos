//! Task checks: scripts and commands that must pass before a task counts as done. This holds
//! the settings, the commands (hidden while `task_checks.enabled` is false), the evidence store
//! over the project's state store, the exporter of that state, and the validation of check
//! declarations in a project's definitions.

mod actions;
mod definitions;
mod evidence_store;
mod exporter;
mod settings;

pub use actions::{ChecksToast, action_types, init_palette, register, task};
pub use definitions::{CHECKS_FOLDER, CHECKS_VALID, ChecksValid, validate_folder};
pub use evidence_store::{
    DONE_NAMESPACE, DoneRecord, EVIDENCE_NAMESPACE, LANDING_PREFIX, LAST_NAMESPACE, LIVE_NAMESPACE,
    OWED_NAMESPACE, SEQ_KEY, SEQ_NAMESPACE, delete_done, delete_record, done_record,
    done_record_in, evidence_key, evidence_prefix, landing_key, landing_prefix, next_generation,
    put_done, record_key, scan_records, scan_records_in, store_record, task_prefix,
};
pub use exporter::{ChecksExporter, EXPORT_NAMESPACE};
pub use settings::TaskChecksSettings;

/// Registers the settings, the `checks` state exporter, the `checks` definition folder and the
/// `checks-valid` landing check.
pub fn init(cx: &mut gpui::App) {
    use ::settings::Settings as _;
    TaskChecksSettings::register(cx);
    exporter::ChecksExporter::register();
    definitions::register(cx);
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod evidence_store_tests;
#[cfg(test)]
mod exporter_tests;
#[cfg(test)]
mod test_support;
