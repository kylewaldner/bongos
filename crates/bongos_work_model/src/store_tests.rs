use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::journal::{Journal, Step, finish_journal, run_steps, write_journal};
use crate::layout::{self, OP_FILE};
use crate::store::{WorkTree, load, reload};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn task_toml(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2026-11-08T10:00:00Z\n{extra}"
    )
}

fn project_toml(title: &str) -> String {
    format!("schema = 1\ntitle = \"{title}\"\nrepositories = [\"invoicing\"]\n")
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

fn expect_ids(tree: &WorkTree, ids: &[&str]) {
    let expected: BTreeMap<String, usize> = ids.iter().map(|id| (id.to_string(), 1)).collect();
    assert_eq!(id_counts(tree), expected);
}

#[test]
fn nested_tree_loads_projects_tasks_and_oneoffs() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", ""),
    );
    write(&layout::plan_file(&project), "# plan\n");
    let backfill = layout::child_dir(&project, "backfill");
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Backfill", ""),
    );
    write(&backfill.join("chats/one.md"), "chat");
    write(&backfill.join("runs/run.log"), "log");
    write(&backfill.join("notes/n.md"), "note");
    write(&backfill.join(".task.toml.tmp-1-abcd"), "partial");
    let deep = layout::child_dir(&backfill, "deep");
    write(&layout::task_file(&deep), &task_toml("l2", "Deep", ""));
    // Neither a hidden nor a temporary directory under `tasks/` is a task.
    fs::create_dir_all(layout::children_dir(&backfill).join(".hidden")).unwrap();
    fs::create_dir_all(layout::children_dir(&backfill).join(".x.tmp-1-ab")).unwrap();
    fs::create_dir_all(layout::children_dir(&backfill).join("chats")).unwrap();

    let oneoff = layout::oneoff_dir(root, "support", "2026-11-08-fix-login");
    write(
        &layout::task_file(&oneoff),
        &task_toml("o1", "Fix login", "activity = \"support\"\n"),
    );
    let oneoff_child = layout::child_dir(&oneoff, "repro");
    write(
        &layout::task_file(&oneoff_child),
        &task_toml("o2", "Repro", ""),
    );

    let tree = load(root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    expect_ids(&tree, &["g1", "l1", "l2", "o1", "o2"]);

    assert_eq!(tree.projects.len(), 1);
    let project = &tree.projects[0];
    assert_eq!(project.slug, "invoicing");
    assert_eq!(project.file.as_ref().unwrap().title, "Invoicing");
    assert_eq!(project.task.children.len(), 1);
    let backfill = &project.task.children[0];
    assert_eq!(backfill.slug, "backfill");
    assert_eq!(backfill.goal(), "Backfill");
    let child_slugs: Vec<&str> = backfill.children.iter().map(|t| t.slug.as_str()).collect();
    assert_eq!(child_slugs, ["deep"]);
    let files: Vec<&Path> = backfill.files.keys().map(PathBuf::as_path).collect();
    assert_eq!(
        files,
        [
            Path::new("chats/one.md"),
            Path::new("notes/n.md"),
            Path::new("runs/run.log"),
            Path::new("task.toml"),
        ]
    );
    assert!(backfill.newest_mtime().is_some());
    assert!(project.task.files.contains_key(Path::new("project.toml")));
    assert!(project.task.files.contains_key(Path::new("plan.md")));

    assert_eq!(tree.activities.len(), 1);
    let activity = &tree.activities[0];
    assert_eq!(activity.slug, "support");
    assert_eq!(activity.oneoffs[0].slug, "2026-11-08-fix-login");
    assert_eq!(activity.oneoffs[0].children[0].goal(), "Repro");
    for task in tree.tasks() {
        assert!(task.check_writable().is_ok());
    }
}

#[test]
fn empty_root_loads_an_empty_tree() {
    let temp = TempDir::new().unwrap();
    let tree = load(temp.path());
    assert!(tree.projects.is_empty() && tree.activities.is_empty() && tree.errors.is_empty());
}

