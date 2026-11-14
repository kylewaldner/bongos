//! The built-in landing check `schema`, pure over the paths and blobs of one tree: every file is
//! in `definitions/` (or is `.gitattributes`), in a known file or folder, and the definitions
//! parse.

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
