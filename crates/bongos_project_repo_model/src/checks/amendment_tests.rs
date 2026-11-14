use std::collections::BTreeSet;

use crate::checks::{AmendOutcome, AmendmentInput, amendment_bounds, bound_amendment, split_goal};
use crate::test_trees::{PROJECT, after, todo, tree_of_files, tree_of_files_with_project};
use crate::{DefinitionTree, RelPath, summarize};

/// A full `task.toml`: id, goal, then `extra` lines.
fn text(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n{extra}"
    )
}

const ADDED_KEYS: &str = "work_kind = \"code\"\nrepositories = [\"app\"]\nmerge_is_ship = true\nsize = 8\nchecks = [\"lint\"]\n";

fn added(todos: &str) -> String {
    text("added", "added", &format!("{ADDED_KEYS}{todos}"))
}

fn x_todos() -> String {
    format!("{}{}", todo("x1", "one"), todo("x2", "two"))
}

/// Master: `existing`, `redef` and `outside`.
fn master_files() -> Vec<(&'static str, String)> {
    vec![
        (
            "tasks/existing",
            text(
                "existing",
                "existing",
                &format!("{}{}", todo("e1", "a"), todo("e2", "b")),
            ),
        ),
        ("tasks/redef", text("redef", "redef", "")),
        ("tasks/outside", text("outside", "outside", "")),
    ]
}

/// The author's part adds `added` (two todos) and `sib` after it, and redefines `redef`.
fn author_files() -> Vec<(&'static str, String)> {
    vec![
        (
            "tasks/existing",
            text(
                "existing",
                "existing",
                &format!("{}{}", todo("e1", "a"), todo("e2", "b")),
            ),
        ),
        ("tasks/redef", text("redef", "redef", "areas = [\"ui\"]\n")),
        ("tasks/outside", text("outside", "outside", "")),
        ("tasks/added", added(&x_todos())),
        ("tasks/sib", text("sib", "sib", &after(&["added"]))),
    ]
}

