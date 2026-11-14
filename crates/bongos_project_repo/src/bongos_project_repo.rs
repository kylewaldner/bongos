//! Project repositories: a project's definitions kept in a git repository, the per-machine
//! record of where each one lives, the creation of a new one, and its live state store.

mod amending;
mod approval;
mod builtin_checks;
mod changes;
mod check_run;
mod create;
mod declared_checks;
mod git;
mod host_boundary;
mod landing;
mod landing_checks;
mod landing_queue;
mod loader;
mod locations;
mod master;
mod peer;
mod pins;
mod quick_edit;
mod registries;
pub mod router;
mod script_check;
mod settings;
mod sink;
mod state;
mod state_hooks;
mod state_ops;
mod working_copies;

pub use amending::KEEPS_AMENDING;
pub use approval::{
    ApprovalHook, ApprovalHooks, ApprovalInput, ApprovalRow, ChatTaskLookup,
    MOVE_ROLLBACK_NAMESPACE, NEEDS_A_WINDOW, OwnSubtreeHook,
};
pub use bongos_project_repo_model::checks::{
    AmendOutcome, CheckOutcome, CheckPhase, HoldReason, LiveFacts, LiveSource, Stamp,
};
pub use bongos_project_repo_model::checks::{
    ApprovalBasis, ApprovalOpinion, ApprovalPolicy, ApprovalVerdict, AutomaticRule,
};
pub use changes::{AmendCommit, ChangeFilter, ChangeRow, ChangeState};
pub use check_run::CheckResult;
pub use create::MAIN_BRANCH;
pub use create::{CreateError, CreateRequest, LayoutTemplates, ProjectRepo};
pub use declared_checks::DeclaredCheck;
pub use git::{
    CommitId, DEFAULT_IDENTITY_EMAIL, DEFAULT_IDENTITY_NAME, Git, GitError, Identity,
    RebaseOutcome, TreeEntry, TreeId, TreeRef,
};
pub use host_boundary::HostBoundary;
pub use landing::{
    ALREADY_LANDING, DEFAULT_SECRET_PATTERNS, LandMode, LandOptions, LandRequest, LandingEvent,
    LandingResult, MAIN_REF, MASTER_IS_SPLIT, RepoEvent, UNSAVED_EDITS, WAITING_MASTER_SPLIT,
};
pub use landing_checks::{
    AmendingCheck, CheckInput, CheckKindParser, CheckKinds, CheckRegistry, DefinitionFolders,
    FolderValidator, LandingCheck, LandingChecks, LiveSources, WorkflowResolver, registered_checks,
};
pub use loader::{LoadedProject, StateHandle, load_definition_tree};
pub use locations::{Location, LocationError, RepositoryLocations};
pub use master::{
    HistoryKind, HistoryRow, MasterStatus, NOT_A_LANDING, OutsideChange, Recovery, Resolved,
    SplitMaster, SplitResolution, UNAPPROVED_OUTSIDE_CHANGE, landed_ref, outside_ref,
};
pub use peer::{ClientKind, PeerIdentity, peer_identity};
pub use pins::PinGuard;
pub use quick_edit::{
    LANDING_MARK, NOT_LANDED_MARK, QUICK_EDITS_NAMESPACE, QuickEdit, QuickEditStarted,
    STALE_SNAPSHOT, check_fresh, written_tasks,
};
pub use registries::{ActivitySource, ActivitySources, StateExporter, StateExporters};
pub use script_check::ScriptCheck;
pub use settings::ProjectRepositorySettings;
pub use sink::{LocalSink, PersonClaim, RefusingSink, RepoCommand, RepoReply, StateSink};
pub use state::{
    KvRead, KvTx, OpenError, StateEvent, StateRead, StateReader, StateStore, StateTx, StateWriter,
    VERIFIED_COMMIT_KEY,
};
pub use state_hooks::{StateApplyChange, StateApplyHook, StateApplyHooks, TaskTouch};
pub use state_ops::{SETTABLE_FIELDS, StateOp};
pub use working_copies::{
    AbandonRefused, WorkingCopy, clean_name, fresh_name, generated_name, protect_files,
};

/// Registers the settings of project repositories.
pub fn init(cx: &mut gpui::App) {
    use ::settings::Settings as _;
    ProjectRepositorySettings::register(cx);
}

#[cfg(test)]
mod amending_tests;
#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod check_fakes;
#[cfg(test)]
mod create_tests;
#[cfg(test)]
mod git_tests;
#[cfg(test)]
mod landing_checks_tests;
#[cfg(test)]
mod landing_race_tests;
#[cfg(test)]
mod landing_tests;
#[cfg(test)]
mod loader_tests;
#[cfg(test)]
mod locations_tests;
#[cfg(test)]
mod master_tests;
#[cfg(test)]
mod peer_tests;
#[cfg(test)]
mod quick_edit_tests;
#[cfg(test)]
mod router_tests;
#[cfg(test)]
mod sink_tests;
#[cfg(test)]
mod split_tests;
#[cfg(test)]
mod state_hooks_tests;
#[cfg(test)]
mod state_ops_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod working_copies_tests;
