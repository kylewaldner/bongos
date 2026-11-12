//! The journaled multi-file commands: split, move, move to project and make one-off, on disk, and
//! a crash between every pair of their journal steps.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::commands::{Change, Move, Outcome, Plan, Write, apply, default_todos, new_child, plan};
use crate::commands::{NewTask, give_up_todos, make_one_off, move_task, move_to_project, split};
use crate::commands::{free_slug, split_work};
use crate::fixtures::{at, ctx};
use crate::journal::{Journal, run_operation, run_steps, write_journal};
use crate::layout::{self, OP_FILE};
use crate::rules::Place;
use crate::store::{Task, WorkTree, load, reload};
use crate::*;

const CREATED: &str = "2026-11-01T09:00:00Z";

fn task_file(id: &str, goal: &str, kind: Kind) -> TaskFile {
    let mut file = TaskFile::new(goal, kind);
    file.id = id.to_string();
    file.created = at(CREATED);
    file.todos = default_todos(kind);
    file
}

fn put(dir: &Path, file: &TaskFile) {
    fs::create_dir_all(dir).unwrap();
    fs::write(layout::task_file(dir), serialize_task(file).unwrap()).unwrap();
}

fn put_text(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn edge(task: &str) -> Edge {
    Edge {
        task: task.to_string(),
        unblock: UnblockPoint::Merged,
        extra: Extra::new(),
    }
}

fn project(root: &Path, slug: &str, id: &str, repository: &str) -> PathBuf {
    let dir = layout::project_dir(root, slug);
    let mut file = ProjectFile::new(slug);
    file.repositories = vec![repository.to_string()];
    put_text(
        &layout::project_file(&dir),
        &serialize_project(&file).unwrap(),
    );
    let mut top = task_file(id, slug, Kind::Other);
    top.todos.clear();
    put(&dir, &top);
    dir
}

/// The oldest history entries of the one-off `fix-login`, which holds exactly the limit.
fn oneoff_history() -> Vec<HistoryEntry> {
    (0..HISTORY_LIMIT)
        .map(|index| HistoryEntry {
            at: at(CREATED),
            by: "kyle".to_string(),
            event: format!("event {index}"),
            detail: None,
            extra: Extra::new(),
        })
        .collect()
}

/// `invoicing` (repository `invoicing`):
/// - `backfill`: a pr-ship leaf with a worktree, branch, result, `chats/`, `runs/` and `notes/`,
///   after `schema`;
/// - `schema`, after `docs`; `stack`, stacked on `schema`; `docs`, a plain document leaf.
///
/// `billing` (repository `billing`) with the leaf `ledger`; the one-off
/// `oneoffs/support/2026-11-01-fix-login` with a child `repro`, files and full history.
fn fixture(root: &Path) {
    let invoicing = project(root, "invoicing", "p1", "invoicing");
    let mut backfill = task_file("backfill", "Backfill", Kind::PrShip);
    backfill.todos[0].state = TodoState::Done;
    backfill.branch = Some("backfill".to_string());
    backfill.base = Some("main".to_string());
    backfill.base_sha = Some("4be1".to_string());
    backfill.worktree = Some("/src/invoicing/.bongos-worktrees/backfill".to_string());
    backfill.result = Some(TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://github.com/acme/invoicing/pull/412".to_string()),
        state: Some(ResultState::Open),
        urls: Vec::new(),
        extra: Extra::new(),
    });
    backfill.after = vec![edge("schema")];
    let backfill_dir = layout::child_dir(&invoicing, "backfill");
    put(&backfill_dir, &backfill);
    put_text(&backfill_dir.join("chats/one.md"), "chat one");
    put_text(&backfill_dir.join("runs/r1.log"), "run one");
    put_text(&backfill_dir.join("notes/plan.md"), "plan");

    let mut schema = task_file("schema", "Schema", Kind::PrShip);
    schema.after = vec![edge("docs")];
    put(&layout::child_dir(&invoicing, "schema"), &schema);
    let mut stack = task_file("stack", "Stack", Kind::PrShip);
    stack.stacked_on = Some(StackedOn {
        task: "schema".to_string(),
        unblock: UnblockPoint::Review,
        extra: Extra::new(),
    });
    put(&layout::child_dir(&invoicing, "stack"), &stack);
    put(
        &layout::child_dir(&invoicing, "docs"),
        &task_file("docs", "Docs", Kind::Document),
    );

    let billing = project(root, "billing", "p2", "billing");
    put(
        &layout::child_dir(&billing, "ledger"),
        &task_file("ledger", "Ledger", Kind::Other),
    );

    let oneoff = layout::oneoff_dir(root, "support", "2026-11-01-fix-login");
    let mut fix = task_file("fix-login", "Fix login", Kind::Spike);
    fix.activity = Some("support".to_string());
    fix.history = oneoff_history();
    put(&oneoff, &fix);
    put_text(&oneoff.join("chats/c.md"), "chat");
    put_text(&oneoff.join("notes/n.md"), "note");
    put_text(
        &layout::artifacts_file(&oneoff),
        "schema = 1\n[[artifact]]\npath = \"notes/n.md\"\ndescription = \"what I found\"\n",
    );
    put(
        &layout::child_dir(&oneoff, "repro"),
        &task_file("repro", "Repro", Kind::Other),
    );
}

