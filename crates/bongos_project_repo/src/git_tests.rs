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

/// Commits `files` (path, text) on top of what `directory` has checked out.
async fn commit_files(git: &Git, directory: &std::path::Path, files: &[(&str, &str)]) -> String {
    for (path, text) in files {
        std::fs::write(directory.join(path), text).unwrap();
    }
    git.commit_all(directory, "edit", &Identity::default())
        .await
        .unwrap()
}

#[test]
fn the_patch_id_sees_whitespace_inside_a_line_and_survives_a_rebase() {
    let dir = TempDir::new().unwrap();
    let git = git();
    let lines: Vec<String> = (1..=20).map(|n| format!("line {n}\n")).collect();
    let file = lines.concat();
    block_on(async {
        git.init_repo(dir.path(), "main").await.unwrap();
        let base = commit_files(&git, dir.path(), &[("a.txt", &file)]).await;
        let edited = |line: &str| file.replacen("line 10\n", line, 1);
        let spaced = commit_files(&git, dir.path(), &[("a.txt", &edited("rm -rf /tmp/x\n"))]).await;
        let id = git.patch_id(dir.path(), &base, &spaced).await.unwrap();
        git.run(dir.path(), &["reset", "-q", "--hard", &base])
            .await
            .unwrap();
        let split = commit_files(&git, dir.path(), &[("a.txt", &edited("rm -rf / tmp/x\n"))]).await;
        let split_id = git.patch_id(dir.path(), &base, &split).await.unwrap();
        assert_ne!(id, split_id);

        // Master gains lines before and after the edit (new blob ids, shifted hunk numbers);
        // the edit rebased onto it keeps its id.
        git.run(dir.path(), &["reset", "-q", "--hard", &base])
            .await
            .unwrap();
        let moved_file = file
            .replacen("line 1\n", "line 1\nline one and a half\n", 1)
            .replacen("line 18\n", "line eighteen\n", 1);
        let moved = commit_files(&git, dir.path(), &[("a.txt", &moved_file)]).await;
        let rebased = commit_files(
            &git,
            dir.path(),
            &[(
                "a.txt",
                &moved_file.replacen("line 10\n", "rm -rf /tmp/x\n", 1),
            )],
        )
        .await;
        assert_eq!(
            git.patch_id(dir.path(), &moved, &rebased).await.unwrap(),
            id
        );
        assert_eq!(
            git.patch_id(dir.path(), &base, &base).await.unwrap(),
            "empty"
        );
    });
}

/// Set in the child process of `inherited_git_variables_do_not_redirect_a_command`.
const CHILD: &str = "BONGOS_TEST_GIT_ENV_CHILD";

#[test]
fn inherited_git_variables_do_not_redirect_a_command() {
    if let Some(directory) = std::env::var_os("GIT_TEST_DIRECTORY") {
        // The child: git variables point at another repository.
        let top =
            block_on(git().run(directory.as_ref(), &["rev-parse", "--show-toplevel"])).unwrap();
        let expected = std::path::Path::new(&directory).canonicalize().unwrap();
        assert_eq!(std::path::Path::new(&top).canonicalize().unwrap(), expected);
        assert!(std::env::var_os(CHILD).is_some());
        return;
    }
    let dir = TempDir::new().unwrap();
    let work = dir.path().join("work");
    let decoy = dir.path().join("decoy");
    for path in [&work, &decoy] {
        std::fs::create_dir_all(path).unwrap();
        block_on(git().init_repo(path, "main")).unwrap();
    }
    let output = block_on(
        util::command::new_command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_tests::inherited_git_variables_do_not_redirect_a_command",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GIT_TEST_DIRECTORY", &work)
            .env("GIT_DIR", decoy.join(".git"))
            .env("GIT_WORK_TREE", &decoy)
            .env("GIT_INDEX_FILE", decoy.join(".git/index"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.bare")
            .env("GIT_CONFIG_VALUE_0", "true")
            .output(),
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the child run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_replacement_ref_does_not_change_what_an_object_reads_as() {
    let dir = TempDir::new().unwrap();
    let git = git();
    block_on(async {
        git.init_repo(dir.path(), "main").await.unwrap();
        commit_files(
            &git,
            dir.path(),
            &[("a.txt", "the real a\n"), ("b.txt", "an impostor\n")],
        )
        .await;
        let a = git.rev_parse(dir.path(), "main:a.txt").await.unwrap();
        let b = git.rev_parse(dir.path(), "main:b.txt").await.unwrap();
        git.run(dir.path(), &["replace", &a, &b]).await.unwrap();
        assert_eq!(
            git.run(dir.path(), &["cat-file", "-p", "main:a.txt"])
                .await
                .unwrap(),
            "the real a"
        );
    });
}
