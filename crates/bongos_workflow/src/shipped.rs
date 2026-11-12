//! The definitions bongos ships (feature 20, step 20): "implement PR", "implement tasks", "ship
//! PR" and "babysit PR". Each lives under `shipped/<directory>/` as a `workflow.toml` with its
//! prompts and scripts, embedded in the binary with `include_str!` and written to
//! `<data dir>/workflows/shipped/<version>/` by [`crate::definitions::materialize`]. They are also
//! the templates `workflow: new` copies into the profile layer to be edited.

use crate::definitions::ShippedFile;

/// One shipped definition: the name it has in the workflow list and the directory its files are
/// in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    pub name: &'static str,
    pub dir: &'static str,
    pub description: &'static str,
}

/// The shipped definitions, in the order the `workflow: new` list shows them.
pub const TEMPLATES: &[Template] = &[
    Template {
        name: "implement PR",
        dir: "implement-pr",
        description: "Triage, plan, fan out over work items, test and review",
    },
    Template {
        name: "implement tasks",
        dir: "implement-tasks",
        description: "One agent implements the current todo",
    },
    Template {
        name: "ship PR",
        dir: "ship-pr",
        description: "Rebase when the parent PR merged, wait for CI and fix it, ask before merging, merge and wait for the deploy",
    },
    Template {
        name: "babysit PR",
        dir: "babysit-pr",
        description: "Watch a pull request: fix red CI (at most five passes), ask before merging when it is green",
    },
];

macro_rules! shipped_file {
    ($dir:literal, $file:literal) => {
        ShippedFile {
            path: concat!($dir, "/", $file),
            contents: include_str!(concat!("../shipped/", $dir, "/", $file)),
        }
    };
}

/// Every file of every shipped definition.
pub const FILES: &[ShippedFile] = &[
    shipped_file!("implement-pr", "workflow.toml"),
    shipped_file!("implement-pr", "triage.md"),
    shipped_file!("implement-pr", "plan.md"),
    shipped_file!("implement-pr", "implement.md"),
    shipped_file!("implement-pr", "implement_item.md"),
    shipped_file!("implement-pr", "fix.md"),
    shipped_file!("implement-pr", "review.md"),
    shipped_file!("implement-pr", "run_tests.py"),
    shipped_file!("implement-tasks", "workflow.toml"),
    shipped_file!("implement-tasks", "implement.md"),
    shipped_file!("ship-pr", "workflow.toml"),
    shipped_file!("ship-pr", "check_ci.py"),
    shipped_file!("ship-pr", "fix.md"),
    shipped_file!("ship-pr", "merge_pr.py"),
    shipped_file!("ship-pr", "push.py"),
    shipped_file!("ship-pr", "rebase_if_parent_merged.py"),
    shipped_file!("ship-pr", "resolve.md"),
    shipped_file!("babysit-pr", "workflow.toml"),
    shipped_file!("babysit-pr", "check_ci.py"),
    shipped_file!("babysit-pr", "fix.md"),
    shipped_file!("babysit-pr", "merge_pr.py"),
    shipped_file!("babysit-pr", "push.py"),
];

/// The template whose workflow name or directory is `key`.
pub fn template(key: &str) -> Option<&'static Template> {
    TEMPLATES
        .iter()
        .find(|template| template.name == key || template.dir == key)
}

/// The files of `template` with their path relative to its directory.
pub fn files_of(template: &Template) -> impl Iterator<Item = (&'static str, &'static str)> {
    FILES.iter().filter_map(move |file| {
        let relative = file.path.strip_prefix(template.dir)?.strip_prefix('/')?;
        Some((relative, file.contents))
    })
}
