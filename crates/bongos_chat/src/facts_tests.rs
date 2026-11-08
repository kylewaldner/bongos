//! `task_facts` against task trees written to a temporary directory and loaded as the Work store
//! loads them.

use std::path::{Path, PathBuf};

use bongos_work::is_project_top;
use bongos_work_model::store::{WorkTree, load};
use tempfile::TempDir;

use crate::task_facts;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task_toml(id: &str, goal: &str, kind: &str, status: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"{kind}\"\nstatus = \"{status}\"\n\
         created = 2020-01-01T10:00:00Z\n{extra}"
    )
}

const SECRET_FILE: &str = "secret-numbers.md";
const SECRET_NOTE: &str = "salary-review.md";
const SECRET_DESCRIPTION: &str = "Compensation numbers nobody shares";

struct Tree {
    _temp: TempDir,
    root: PathBuf,
    tree: WorkTree,
}

/// A project `invoicing` with a group `billing` holding the leaves `schema` (done), `review`
/// (open) and `backfill` (after `schema` and `review`, with human-only artifacts and a `prs`
/// result), and a one-off with a `pr` result.
fn tree() -> Tree {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let project = root.join("projects/invoicing");
    write(
        &project.join("project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        &project.join("task.toml"),
        &task_toml("top", "Invoicing top", "other", "open", ""),
    );
    let group = project.join("tasks/billing");
    write(
        &group.join("task.toml"),
        &task_toml("grp", "Billing", "other", "open", ""),
    );
    write(
        &group.join("tasks/schema/task.toml"),
        &task_toml("schema", "Create the events table", "pr-ship", "done", ""),
    );
    write(
        &group.join("tasks/review/task.toml"),
        &task_toml("review", "Review the schema", "other", "open", ""),
    );
    let backfill = group.join("tasks/backfill");
    write(
        &backfill.join("task.toml"),
        &task_toml(
            "leaf",
            "Backfill usage events",
            "pr-ship",
            "open",
            "[[after]]\ntask = \"schema\"\nunblock = \"merged\"\n\n\
             [[after]]\ntask = \"review\"\nunblock = \"review\"\n\n\
             [[after]]\ntask = \"schema\"\nunblock = \"review\"\n\n\
             [result]\nkind = \"prs\"\nurl = \"https://example.com/pull/7\"\n\
             urls = [\"https://example.com/pull/7\", \"https://example.com/pull/8\"]\n",
        ),
    );
    write(&backfill.join(SECRET_FILE), "private\n");
    write(&backfill.join("notes").join(SECRET_NOTE), "private\n");
    write(
        &backfill.join("artifacts.toml"),
        &format!(
            "schema = 1\n\n[[artifact]]\npath = \"{SECRET_FILE}\"\n\
             description = \"{SECRET_DESCRIPTION}\"\nvisibility = \"human-only\"\n\n\
             [[artifact]]\npath = \"notes/{SECRET_NOTE}\"\n\
             description = \"{SECRET_DESCRIPTION}\"\nvisibility = \"human-only\"\n"
        ),
    );
    write(
        &root.join("oneoffs/misc/2020-01-02-fix-typo/task.toml"),
        &task_toml(
            "oneoff",
            "Fix the typo",
            "pr-ship",
            "open",
            "[result]\nkind = \"pr\"\nurl = \"https://example.com/pull/9\"\n",
        ),
    );
    let tree = load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    Tree {
        _temp: temp,
        root,
        tree,
    }
}

#[test]
fn test_facts_of_a_leaf_in_a_group_in_a_project() {
    let t = tree();
    let facts = task_facts(&t.tree, "leaf").unwrap();
    assert_eq!(facts.project_title.as_deref(), Some("Invoicing"));
    assert_eq!(facts.project_dir, Some(t.root.join("projects/invoicing")));
    assert_eq!(
        facts.notes_dir,
        Some(
            t.root
                .join("projects/invoicing/tasks/billing/tasks/backfill/notes")
        )
    );
    // Each dependency once, in file order, with the state label the Work view shows.
    assert_eq!(
        facts.deps,
        vec![
            ("Create the events table".to_string(), "Done".to_string()),
            ("Review the schema".to_string(), "Ready".to_string()),
        ]
    );
    // The `pr` link is not repeated between `url` and `urls`.
    assert_eq!(
        facts.pr_links,
        vec![
            "https://example.com/pull/7".to_string(),
            "https://example.com/pull/8".to_string()
        ]
    );
}

#[test]
fn test_facts_of_a_project_top_task() {
    let t = tree();
    assert!(is_project_top(&t.tree, "top"));
    assert!(!is_project_top(&t.tree, "leaf"));
    assert!(!is_project_top(&t.tree, "grp"));
    assert!(!is_project_top(&t.tree, "oneoff"));
    assert!(!is_project_top(&t.tree, "nope"));

    let facts = task_facts(&t.tree, "top").unwrap();
    assert_eq!(facts.project_title.as_deref(), Some("Invoicing"));
    assert_eq!(facts.project_dir, Some(t.root.join("projects/invoicing")));
    assert_eq!(
        facts.notes_dir,
        Some(t.root.join("projects/invoicing/notes"))
    );
    assert!(facts.deps.is_empty());
    assert!(facts.pr_links.is_empty());
}

#[test]
fn test_facts_of_a_one_off_have_no_project() {
    let t = tree();
    let facts = task_facts(&t.tree, "oneoff").unwrap();
    assert_eq!(facts.project_title, None);
    assert_eq!(facts.project_dir, None);
    assert_eq!(
        facts.notes_dir,
        Some(t.root.join("oneoffs/misc/2020-01-02-fix-typo/notes"))
    );
    assert!(facts.deps.is_empty());
    assert_eq!(
        facts.pr_links,
        vec!["https://example.com/pull/9".to_string()]
    );
    assert_eq!(task_facts(&t.tree, "missing"), None);
}

/// The promise: nothing human-only reaches `TaskFacts`. A task with human-only artifacts in its
/// root and in `notes/` yields facts that name neither file nor describe them; `notes_dir` is a
/// directory, never a listing.
#[test]
fn test_facts_hold_no_artifact_path() {
    let t = tree();
    for id in ["leaf", "grp", "top", "schema", "review", "oneoff"] {
        let facts = task_facts(&t.tree, id).unwrap();
        let text = format!("{facts:?}");
        for secret in [
            SECRET_FILE,
            SECRET_NOTE,
            SECRET_DESCRIPTION,
            "artifacts.toml",
        ] {
            assert!(!text.contains(secret), "{id}: {secret} in {text}");
        }
        if let Some(notes) = &facts.notes_dir {
            assert_eq!(notes.file_name().unwrap(), "notes", "{id}");
        }
    }
}