fn loaded(temp: &TempDir) -> WorkTree {
    fixture(temp.path());
    let tree = load(temp.path());
    assert_clean(&tree);
    tree
}

fn assert_clean(tree: &WorkTree) {
    for task in tree.tasks() {
        assert!(task.errors.is_empty(), "{}: {:?}", task.slug, task.errors);
    }
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
}

fn accept(root: &Path, tree: &WorkTree, outcome: Outcome) -> WorkTree {
    let change = outcome.expect("the command should be accepted");
    apply(root, &change).unwrap();
    assert!(
        no_journal_left(root),
        "a journal was left after {}",
        change.entry.event
    );
    let tree = reload(root, tree);
    assert_clean(&tree);
    tree
}

fn no_journal_left(root: &Path) -> bool {
    files_under(root)
        .keys()
        .all(|path| !path.ends_with(OP_FILE))
}

fn by_id<'a>(tree: &'a WorkTree, id: &str) -> &'a Task {
    tree.task_by_id(id)
        .unwrap_or_else(|| panic!("no task {id}"))
}

fn file_of<'a>(tree: &'a WorkTree, id: &str) -> &'a TaskFile {
    by_id(tree, id).file.as_ref().unwrap()
}

fn has_file(task: &Task, path: &str) -> bool {
    task.files.contains_key(Path::new(path))
}

fn events(file: &TaskFile) -> Vec<&str> {
    file.history
        .iter()
        .map(|entry| entry.event.as_str())
        .collect()
}