#[test]
fn broken_task_toml_keeps_the_last_good_parse_and_refuses_writes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", ""),
    );
    let leaf = layout::child_dir(&project, "backfill");
    let leaf_file = layout::task_file(&leaf);
    write(&leaf_file, &task_toml("l1", "Backfill", ""));

    let good = load(root);
    assert!(good.task_by_dir(&leaf).unwrap().check_writable().is_ok());

    let broken = "schema = 1\nid = \"l1\"\ngoal = \"Backfill\nkind = \"pr-ship\"\n";
    fs::write(&leaf_file, broken).unwrap();
    let tree = reload(root, &good);
    let task = tree.task_by_dir(&leaf).unwrap();
    assert_eq!(task.file, good.task_by_dir(&leaf).unwrap().file);
    assert_eq!(task.errors.len(), 1);
    assert_eq!(task.errors[0].path, leaf_file);
    assert_eq!(task.errors[0].line, Some(3));
    assert!(!task.errors[0].reason.is_empty());
    let refused = task.check_writable().unwrap_err();
    assert!(refused.to_string().contains("task.toml:3"), "{refused}");
    // The other tasks are unaffected.
    assert!(tree.task_by_id("g1").unwrap().check_writable().is_ok());
    // Nothing repairs the file.
    assert_eq!(fs::read_to_string(&leaf_file).unwrap(), broken);

    // A second reload while still broken still has the good parse.
    let again = reload(root, &tree);
    assert_eq!(again.task_by_dir(&leaf).unwrap().goal(), "Backfill");
    assert!(again.task_by_dir(&leaf).unwrap().check_writable().is_err());

    // On a first load there is no good parse to keep.
    let fresh = load(root);
    let task = fresh.task_by_dir(&leaf).unwrap();
    assert!(task.file.is_none());
    assert!(task.check_writable().is_err());

    write(&leaf_file, &task_toml("l1", "Backfill renamed", ""));
    let fixed = reload(root, &again);
    let task = fixed.task_by_dir(&leaf).unwrap();
    assert!(task.check_writable().is_ok());
    assert_eq!(task.goal(), "Backfill renamed");
}

#[test]
fn broken_project_and_artifacts_files_badge_the_task() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", ""),
    );
    let good = load(root);

    write(
        &layout::project_file(&project),
        "schema = 2\ntitle = \"x\"\n",
    );
    write(
        &layout::artifacts_file(&project),
        "schema = 1\n[[artifact]\n",
    );
    let tree = reload(root, &good);
    let loaded = &tree.projects[0];
    assert_eq!(loaded.file.as_ref().unwrap().title, "Invoicing");
    let paths: Vec<&Path> = loaded
        .task
        .errors
        .iter()
        .map(|e| e.path.as_path())
        .collect();
    assert_eq!(
        paths,
        [
            layout::artifacts_file(&project).as_path(),
            layout::project_file(&project).as_path(),
        ]
    );
    assert!(loaded.task.check_writable().is_err());
}

#[test]
fn second_stacked_on_is_an_error_with_its_line() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let oneoff = layout::oneoff_dir(root, "support", "2026-11-08-x");
    write(&layout::task_file(&oneoff), &task_toml("o1", "X", ""));
    write(
        &layout::task_file(&layout::child_dir(&oneoff, "a")),
        &task_toml("a", "A", ""),
    );
    let b = layout::child_dir(&oneoff, "b");
    write(
        &layout::task_file(&b),
        &task_toml(
            "b",
            "B",
            "stacked_on = { task = \"a\", unblock = \"review\" }\n\
             stacked_on = { task = \"a\", unblock = \"merged\" }\n",
        ),
    );
    let tree = load(root);
    let task = tree.task_by_dir(&b).unwrap();
    assert_eq!(task.errors.len(), 1, "{:?}", task.errors);
    assert_eq!(task.errors[0].line, Some(8));
    assert!(task.check_writable().is_err());
}

#[test]
fn edge_to_a_missing_sibling_is_a_broken_edge_not_an_error() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "p");
    write(&layout::project_file(&project), &project_toml("P"));
    write(&layout::task_file(&project), &task_toml("g", "P", ""));
    let a = layout::child_dir(&project, "a");
    write(&layout::task_file(&a), &task_toml("a", "A", ""));
    let b = layout::child_dir(&project, "b");
    write(
        &layout::task_file(&b),
        &task_toml(
            "b",
            "B",
            "after = [{ task = \"a\", unblock = \"merged\" }, { task = \"ghost\", unblock = \"merged\" }]\n",
        ),
    );
    let tree = load(root);
    let task = tree.task_by_dir(&b).unwrap();
    assert_eq!(task.broken_edges, ["ghost"]);
    assert!(task.errors.is_empty());
    assert!(task.check_writable().is_ok());

    // The agent creates the sibling a moment later.
    write(
        &layout::task_file(&layout::child_dir(&project, "ghost")),
        &task_toml("ghost", "Ghost", ""),
    );
    let tree = reload(root, &tree);
    assert!(tree.task_by_dir(&b).unwrap().broken_edges.is_empty());
}

