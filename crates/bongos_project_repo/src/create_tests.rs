use std::ffi::OsString;
use std::path::Path;

use futures::executor::block_on;
use tempfile::TempDir;

use crate::{
    CreateError, CreateRequest, Git, GitError, LayoutTemplates, ProjectRepo, RepositoryLocations,
};

fn request(folder: &Path) -> CreateRequest {
    CreateRequest {
        title: "Invoicing".to_string(),
        folder: folder.to_path_buf(),
        layout: LayoutTemplates::default(),
        repositories: Vec::new(),
        remote: None,
    }
}

fn isolated_git() -> Result<Git, GitError> {
    Git::locate().map(Git::isolated)
}

fn tracked_files(repository: &Path) -> Vec<String> {
    let git = isolated_git().unwrap();
    let text = block_on(git.run(repository, &["ls-tree", "-r", "--name-only", "main"])).unwrap();
    text.lines().map(str::to_string).collect()
}

#[test]
fn create_makes_the_folder_layout_and_the_first_commit() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("projects").join("invoicing");
    let mut locations =
        RepositoryLocations::load(root.path().join("state").join("project-repositories.toml"))
            .unwrap();
    let repo = block_on(ProjectRepo::create_with(
        request(&folder),
        &mut locations,
        isolated_git,
    ))
    .unwrap();

    for part in ["repository", "working-copies", "state", "files"] {
        assert!(folder.join(part).is_dir(), "{part}");
    }
    assert_eq!(std::fs::read_dir(folder.join("state")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(folder.join("working-copies"))
            .unwrap()
            .count(),
        0
    );

    let repository = folder.join("repository");
    let git = isolated_git().unwrap();
    assert_eq!(
        block_on(git.run(&repository, &["rev-parse", "--abbrev-ref", "HEAD"])).unwrap(),
        "main"
    );
    assert_eq!(
        block_on(git.run(&repository, &["rev-list", "--count", "main"])).unwrap(),
        "1"
    );
    assert_eq!(
        tracked_files(&repository),
        [
            ".gitattributes",
            "definitions/plan.md",
            "definitions/project.toml",
            "definitions/task.toml"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(repository.join(".gitattributes")).unwrap(),
        "* text=auto\n"
    );
    let project = std::fs::read_to_string(repository.join("definitions/project.toml")).unwrap();
    assert!(project.contains("title = \"Invoicing\""), "{project}");
    assert!(project.contains(&repo.location().id), "{project}");
    assert!(
        std::fs::read_to_string(repository.join("definitions/task.toml"))
            .unwrap()
            .contains("goal = \"Invoicing\"")
    );
    // The working tree is clean after the commit.
    assert_eq!(
        block_on(git.run(&repository, &["status", "--porcelain"])).unwrap(),
        ""
    );

    assert_eq!(
        locations.location(&repo.location().id),
        Some(repo.location())
    );
    assert_eq!(repo.location().repository, repository);
}

#[test]
fn without_git_nothing_is_created_and_the_error_names_git() {
    let root = TempDir::new().unwrap();
    let no_git_dir = TempDir::new().unwrap();
    let folder = root.path().join("projects").join("invoicing");
    let record = root.path().join("state").join("project-repositories.toml");
    let mut locations = RepositoryLocations::load(&record).unwrap();

    let error = block_on(ProjectRepo::create_with(
        request(&folder),
        &mut locations,
        || Git::locate_in(&OsString::from(no_git_dir.path())),
    ))
    .unwrap_err();

    assert!(matches!(error, CreateError::Git(GitError::GitMissing)));
    assert!(
        error.to_string().contains("git is not installed"),
        "{error}"
    );
    assert!(!folder.exists());
    assert!(!root.path().join("projects").exists());
    assert!(!record.exists());
    assert!(locations.locations().is_empty());
}

#[test]
fn a_failure_after_creating_folders_removes_them() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("projects").join("invoicing");
    // A record path whose parent is a file: recording fails after everything else is made.
    let blocker = root.path().join("blocker");
    let mut locations = RepositoryLocations::load(blocker.join("record.toml")).unwrap();
    std::fs::write(&blocker, "").unwrap();
    let result = block_on(ProjectRepo::create_with(
        request(&folder),
        &mut locations,
        isolated_git,
    ));
    assert!(result.is_err());
    assert!(!root.path().join("projects").exists());
}

#[test]
fn a_changed_layout_does_not_move_an_existing_project() {
    let root = TempDir::new().unwrap();
    let record = root.path().join("project-repositories.toml");
    let mut locations = RepositoryLocations::load(&record).unwrap();
    let first_folder = root.path().join("first");
    let first = block_on(ProjectRepo::create_with(
        request(&first_folder),
        &mut locations,
        isolated_git,
    ))
    .unwrap();

    let second_folder = root.path().join("second");
    let mut changed = request(&second_folder);
    changed.title = "Second".to_string();
    changed.layout = LayoutTemplates {
        repository: "{folder}/main-checkout".to_string(),
        working_copies: "{folder}/wc".to_string(),
        state: "{folder}/live".to_string(),
        files: "{folder}/task-files".to_string(),
    };
    let second = block_on(ProjectRepo::create_with(
        changed,
        &mut locations,
        isolated_git,
    ))
    .unwrap();

    let reread = RepositoryLocations::load(&record).unwrap();
    let kept = reread.location(&first.location().id).unwrap();
    assert_eq!(kept.repository, first_folder.join("repository"));
    assert_eq!(kept.state, first_folder.join("state"));
    assert!(first_folder.join("repository").is_dir());
    let new = reread.location(&second.location().id).unwrap();
    assert_eq!(new.repository, second_folder.join("main-checkout"));
    assert_eq!(new.files, second_folder.join("task-files"));
}

#[test]
fn an_existing_folder_is_refused_and_left_alone() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("taken");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("mine.txt"), "x").unwrap();
    let mut locations =
        RepositoryLocations::load(root.path().join("project-repositories.toml")).unwrap();
    let error = block_on(ProjectRepo::create_with(
        request(&folder),
        &mut locations,
        isolated_git,
    ))
    .unwrap_err();
    assert!(matches!(error, CreateError::Exists(_)));
    assert!(folder.join("mine.txt").exists());
}