#[test]
fn split_moves_the_work_into_a_child_and_the_parent_becomes_a_group() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);
    let before = file_of(&tree, "backfill").clone();

    // A leaf with a worktree gains children only by split.
    let refused = new_child(
        &tree,
        &ctx(),
        "backfill",
        NewTask {
            goal: "Part".to_string(),
            ..NewTask::default()
        },
    )
    .unwrap_err();
    assert!(refused.ends_with("use task: split"), "{refused}");

    let tree = accept(
        root,
        &tree,
        split(&tree, &ctx(), "backfill", Some("Implement the backfill")),
    );
    let parent = by_id(&tree, "backfill");
    assert_eq!(parent.children.len(), 1);
    assert_eq!(derive::shape(parent, &tree), derive::Shape::Group);
    let child = &parent.children[0];
    assert_eq!(child.slug, "implement-the-backfill");
    assert_eq!(child.dir, parent.dir.join("tasks/implement-the-backfill"));

    let parent_file = parent.file.as_ref().unwrap();
    assert!(parent_file.todos.is_empty());
    assert_eq!(parent_file.worktree, None);
    assert_eq!(parent_file.branch, None);
    assert_eq!(parent_file.result, None);
    assert_eq!(parent_file.base_sha, None);
    // The parent keeps its id, goal, edges and history.
    assert_eq!(parent_file.goal, "Backfill");
    assert_eq!(parent_file.after, before.after);
    let entry = parent_file.history.last().unwrap();
    assert_eq!(entry.event, "split");
    assert_eq!(
        entry.detail.as_deref(),
        Some(
            "implement-the-backfill; moved: todos, result, worktree, branch, base, base sha, \
             chats/, runs/"
        )
    );

    let child_file = child.file.as_ref().unwrap();
    assert_eq!(child_file.goal, "Implement the backfill");
    assert_eq!(child_file.kind, Kind::PrShip);
    assert_eq!(child_file.todos, before.todos);
    assert_eq!(child_file.worktree, before.worktree);
    assert_eq!(child_file.branch, before.branch);
    assert_eq!(child_file.base, before.base);
    assert_eq!(child_file.base_sha, before.base_sha);
    assert_eq!(child_file.result, before.result);
    assert_ne!(child_file.id, "backfill");
    assert_eq!(events(child_file), ["created"]);
    assert_eq!(
        child_file.history[0].detail.as_deref(),
        Some("split from backfill")
    );

    assert!(has_file(child, "chats/one.md"));
    assert!(has_file(child, "runs/r1.log"));
    assert!(!has_file(parent, "chats/one.md"));
    assert!(!has_file(parent, "runs/r1.log"));
    assert!(has_file(parent, "notes/plan.md"));

    // Now a group: split is refused and a new child is accepted.
    assert_eq!(
        split(&tree, &ctx(), "backfill", None).unwrap_err(),
        "backfill is already a group; use task: new child"
    );
    let new = NewTask {
        goal: "Verify".to_string(),
        ..NewTask::default()
    };
    accept(root, &tree, new_child(&tree, &ctx(), "backfill", new));
}

#[test]
fn split_work_moves_the_planned_result_to_the_keeper() {
    let temp = TempDir::new().unwrap();
    let tree = loaded(&temp);
    let task = by_id(&tree, "backfill");
    let mut parent = file_of(&tree, "backfill").clone();
    parent.planned_result = Some("a PR that backfills".to_string());
    let before = parent.clone();
    let mut keeper = TaskFile::new("Keeper", Kind::PrShip);
    let child_dir = layout::child_dir(&task.dir, "keeper");

    let (moved, moves) = split_work(&mut parent, &mut keeper, task, &child_dir);
    assert_eq!(
        moved,
        [
            "todos",
            "result",
            "planned result",
            "worktree",
            "branch",
            "base",
            "base sha",
            "chats/",
            "runs/"
        ]
    );
    assert_eq!(parent.planned_result, None);
    assert_eq!(keeper.planned_result, before.planned_result);
    assert_eq!(keeper.todos, before.todos);
    assert_eq!(keeper.worktree, before.worktree);
    assert!(parent.todos.is_empty() && parent.result.is_none());
    assert_eq!(
        moves,
        [
            Move {
                from: task.dir.join("chats"),
                to: child_dir.join("chats")
            },
            Move {
                from: task.dir.join("runs"),
                to: child_dir.join("runs")
            },
        ]
    );
}

#[test]
fn split_notes_a_moved_planned_result_in_the_history() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fixture(root);
    let docs_dir = layout::child_dir(&layout::project_dir(root, "invoicing"), "docs");
    let mut docs = task_file("docs", "Docs", Kind::Document);
    docs.planned_result = Some("a design document".to_string());
    put(&docs_dir, &docs);
    let tree = load(root);
    let tree = accept(root, &tree, split(&tree, &ctx(), "docs", Some("Write")));
    let parent = file_of(&tree, "docs");
    assert_eq!(parent.planned_result, None);
    assert_eq!(
        parent.history.last().unwrap().detail.as_deref(),
        Some("write; moved: todos, planned result")
    );
    let child = &by_id(&tree, "docs").children[0];
    assert_eq!(
        child.file.as_ref().unwrap().planned_result.as_deref(),
        Some("a design document")
    );
}