#[test]
fn edge_cycle_badges_every_task_on_it_and_keeps_the_good_parse() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "p");
    write(&layout::project_file(&project), &project_toml("P"));
    write(&layout::task_file(&project), &task_toml("g", "P", ""));
    let edge = |to: &str| format!("after = [{{ task = \"{to}\", unblock = \"merged\" }}]\n");
    let a = layout::child_dir(&project, "a");
    let b = layout::child_dir(&project, "b");
    let c = layout::child_dir(&project, "c");
    write(&layout::task_file(&a), &task_toml("a", "A", &edge("b")));
    write(&layout::task_file(&b), &task_toml("b", "B", ""));
    write(&layout::task_file(&c), &task_toml("c", "C", ""));
    let good = load(root);
    assert!(good.tasks().iter().all(|task| task.errors.is_empty()));

    write(
        &layout::task_file(&b),
        &task_toml("b", "B cyclic", &edge("a")),
    );
    let tree = reload(root, &good);
    let task_a = tree.task_by_dir(&a).unwrap();
    let task_b = tree.task_by_dir(&b).unwrap();
    assert_eq!(task_a.errors[0].reason, "cycle: a -> b -> a");
    assert_eq!(task_b.errors[0].reason, "cycle: b -> a -> b");
    assert_eq!(task_b.errors[0].line, Some(7));
    assert_eq!(task_b.goal(), "B");
    assert!(task_a.check_writable().is_err() && task_b.check_writable().is_err());
    assert!(tree.task_by_dir(&c).unwrap().check_writable().is_ok());
}

#[test]
fn duplicate_ids_are_badged() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "p");
    write(&layout::project_file(&project), &project_toml("P"));
    write(&layout::task_file(&project), &task_toml("g", "P", ""));
    for slug in ["a", "copy-of-a"] {
        write(
            &layout::task_file(&layout::child_dir(&project, slug)),
            &task_toml("same", "A", ""),
        );
    }
    let tree = load(root);
    let badged = tree
        .tasks()
        .into_iter()
        .filter(|task| task.check_writable().is_err())
        .count();
    assert_eq!(badged, 2);
}

/// Two projects; `backfill` (with `chats/`) is in `invoicing`, `schema` beside it.
fn journal_fixture(root: &Path) {
    let invoicing = layout::project_dir(root, "invoicing");
    write(
        &layout::project_file(&invoicing),
        &project_toml("Invoicing"),
    );
    write(
        &layout::task_file(&invoicing),
        &task_toml("g1", "Invoicing", ""),
    );
    let backfill = layout::child_dir(&invoicing, "backfill");
    write(
        &layout::task_file(&backfill),
        &task_toml(
            "l1",
            "Backfill",
            "after = [{ task = \"schema\", unblock = \"merged\" }]\n",
        ),
    );
    write(&backfill.join("chats/one.md"), "chat one");
    write(
        &layout::task_file(&layout::child_dir(&invoicing, "schema")),
        &task_toml("l2", "Schema", ""),
    );
    let billing = layout::project_dir(root, "billing");
    write(&layout::project_file(&billing), &project_toml("Billing"));
    write(
        &layout::task_file(&billing),
        &task_toml("g2", "Billing", ""),
    );
}

/// Moves `backfill` to `billing` (dropping its edge), then splits it: a new child `implement`
/// takes its `chats/`.
fn journal_steps() -> Journal {
    let moved = "projects/billing/tasks/backfill";
    Journal::new(
        "move",
        vec!["l1".into(), "l3".into()],
        vec![
            Step::CreateDir {
                path: "projects/billing/tasks".into(),
            },
            Step::Rename {
                from: "projects/invoicing/tasks/backfill".into(),
                to: moved.into(),
            },
            Step::WriteFile {
                path: format!("{moved}/task.toml"),
                content: task_toml("l1", "Backfill", ""),
            },
            Step::CreateDir {
                path: format!("{moved}/tasks/implement"),
            },
            Step::WriteFile {
                path: format!("{moved}/tasks/implement/task.toml"),
                content: task_toml("l3", "Implement", ""),
            },
            Step::Rename {
                from: format!("{moved}/chats"),
                to: format!("{moved}/tasks/implement/chats"),
            },
        ],
    )
}

