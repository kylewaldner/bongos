use std::ffi::OsString;

use futures::executor::block_on;
use tempfile::TempDir;

use crate::{Git, GitError, Identity};

fn git() -> Git {
    Git::locate().expect("git is installed").isolated()
}

#[test]
fn init_commit_and_rev_parse_work_on_a_real_repository() {
    let dir = TempDir::new().unwrap();
    let git = git();
    block_on(async {
        git.init_repo(dir.path(), "main").await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let commit = git
            .commit_all(dir.path(), "first", &Identity::default())
            .await
            .unwrap();
        assert_eq!(commit.len(), 40);
        assert_eq!(git.rev_parse(dir.path(), "main").await.unwrap(), commit);
        assert!(git.rev_parse(dir.path(), "nope").await.is_err());
    });
}

#[test]
fn a_path_without_git_names_git() {
    let empty = TempDir::new().unwrap();
    let error = Git::locate_in(&OsString::from(empty.path())).unwrap_err();
    assert!(matches!(error, GitError::GitMissing));
    assert_eq!(
        error.to_string(),
        "git is not installed (needed for project repositories); install git and try again"
    );
    assert_eq!(GitError::missing_message(), error.to_string());
}