#[test]
fn giving_up_todos_clears_the_planned_result_and_says_so() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fixture(root);
    let docs_dir = layout::child_dir(&layout::project_dir(root, "invoicing"), "docs");
    let mut docs = task_file("docs", "Docs", Kind::Document);
    docs.planned_result = Some("a design document".to_string());
    put(&docs_dir, &docs);
    let tree = load(root);

    let place = Place::locate(&tree, "docs").unwrap();
    let mut file = file_of(&tree, "docs").clone();
    assert!(give_up_todos(&ctx(), &place, &tree, &mut file, "part"));
    assert!(file.todos.is_empty());
    assert_eq!(file.planned_result, None);
    assert_eq!(
        file.history.last().unwrap().detail.as_deref(),
        Some("part; became a group, todos removed: draft, review, publish, planned result removed")
    );

    // With no todos left, a planned result alone is still given up.
    let mut file = file_of(&tree, "docs").clone();
    file.todos.clear();
    assert!(give_up_todos(&ctx(), &place, &tree, &mut file, "part"));
    assert_eq!(file.planned_result, None);
    assert_eq!(
        file.history.last().unwrap().detail.as_deref(),
        Some("part; became a group, planned result removed")
    );

    // Nothing to give up: unchanged.
    let mut file = file_of(&tree, "docs").clone();
    file.todos.clear();
    file.planned_result = None;
    assert!(!give_up_todos(&ctx(), &place, &tree, &mut file, "part"));

    // Through new_child, the same note lands on the parent's history.
    let tree = accept(
        root,
        &tree,
        new_child(
            &tree,
            &ctx(),
            "docs",
            NewTask {
                goal: "Part".to_string(),
                ..NewTask::default()
            },
        ),
    );
    let parent = file_of(&tree, "docs");
    assert_eq!(parent.planned_result, None);
    assert!(
        parent
            .history
            .last()
            .unwrap()
            .detail
            .as_deref()
            .unwrap()
            .ends_with("planned result removed")
    );
}

#[test]
fn new_task_todos_override_the_kind_defaults() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);
    let plan = Todo {
        id: None,
        name: "plan".to_string(),
        role: Some(TodoRole::Plans),
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    };
    let new = |todos| NewTask {
        goal: "Verify".to_string(),
        todos,
        ..NewTask::default()
    };
    let tree = accept(
        root,
        &tree,
        new_child(&tree, &ctx(), "p1", new(Some(vec![plan.clone()]))),
    );
    let verify = tree
        .tasks()
        .into_iter()
        .find(|task| task.slug == "verify")
        .unwrap();
    assert_eq!(
        verify.file.as_ref().unwrap().todos,
        std::slice::from_ref(&plan)
    );

    // Unset, the kind's defaults apply, as before.
    let tree = accept(
        root,
        &tree,
        new_child(
            &tree,
            &ctx(),
            "p1",
            NewTask {
                goal: "Other".to_string(),
                ..NewTask::default()
            },
        ),
    );
    let other = tree
        .tasks()
        .into_iter()
        .find(|task| task.slug == "other")
        .unwrap();
    assert_eq!(
        other.file.as_ref().unwrap().todos,
        default_todos(Kind::PrShip)
    );

    // One-offs use it too.
    let change =
        crate::commands::new_oneoff(&tree, &ctx(), "support", new(Some(vec![plan.clone()])))
            .unwrap();
    match &change.writes[0] {
        Write::Task { file, .. } => assert_eq!(file.todos, [plan]),
        other => panic!("unexpected write {other:?}"),
    }
}

#[test]
fn free_slug_skips_taken_and_reserved_names() {
    assert_eq!(free_slug("part", &[]), "part");
    assert_eq!(free_slug("part", &["part"]), "part-2");
    assert_ne!(free_slug("runs", &[]), "runs");
}

#[test]
fn split_refusals() {
    let temp = TempDir::new().unwrap();
    let tree = loaded(&temp);
    assert_eq!(
        split(&tree, &ctx(), "stack", None).unwrap_err(),
        "stack is stacked on schema, and a group cannot be stacked; remove that edge first"
    );
    assert_eq!(
        split(&tree, &ctx(), "schema", None).unwrap_err(),
        "stack is stacked on schema, and a group cannot be stacked; remove that edge first"
    );
    assert_eq!(
        split(&tree, &ctx(), "p1", None).unwrap_err(),
        "invoicing is already a group; use task: new child"
    );
    assert_eq!(
        split(&tree, &ctx(), "docs", Some(" ")).unwrap_err(),
        "a task needs a goal"
    );
    // Unnamed, the child takes the parent's goal.
    let change = split(&tree, &ctx(), "docs", None).unwrap();
    assert!(
        change.writes[1]
            .path()
            .ends_with("docs/tasks/docs/task.toml")
    );
}