/// Every file under `root` but journals, with its content.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                walk(root, &path, files);
            } else if entry.file_name() != OP_FILE {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                files.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn assert_final_layout(tree: &WorkTree, root: &Path) {
    expect_ids(tree, &["g1", "g2", "l1", "l2", "l3"]);
    let backfill = tree.task_by_id("l1").unwrap();
    assert_eq!(backfill.dir, root.join("projects/billing/tasks/backfill"));
    assert!(backfill.file.as_ref().unwrap().after.is_empty());
    let implement = tree.task_by_id("l3").unwrap();
    assert_eq!(implement.dir, backfill.dir.join("tasks/implement"));
    assert!(implement.files.contains_key(Path::new("chats/one.md")));
    let invoicing: Vec<&str> = tree.projects[1]
        .task
        .children
        .iter()
        .map(|task| task.slug.as_str())
        .collect();
    assert_eq!(invoicing, ["schema"]);
    for task in tree.tasks() {
        assert!(task.errors.is_empty(), "{:?}", task.errors);
    }
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
}

#[test]
fn journal_left_after_any_step_completes_on_load() {
    let journal = journal_steps();
    let reference = {
        let temp = TempDir::new().unwrap();
        journal_fixture(temp.path());
        let dir = layout::project_dir(temp.path(), "invoicing");
        crate::journal::run_operation(temp.path(), &dir, &journal).unwrap();
        snapshot(temp.path())
    };

    // 0 is a journal with no step done; `len` is every step done but the journal not deleted.
    for done in 0..=journal.steps.len() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        journal_fixture(root);
        let journal_dir = layout::project_dir(root, "invoicing");
        let journal_path = write_journal(root, &journal_dir, &journal).unwrap();
        run_steps(root, &journal, done).unwrap();

        let tree = load(root);
        assert!(!journal_path.exists(), "journal left after {done} steps");
        assert_final_layout(&tree, root);
        assert_eq!(snapshot(root), reference, "after {done} steps");
    }
}

#[test]
fn journal_in_a_nested_tasks_dir_is_found() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    journal_fixture(root);
    let tasks_dir = layout::children_dir(&layout::project_dir(root, "invoicing"));
    let path = write_journal(root, &tasks_dir, &journal_steps()).unwrap();
    let tree = load(root);
    assert!(!path.exists());
    assert_final_layout(&tree, root);
}

#[test]
fn journal_steps_are_idempotent() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    journal_fixture(root);
    let journal = journal_steps();
    run_steps(root, &journal, journal.steps.len()).unwrap();
    let once = snapshot(root);
    run_steps(root, &journal, journal.steps.len()).unwrap();
    assert_eq!(snapshot(root), once);
    // Finishing it from disk a third time changes nothing either.
    let path = write_journal(root, &layout::project_dir(root, "invoicing"), &journal).unwrap();
    finish_journal(root, &path).unwrap();
    assert_eq!(snapshot(root), once);
    assert!(!path.exists());
}

#[test]
fn journal_round_trips_through_toml() {
    let journal = journal_steps();
    let text = crate::journal::serialize_journal(&journal).unwrap();
    assert!(text.starts_with("schema = 1\n"), "{text}");
    assert!(text.contains("action = \"rename\""), "{text}");
    assert_eq!(
        crate::journal::parse_journal(&text, Path::new("/r")).unwrap(),
        journal
    );
}

#[test]
fn malformed_journal_is_left_untouched_and_reported() {
    let cases: Vec<(&str, String)> = vec![
        ("syntax", "schema = 1\noperation = \"move\n".to_string()),
        (
            "schema",
            "schema = 2\noperation = \"move\"\n".to_string(),
        ),
        (
            "escape",
            "schema = 1\noperation = \"move\"\n\n[[step]]\naction = \"create-dir\"\npath = \"../outside\"\n"
                .to_string(),
        ),
        (
            "absolute",
            "schema = 1\noperation = \"move\"\n\n[[step]]\naction = \"rename\"\nfrom = \"/etc\"\nto = \"x\"\n"
                .to_string(),
        ),
        (
            "unknown step",
            "schema = 1\noperation = \"move\"\n\n[[step]]\naction = \"delete-everything\"\n"
                .to_string(),
        ),
        (
            // `link` is a symbolic link to a directory outside the root; see below.
            "symlink",
            "schema = 1\noperation = \"move\"\n\n[[step]]\naction = \"write-file\"\npath = \"projects/invoicing/link/evil.txt\"\ncontent = \"x\"\n"
                .to_string(),
        ),
    ];
    for (name, text) in cases {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        journal_fixture(&root);
        let before = snapshot(&root);
        let invoicing = layout::project_dir(&root, "invoicing");
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, invoicing.join("link")).unwrap();
        let journal_path = layout::op_file(&invoicing);
        write(&journal_path, &text);

        let tree = load(&root);
        assert_eq!(fs::read_to_string(&journal_path).unwrap(), text, "{name}");
        assert_eq!(snapshot(&root), before, "{name}");
        assert!(!temp.path().join("outside").exists(), "{name}");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "{name}");
        expect_ids(&tree, &["g1", "g2", "l1", "l2"]);
        assert_eq!(tree.errors.len(), 1, "{name}");
        assert_eq!(tree.errors[0].path, journal_path, "{name}");
        let owner = tree.task_by_dir(&invoicing).unwrap();
        assert!(owner.check_writable().is_err(), "{name}");
        assert!(tree.task_by_id("g2").unwrap().check_writable().is_ok());
    }
}

