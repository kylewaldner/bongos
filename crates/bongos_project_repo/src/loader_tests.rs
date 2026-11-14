use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_project_repo_model::DefinitionTree;
use bongos_work_model::{Status, TodoState};
use futures::executor::block_on;
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::test_support::{definition_toml, fixture_definitions, fixture_project, task_with_todos};
use crate::{CommitId, Identity, LoadedProject, MAIN_BRANCH, ProjectRepo, TreeRef};

fn writer(repo: &ProjectRepo) -> &Arc<crate::StateWriter> {
    repo.state().unwrap().writer().unwrap()
}

fn main_commit(repo: &ProjectRepo) -> CommitId {
    CommitId(
        block_on(
            repo.git()
                .rev_parse(&repo.location().repository, MAIN_BRANCH),
        )
        .unwrap(),
    )
}

/// `l1` done with its first todo done and the second skipped.
fn seed_state(repo: &ProjectRepo) {
    let mut record = task_with_todos(&[("t1", TodoState::Done), ("t2", TodoState::Skipped)]);
    record.status = Status::Done;
    record.result = Some(bongos_work_model::TaskResult {
        kind: bongos_work_model::ResultKind::Link,
        url: Some("https://example.com".to_string()),
        state: None,
        urls: Vec::new(),
        extra: Default::default(),
    });
    writer(repo)
        .transact(|tx| tx.set_task("l1", record.clone()))
        .unwrap();
}

/// Commits `files` (relative to `definitions/`) on a new branch in a separate worktree, the way a
/// working copy does, and returns the commit. `main` and its checkout stay as they were.
fn commit_on_branch(repo: &ProjectRepo, root: &Path, branch: &str, files: &[(&str, String)]) -> CommitId {
    let git = repo.git();
    let copy = root.join("working-copies").join(branch);
    let copy_arg = copy.to_str().unwrap().to_string();
    block_on(git.run(
        &repo.location().repository,
        &["worktree", "add", "-q", "-b", branch, &copy_arg, MAIN_BRANCH],
    ))
    .unwrap();
    for (path, text) in files {
        let path = copy.join("definitions").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    CommitId(block_on(git.commit_all(&copy, "edit", &Identity::default())).unwrap())
}

#[test]
fn ls_tree_and_cat_file_read_a_commit() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let commit = main_commit(&repo);
    let git = repo.git();
    let entries = block_on(git.ls_tree_r(&repo.location().repository, &commit.0)).unwrap();
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert!(paths.contains(&"definitions/tasks/backfill/task.toml"));
    assert!(entries.iter().all(|entry| entry.kind == "blob"));

    let objects = vec![
        format!("{}:definitions/project.toml", commit.0),
        "0000000000000000000000000000000000000000".to_string(),
        format!("{}:definitions/plan.md", commit.0),
    ];
    let blobs = block_on(git.cat_file_batch(&repo.location().repository, &objects)).unwrap();
    assert_eq!(
        blobs[0].as_deref(),
        Some(crate::test_support::PROJECT_TOML.as_bytes())
    );
    assert_eq!(blobs[1], None);
    assert_eq!(blobs[2].as_deref(), Some(&b""[..]));
}

#[test]
fn work_tree_at_an_unlanded_commit_joins_its_definitions_with_current_state() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    seed_state(&repo);
    let main_before = main_commit(&repo);
    let checkout_file = repo
        .location()
        .repository
        .join("definitions/tasks/backfill/task.toml");
    let checkout_before = std::fs::read_to_string(&checkout_file).unwrap();

    // Rename the goal and a todo, move the todos around, and add a task.
    let edited = commit_on_branch(
        &repo,
        root.path(),
        "work/rename",
        &[
            (
                "tasks/backfill/task.toml",
                definition_toml(
                    "l1",
                    "Backfill usage",
                    "other",
                    &[("t3", "check"), ("t2", "review it"), ("t1", "implement")],
                ),
            ),
            (
                "tasks/added/task.toml",
                definition_toml("l3", "Added", "other", &[]),
            ),
        ],
    );

    let tree = block_on(repo.load_work_tree_at(&TreeRef::Commit(edited))).unwrap();
    let backfill = tree.task_by_id("l1").unwrap();
    let file = backfill.file.as_ref().unwrap();
    assert_eq!(file.goal, "Backfill usage");
    assert_eq!(file.status, Status::Done);
    assert!(file.result.is_some());
    let todos: Vec<(&str, TodoState)> = file
        .todos
        .iter()
        .map(|todo| (todo.name.as_str(), todo.state))
        .collect();
    assert_eq!(
        todos,
        [
            ("check", TodoState::Open),
            ("review it", TodoState::Skipped),
            ("implement", TodoState::Done),
        ]
    );
    assert_eq!(backfill.dir, repo.location().files.join("l1"));
    assert_eq!(backfill.definition, Some(PathBuf::from("tasks/backfill")));
    assert!(tree.task_by_id("l3").is_some());

    // Master's tree and its checkout are untouched.
    assert_eq!(main_commit(&repo), main_before);
    let main_tree = block_on(repo.load_work_tree_at(&TreeRef::Commit(main_before))).unwrap();
    assert_eq!(main_tree.task_by_id("l1").unwrap().goal(), "Backfill");
    assert!(main_tree.task_by_id("l3").is_none());
    assert_eq!(
        std::fs::read_to_string(&checkout_file).unwrap(),
        checkout_before
    );
}