#[test]
fn move_removes_edges() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);

    let tree = accept(root, &tree, move_task(&tree, &ctx(), "schema", "docs"));
    let schema = by_id(&tree, "schema");
    assert_eq!(
        schema.dir,
        layout::project_dir(root, "invoicing").join("tasks/docs/tasks/schema")
    );
    let schema_file = schema.file.as_ref().unwrap();
    assert!(schema_file.after.is_empty());
    let entry = schema_file.history.last().unwrap();
    assert_eq!(entry.event, "moved");
    assert_eq!(
        entry.detail.as_deref(),
        Some(
            "projects/invoicing/tasks/schema -> projects/invoicing/tasks/docs/tasks/schema; \
             edges removed: docs, backfill, stack"
        )
    );
    // The old siblings' edges to it are gone, with a history entry each.
    let backfill = file_of(&tree, "backfill");
    assert!(backfill.after.is_empty());
    assert_eq!(events(backfill), ["edge removed"]);
    let stack = file_of(&tree, "stack");
    assert_eq!(stack.stacked_on, None);
    assert_eq!(
        stack.history.last().unwrap().detail.as_deref(),
        Some("schema moved away")
    );
    // The plain leaf it went under became a group and gave up its todos.
    let docs = by_id(&tree, "docs");
    assert_eq!(docs.children.len(), 1);
    let docs_file = docs.file.as_ref().unwrap();
    assert!(docs_file.todos.is_empty());
    assert_eq!(
        docs_file.history.last().unwrap().detail.as_deref(),
        Some("schema; became a group, todos removed: draft, review, publish")
    );
    assert_eq!(tree.tasks().len(), 9);
}

#[test]
fn move_refusals_and_repository() {
    let temp = TempDir::new().unwrap();
    let tree = loaded(&temp);
    assert_eq!(
        move_task(&tree, &ctx(), "schema", "backfill").unwrap_err(),
        "backfill is a leaf with a worktree, a branch and a result; use task: split"
    );
    assert_eq!(
        move_task(&tree, &ctx(), "fix-login", "repro").unwrap_err(),
        "2026-11-01-fix-login cannot move under itself or one of its own tasks"
    );
    assert_eq!(
        move_task(&tree, &ctx(), "schema", "p1").unwrap_err(),
        "schema is already under invoicing"
    );
    assert_eq!(
        move_to_project(&tree, &ctx(), "schema", "invoicing").unwrap_err(),
        "schema is already under invoicing"
    );
    assert_eq!(
        move_task(&tree, &ctx(), "p1", "p2").unwrap_err(),
        "invoicing is a project; only its tasks can move"
    );
    assert_eq!(
        move_to_project(&tree, &ctx(), "schema", "nowhere").unwrap_err(),
        "no project is named nowhere"
    );
    assert_eq!(
        make_one_off(&tree, &ctx(), "fix-login", "support").unwrap_err(),
        "2026-11-01-fix-login is already a one-off"
    );
    assert_eq!(
        make_one_off(&tree, &ctx(), "schema", " ").unwrap_err(),
        "a one-off needs an activity"
    );

    // A task with a worktree keeps the repository it inherited.
    let change = move_to_project(&tree, &ctx(), "backfill", "billing").unwrap();
    let moved = change.file().unwrap();
    assert_eq!(moved.repository.as_deref(), Some("invoicing"));
    assert!(
        change
            .entry
            .detail
            .as_deref()
            .unwrap()
            .ends_with("repository kept: invoicing")
    );
    // A plain task takes the new project's.
    let change = move_to_project(&tree, &ctx(), "docs", "billing").unwrap();
    assert_eq!(change.file().unwrap().repository, None);
}

/// The history of the task with `id`, `history.old.toml` first.
fn full_history(tree: &WorkTree, id: &str) -> Vec<HistoryEntry> {
    let task = by_id(tree, id);
    let old_path = layout::history_old_file(&task.dir);
    let mut history = if old_path.exists() {
        read_history(&old_path).unwrap().history
    } else {
        Vec::new()
    };
    history.extend(task.file.as_ref().unwrap().history.iter().cloned());
    history
}