#[test]
fn journal_that_cannot_finish_is_left_and_reported() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    journal_fixture(root);
    // Writing into a directory that does not exist fails, and no step after it runs.
    let journal = Journal::new(
        "move",
        vec![],
        vec![Step::WriteFile {
            path: "projects/nowhere/task.toml".into(),
            content: "x".into(),
        }],
    );
    let path = write_journal(root, &layout::project_dir(root, "billing"), &journal).unwrap();
    let tree = load(root);
    assert!(path.exists());
    assert_eq!(tree.errors.len(), 1);
    assert!(tree.task_by_id("g2").unwrap().check_writable().is_err());
    // A second load tries again and still leaves it.
    let tree = reload(root, &tree);
    assert!(path.exists());
    assert_eq!(tree.errors.len(), 1);
}

#[test]
fn rename_with_source_and_target_both_present_fails_the_journal() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    journal_fixture(root);
    let journal = Journal::new(
        "move",
        vec!["l1".into()],
        vec![Step::Rename {
            from: "projects/invoicing/tasks/backfill".into(),
            to: "projects/invoicing/tasks/schema".into(),
        }],
    );
    let before = snapshot(root);
    let invoicing = layout::project_dir(root, "invoicing");
    let path = write_journal(root, &invoicing, &journal).unwrap();
    let tree = load(root);
    assert!(path.exists());
    assert_eq!(snapshot(root), before);
    expect_ids(&tree, &["g1", "g2", "l1", "l2"]);
    assert_eq!(tree.errors.len(), 1);
    assert!(
        tree.errors[0].reason.contains("both exist"),
        "{:?}",
        tree.errors
    );
    assert!(
        tree.task_by_dir(&invoicing)
            .unwrap()
            .check_writable()
            .is_err()
    );
}

#[test]
fn load_records_the_hash_of_each_model_file_as_read() {
    use crate::files::content_hash;

    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "invoicing");
    let project_text = project_toml("Invoicing");
    let task_text = task_toml("g1", "Invoicing", "");
    write(&layout::project_file(&project), &project_text);
    write(&layout::task_file(&project), &task_text);
    let child = layout::child_dir(&project, "backfill");
    // A file that does not parse still has its hash recorded.
    write(&layout::task_file(&child), "not toml [");
    write(&layout::artifacts_file(&child), "schema = 1\n");

    let tree = load(root);
    let top = &tree.projects[0].task;
    assert_eq!(
        top.read_hash(layout::TASK_FILE),
        Some(content_hash(task_text.as_bytes()))
    );
    assert_eq!(
        top.read_hash(layout::PROJECT_FILE),
        Some(content_hash(project_text.as_bytes()))
    );
    assert_eq!(top.read_hash(layout::ARTIFACTS_FILE), None);
    let child = tree.task_by_dir(&child).unwrap();
    assert_eq!(
        child.read_hash(layout::TASK_FILE),
        Some(content_hash(b"not toml ["))
    );
    assert_eq!(
        child.read_hash(layout::ARTIFACTS_FILE),
        Some(content_hash(b"schema = 1\n"))
    );
}

#[test]
fn a_child_slugged_workflows_still_loads_and_a_workflows_directory_is_no_artifact() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", ""),
    );
    write(&project.join("workflows/ship/workflow.toml"), "definition");
    let child = layout::child_dir(&project, layout::WORKFLOWS_DIR);
    write(
        &layout::task_file(&child),
        &task_toml("l1", "Workflows", ""),
    );
    write(&project.join("notes/n.md"), "note");

    let tree = load(root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    expect_ids(&tree, &["g1", "l1"]);
    let project = &tree.projects[0];
    assert_eq!(project.task.children[0].slug, "workflows");
    let artifacts: Vec<String> = project
        .task
        .files
        .keys()
        .filter_map(|path| crate::artifacts::artifact_key(path))
        .collect();
    assert_eq!(artifacts, ["notes/n.md"]);
}