#[test]
fn a_tree_ref_by_tree_id_reads_the_same_definitions() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let commit = main_commit(&repo);
    let tree_id = block_on(repo.git().run(
        &repo.location().repository,
        &["rev-parse", &format!("{}^{{tree}}", commit.0)],
    ))
    .unwrap();
    let by_commit =
        block_on(crate::load_definition_tree(repo.git(), &repo.location().repository, &TreeRef::Commit(commit)))
            .unwrap();
    let by_tree = block_on(crate::load_definition_tree(
        repo.git(),
        &repo.location().repository,
        &TreeRef::Tree(crate::TreeId(tree_id)),
    ))
    .unwrap();
    assert_eq!(by_commit, by_tree);

    let mut files = Vec::new();
    for (path, text) in fixture_definitions() {
        files.push((
            bongos_project_repo_model::RelPath::new(path).unwrap(),
            text.into_bytes(),
        ));
    }
    assert_eq!(DefinitionTree::parse_files(files).unwrap(), by_commit);
}

#[test]
fn the_loaded_project_follows_the_verified_commit() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let first = main_commit(&repo);
    let loaded = LoadedProject::new(repo.clone());
    assert!(loaded.projects().unwrap().is_empty());

    // Before a verified commit is recorded, main is shown.
    assert!(block_on(loaded.load()).unwrap());
    assert_eq!(loaded.commit(), Some(first.clone()));
    assert!(!block_on(loaded.load()).unwrap());
    assert_eq!(repo.verified_commit().unwrap(), None);
    assert_eq!(writer(&repo).definitions(), loaded.definitions());

    let edited = commit_on_branch(
        &repo,
        root.path(),
        "work/goal",
        &[(
            "tasks/report/task.toml",
            definition_toml("l2", "Monthly report", "other", &[("r1", "write")]),
        )],
    );
    let definitions = Arc::new(
        block_on(crate::load_definition_tree(
            repo.git(),
            &repo.location().repository,
            &TreeRef::Commit(edited.clone()),
        ))
        .unwrap(),
    );
    writer(&repo)
        .transact(|tx| tx.set_verified_commit(&edited.0, definitions.clone()))
        .unwrap();
    assert_eq!(repo.verified_commit().unwrap(), Some(edited.clone()));
    assert!(block_on(loaded.load()).unwrap());
    let projects = loaded.projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].slug, "invoicing");
    let report = projects[0]
        .task
        .walk()
        .into_iter()
        .find(|task| task.id() == Some("l2"))
        .unwrap()
        .clone();
    assert_eq!(report.goal(), "Monthly report");
}

#[test]
fn a_dot_git_in_a_files_folder_is_not_an_artifact() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let files = repo.location().files.join("l1");
    std::fs::create_dir_all(files.join(".git/refs")).unwrap();
    std::fs::write(files.join(".git/HEAD"), "x").unwrap();
    std::fs::write(files.join("notes.md"), "x").unwrap();
    let loaded = LoadedProject::new(repo.clone());
    block_on(loaded.load()).unwrap();
    let tree = loaded.work_tree().unwrap();
    let names: Vec<&Path> = tree
        .task_by_id("l1")
        .unwrap()
        .files
        .keys()
        .map(PathBuf::as_path)
        .collect();
    assert_eq!(names, [Path::new("notes.md")]);
}

#[gpui::test]
async fn definition_tree_and_work_tree_at_run_in_the_background(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let commit = main_commit(&repo);
    let definitions = cx
        .update(|cx| repo.definition_tree(TreeRef::Commit(commit.clone()), cx))
        .await
        .unwrap();
    assert_eq!(definitions.nodes().len(), 3);
    let tree = cx
        .update(|cx| repo.work_tree_at(TreeRef::Commit(commit), cx))
        .await
        .unwrap();
    assert_eq!(
        tree.task_by_id("l1").unwrap().file.as_ref().unwrap().status,
        Status::Open
    );
}