#[test]
fn move_to_project_and_back_keeps_files_and_history() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);
    let original = by_id(&tree, "fix-login").dir.clone();
    let files: Vec<PathBuf> = by_id(&tree, "fix-login").files.keys().cloned().collect();
    assert_eq!(
        files,
        [
            PathBuf::from("artifacts.toml"),
            "chats/c.md".into(),
            "notes/n.md".into(),
            "task.toml".into()
        ]
    );

    let tree = accept(
        root,
        &tree,
        move_to_project(&tree, &ctx(), "fix-login", "billing"),
    );
    let moved = by_id(&tree, "fix-login");
    assert_eq!(
        moved.dir,
        layout::project_dir(root, "billing").join("tasks/fix-login")
    );
    assert!(!original.exists());
    let moved_files: Vec<PathBuf> = moved
        .files
        .keys()
        .filter(|path| **path != Path::new(layout::HISTORY_OLD_FILE))
        .cloned()
        .collect();
    assert_eq!(moved_files, files);
    assert_eq!(moved.artifacts.as_ref().unwrap().artifacts.len(), 1);
    assert_eq!(moved.children[0].id(), Some("repro"));
    assert_eq!(moved.file.as_ref().unwrap().activity, None);
    // Past the limit, the oldest entry went to `history.old.toml`.
    assert_eq!(moved.file.as_ref().unwrap().history.len(), HISTORY_LIMIT);
    let history = full_history(&tree, "fix-login");
    assert_eq!(history[..HISTORY_LIMIT], oneoff_history()[..]);
    assert_eq!(history[HISTORY_LIMIT].event, "moved to project");

    let tree = accept(
        root,
        &tree,
        make_one_off(&tree, &ctx(), "fix-login", "support"),
    );
    let back = by_id(&tree, "fix-login");
    assert_eq!(back.dir, original);
    let back_files: Vec<PathBuf> = back
        .files
        .keys()
        .filter(|path| **path != Path::new(layout::HISTORY_OLD_FILE))
        .cloned()
        .collect();
    assert_eq!(back_files, files);
    assert_eq!(back.children[0].id(), Some("repro"));
    assert_eq!(
        back.file.as_ref().unwrap().activity.as_deref(),
        Some("support")
    );
    let history = full_history(&tree, "fix-login");
    assert_eq!(history.len(), HISTORY_LIMIT + 2);
    assert_eq!(history[..HISTORY_LIMIT], oneoff_history()[..]);
    assert_eq!(history[HISTORY_LIMIT + 1].event, "made one-off");
    let old = read_history(&layout::history_old_file(&back.dir)).unwrap();
    assert_eq!(old.history.len(), 2);
    // The billing project is as it was.
    let billing: Vec<&str> = tree.projects[0]
        .task
        .children
        .iter()
        .map(|task| task.slug.as_str())
        .collect();
    assert_eq!(billing, ["ledger"]);
}

/// Every file under `root` but journals, with its content.
fn files_under(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                files.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files_under(root);
    files.retain(|path, _| !path.ends_with(OP_FILE));
    files
}

/// The ids in the tree, each with how many tasks carry it.
fn id_counts(tree: &WorkTree) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for task in tree.tasks() {
        *counts
            .entry(task.id().expect("every task parsed").to_string())
            .or_insert(0) += 1;
    }
    counts
}

/// The journal `command` writes on the fixture, and the directory it goes in, relative to the root.
fn journal_of(command: &dyn Fn(&WorkTree) -> Outcome) -> (PathBuf, Journal) {
    let temp = TempDir::new().unwrap();
    let tree = loaded(&temp);
    let change: Change = command(&tree).unwrap();
    match plan(temp.path(), &change).unwrap() {
        Plan::Journal { dir, journal } => (dir.strip_prefix(temp.path()).unwrap().into(), journal),
        Plan::One { .. } => panic!("{} wrote one file", change.entry.event),
    }
}

