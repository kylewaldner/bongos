//! The built-in landing checks, pure over parsed trees, and the data a landing check works with.
//! This file holds check 1, `schema`, over the paths and blobs of one tree: every file is in
//! `definitions/` (or is `.gitattributes`), in a known file or folder, and the definitions parse;
//! [`landing_declaration_problems`] adds the project's `[[landing.checks]]` entries.

mod declared;
mod live;
mod no_cycle;
mod no_state;
mod outcome;
mod references;
mod secrets;
mod size_and_kind;
mod started;

pub use declared::{
    APPROVAL_CHECK, CANNOT_DISABLE, NO_CYCLE_CHECK, NO_STATE_CHECK, REFERENCES_CHECK,
    SECRETS_CHECK, SIZE_AND_KIND_CHECK, STARTED_UNCHANGED_CHECK, landing_declaration_problems,
};
pub use live::{HoldReason, LiveFacts, LiveSource, MoveHold, state_is_started};
pub use no_cycle::no_cycle;
pub use no_state::no_state;
pub use outcome::{AmendOutcome, CheckOutcome, CheckPhase, Stamp};
pub use references::references;
pub use secrets::{compile_secret_patterns, secrets};
pub use size_and_kind::{
    BINARY_SNIFF_BYTES, FileFacts, IMAGE_EXTENSIONS, SizeLimits, size_and_kind,
};
pub use started::{
    MOVE_MARKER, StagedTodoMove, StartedInput, started_unchanged, strip_move_meta, strip_move_text,
};

#[cfg(test)]
mod declared_tests;
#[cfg(test)]
mod pure_checks_tests;
#[cfg(test)]
mod started_tests;

use crate::definitions::{DefinitionTree, Problem, RelPath};
use crate::layout::{
    DEFINITIONS_DIR, GITATTRIBUTES_FILE, PLAN_FILE, PROJECT_FILE, TASK_FILE, is_known_folder,
};

/// The name of the schema check, as results and trailers show it.
pub const SCHEMA_CHECK: &str = "schema";

/// The problems of the layout of a tree whose files are `paths` (relative to the repository
/// root). `extra` lists more paths under `definitions/` that are allowed, such as the files an
/// amending check writes.
pub fn layout_problems<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    extra: &[String],
) -> Vec<Problem> {
    let prefix = format!("{DEFINITIONS_DIR}/");
    let mut problems = Vec::new();
    for path in paths {
        if path == GITATTRIBUTES_FILE {
            continue;
        }
        let Some(relative) = path.strip_prefix(&prefix) else {
            problems.push(Problem {
                path: path.to_string(),
                reason: format!(
                    "only {DEFINITIONS_DIR}/ and {GITATTRIBUTES_FILE} may be committed"
                ),
            });
            continue;
        };
        let known = match relative.split_once('/') {
            None => {
                [PROJECT_FILE, TASK_FILE, PLAN_FILE].contains(&relative)
                    || extra.iter().any(|allowed| allowed == relative)
            }
            Some((folder, _)) => {
                is_known_folder(folder) || extra.iter().any(|allowed| allowed == relative)
            }
        };
        if !known {
            problems.push(Problem {
                path: relative.to_string(),
                reason: "not a known file or folder of definitions/".to_string(),
            });
        }
    }
    problems
}

/// Every problem the schema check finds in a tree: its layout, then its definitions, each of
/// `files` being a blob under `definitions/` with its path relative to it.
pub fn schema_problems<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    files: impl IntoIterator<Item = (RelPath, Vec<u8>)>,
    extra: &[String],
) -> Vec<Problem> {
    let mut problems = layout_problems(paths, extra);
    if let Err(parse) = DefinitionTree::parse_files(files) {
        problems.extend(parse);
    }
    problems
}