/// Replaces or adds the file of `folder`; `None` removes it.
fn with(
    mut files: Vec<(&'static str, String)>,
    folder: &'static str,
    file: Option<String>,
) -> Vec<(&'static str, String)> {
    files.retain(|(f, _)| *f != folder);
    if let Some(file) = file {
        files.push((folder, file));
    }
    files
}

struct Case {
    master: DefinitionTree,
    author: DefinitionTree,
    writable: DefinitionTree,
    extra_paths: Vec<String>,
    started: BTreeSet<String>,
}

impl Case {
    fn new(writable: Vec<(&'static str, String)>) -> Self {
        Self {
            master: tree_of_files(&master_files()),
            author: tree_of_files(&author_files()),
            writable: tree_of_files(&writable),
            extra_paths: Vec::new(),
            started: BTreeSet::new(),
        }
    }

    fn bounds(&self) -> Result<(), String> {
        let author_part = summarize(&self.master, &self.author);
        let mut changed: Vec<String> = summarize(&self.author, &self.writable)
            .files
            .into_iter()
            .map(|(path, _)| format!("definitions/{path}"))
            .collect();
        changed.extend(self.extra_paths.iter().cloned());
        let stamps = [RelPath::new("rebalance.toml").unwrap()];
        let input = AmendmentInput {
            author: &self.author,
            writable: &self.writable,
            author_part: &author_part,
            changed_paths: &changed,
            stamp_paths: &stamps,
            started: &self.started,
        };
        let result = amendment_bounds(&input);
        // The engine never commits an amendment outside its bounds.
        let outcome = bound_amendment(
            AmendOutcome::Amended {
                message: "rebalance".to_string(),
            },
            &input,
        );
        match &result {
            Ok(()) => assert!(matches!(outcome, AmendOutcome::Amended { .. })),
            Err(reason) => assert_eq!(outcome, AmendOutcome::no_verdict(reason.clone())),
        }
        result
    }
}

fn refused(case: Case, what: &str) {
    assert_eq!(
        case.bounds(),
        Err(format!("amendment outside its bounds: {what}"))
    );
}

#[test]
fn edges_tags_size_and_the_stamp_may_change_on_the_authors_part() {
    let writable = with(
        with(
            author_files(),
            "tasks/sib",
            Some(text("sib", "sib", "areas = [\"core\"]\nsize = 2\n")),
        ),
        "tasks/redef",
        Some(text(
            "redef",
            "redef",
            &format!("areas = [\"ui\", \"api\"]\n{}", after(&["outside"])),
        )),
    );
    let mut case = Case::new(writable);
    case.extra_paths
        .push("definitions/rebalance.toml".to_string());
    assert_eq!(case.bounds(), Ok(()));
}

#[test]
fn a_split_of_an_added_task_into_a_sibling_with_a_retargeted_edge_is_accepted() {
    let rest = text(
        "added-rest",
        &split_goal(["two"]),
        &format!(
            "work_kind = \"code\"\nrepositories = [\"app\"]\nmerge_is_ship = true\nsize = 3\nchecks = [\"lint\"]\n{}{}",
            after(&["added"]),
            todo("x2", "two")
        ),
    );
    let writable = with(
        with(
            with(
                author_files(),
                "tasks/added",
                Some(added(&todo("x1", "one"))),
            ),
            "tasks/added-rest",
            Some(rest),
        ),
        "tasks/sib",
        Some(text("sib", "sib", &after(&["added-rest"]))),
    );
    assert_eq!(Case::new(writable).bounds(), Ok(()));
}

fn child(id: &str, todo_id: &str, name: &str, keys: &str) -> String {
    text(id, name, &format!("{keys}{}", todo(todo_id, name)))
}

fn children_split(first_keys: &str, first_goal: &str) -> Vec<(&'static str, String)> {
    let first = text(
        "part-1",
        first_goal,
        &format!("{first_keys}{}", todo("x1", "one")),
    );
    with(
        with(
            with(author_files(), "tasks/added", Some(added(""))),
            "tasks/added/tasks/part-1",
            Some(first),
        ),
        "tasks/added/tasks/part-2",
        Some(child(
            "part-2",
            "x2",
            "two",
            &format!("{ADDED_KEYS}{}", after(&["part-1"])),
        )),
    )
}

#[test]
fn a_split_into_children_carries_the_parents_fields() {
    let writable = children_split(ADDED_KEYS, "one");
    let case = Case::new(writable);
    assert_eq!(case.bounds(), Ok(()));
    let part = case.writable.task("part-2").unwrap();
    assert_eq!(part.repositories, ["app"]);
    assert_eq!(part.goal, "two");

    for (keys, field) in [
        (ADDED_KEYS.replace("code", "docs"), "work_kind"),
        (ADDED_KEYS.replace("[\"app\"]", "[\"web\"]"), "repositories"),
        (ADDED_KEYS.replace("true", "false"), "merge_is_ship"),
        (ADDED_KEYS.replace("lint", "none"), "checks"),
        (format!("repository = \"web\"\n{ADDED_KEYS}"), "repository"),
    ] {
        refused(
            Case::new(children_split(&keys, "one")),
            &format!("tasks/added/tasks/part-1/task.toml: {field}"),
        );
    }
    let kind = children_split(ADDED_KEYS, "one")
        .into_iter()
        .map(|(folder, file)| {
            let file = if folder.ends_with("part-1") {
                file.replace("kind = \"other\"", "kind = \"spike\"")
            } else {
                file
            };
            (folder, file)
        })
        .collect();
    refused(Case::new(kind), "tasks/added/tasks/part-1/task.toml: kind");
    refused(
        Case::new(children_split(ADDED_KEYS, "a free text goal")),
        "tasks/added/tasks/part-1/task.toml: goal",
    );
}

#[test]
fn a_split_that_drops_alters_or_splits_an_existing_task_gives_no_verdict() {
    let dropped = with(
        children_split(ADDED_KEYS, "one"),
        "tasks/added/tasks/part-2",
        None,
    );
    refused(
        Case::new(dropped),
        "tasks/added/task.toml: todo x2 was dropped",
    );

    let altered = with(
        children_split(ADDED_KEYS, "one"),
        "tasks/added/tasks/part-2",
        Some(child("part-2", "x2", "two, reworded", ADDED_KEYS)),
    );
    refused(
        Case::new(altered),
        "tasks/added/tasks/part-2/task.toml: todo x2 was altered",
    );

    let existing = with(
        with(
            author_files(),
            "tasks/existing",
            Some(text("existing", "existing", &todo("e1", "a"))),
        ),
        "tasks/existing/tasks/rest",
        Some(child("rest", "e2", "b", "")),
    );
    refused(
        Case::new(existing),
        "tasks/existing/task.toml: split of a task the change did not add",
    );
}

#[test]
fn anything_outside_the_bounds_gives_no_verdict() {
    let goal = with(
        author_files(),
        "tasks/sib",
        Some(text("sib", "new goal", &after(&["added"]))),
    );
    refused(Case::new(goal), "tasks/sib/task.toml: goal");

    let outside = with(
        author_files(),
        "tasks/outside",
        Some(text("outside", "outside", &after(&["redef"]))),
    );
    refused(Case::new(outside), "tasks/outside/task.toml: after");

    let mut started = Case::new(with(
        author_files(),
        "tasks/redef",
        Some(text("redef", "redef", "areas = [\"api\"]\n")),
    ));
    started.started.insert("redef".to_string());
    refused(started, "tasks/redef/task.toml: areas");

    let mut workflows = Case::new(author_files());
    workflows
        .extra_paths
        .push("definitions/workflows/review/workflow.toml".to_string());
    refused(workflows, "workflows/review/workflow.toml");

    let mut outside_definitions = Case::new(author_files());
    outside_definitions
        .extra_paths
        .push("README.md".to_string());
    refused(outside_definitions, "README.md");

    let mut project = Case::new(author_files());
    project.writable =
        tree_of_files_with_project(&format!("{PROJECT}archived = true\n"), &author_files());
    project
        .extra_paths
        .push("definitions/project.toml".to_string());
    refused(project, "project.toml");
}