#[test]
fn a_kill_between_any_two_journal_steps_loses_and_duplicates_no_task() {
    let commands: [(&str, Box<dyn Fn(&WorkTree) -> Outcome>); 5] = [
        (
            "split",
            Box::new(|tree| split(tree, &ctx(), "backfill", None)),
        ),
        (
            "move",
            Box::new(|tree| move_task(tree, &ctx(), "schema", "docs")),
        ),
        (
            "move to project",
            Box::new(|tree| move_to_project(tree, &ctx(), "fix-login", "billing")),
        ),
        (
            "move to project from a project",
            Box::new(|tree| move_to_project(tree, &ctx(), "backfill", "billing")),
        ),
        (
            "make one-off",
            Box::new(|tree| make_one_off(tree, &ctx(), "schema", "support")),
        ),
    ];
    for (name, command) in &commands {
        let (dir, journal) = journal_of(command.as_ref());
        assert!(journal.steps.len() >= 3, "{name}: {:?}", journal.steps);

        let reference = TempDir::new().unwrap();
        fixture(reference.path());
        run_operation(reference.path(), &reference.path().join(&dir), &journal).unwrap();
        let expected_tree = load(reference.path());
        assert_clean(&expected_tree);
        let expected_ids = id_counts(&expected_tree);
        assert!(expected_ids.values().all(|count| *count == 1), "{name}");
        let expected = snapshot(reference.path());

        // 0 is a journal written and no step run; `len` is every step run but the journal kept.
        for done in 0..=journal.steps.len() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fixture(root);
            write_journal(root, &root.join(&dir), &journal).unwrap();
            run_steps(root, &journal, done).unwrap();

            let tree = load(root);
            assert!(no_journal_left(root), "{name}: journal left after {done}");
            assert_clean(&tree);
            assert_eq!(id_counts(&tree), expected_ids, "{name} after {done} steps");
            assert_eq!(snapshot(root), expected, "{name} after {done} steps");
        }
    }
}

#[test]
fn a_move_never_lands_on_an_existing_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);
    let change = move_task(&tree, &ctx(), "schema", "docs").unwrap();
    // Something appeared at the target after the tree was loaded.
    fs::create_dir_all(&change.dir).unwrap();
    let before = snapshot(root);
    apply(root, &change).unwrap_err();
    assert_eq!(snapshot(root), before);
    assert!(no_journal_left(root));
}

#[test]
fn a_moved_group_keeps_the_repository_of_a_nested_worktree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fixture(root);
    // `area` and its child `piece` inherit `invoicing`; `piece` has a worktree there.
    let area = layout::child_dir(&layout::project_dir(root, "invoicing"), "area");
    let mut area_file = task_file("area", "Area", Kind::Other);
    area_file.todos.clear();
    put(&area, &area_file);
    let mut piece = task_file("piece", "Piece", Kind::PrShip);
    piece.worktree = Some("/src/invoicing/.bongos-worktrees/piece".to_string());
    put(&layout::child_dir(&area, "piece"), &piece);
    let tree = load(root);
    assert_clean(&tree);

    let tree = accept(
        root,
        &tree,
        move_to_project(&tree, &ctx(), "area", "billing"),
    );
    let moved = by_id(&tree, "area");
    assert_eq!(
        moved.dir,
        layout::project_dir(root, "billing").join("tasks/area")
    );
    assert_eq!(
        file_of(&tree, "area").repository.as_deref(),
        Some("invoicing")
    );
    let place = Place::locate(&tree, "piece").unwrap();
    assert_eq!(
        place.repository(file_of(&tree, "piece")).as_deref(),
        Some("invoicing")
    );
}

#[test]
fn an_unfinished_journal_is_never_replaced() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = loaded(&temp);
    let change = move_task(&tree, &ctx(), "schema", "docs").unwrap();
    // A journal that could not finish sits in the source parent.
    let stuck = layout::op_file(&layout::children_dir(&layout::project_dir(
        root,
        "invoicing",
    )));
    put_text(&stuck, "not a journal");
    let before = files_under(root);
    let error = apply(root, &change).unwrap_err().to_string();
    assert!(error.contains("holds an unfinished operation"), "{error}");
    assert_eq!(files_under(root), before);
}
