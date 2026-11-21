//! Tests of run worktrees, joins, the built-in merge and `clear finished` over a real temporary git
//! repository. Git runs with an isolated home and no global or system config, so nothing here reads
//! or writes a real checkout.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bongos_workflow_model::record::{Attempt, EndState, Lifecycle};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::builtin_merge::{
    DIRTY_TASK_WORKTREE, MergeRefusal, MergeRequest, MergeStyle, Merged, merge,
};
use crate::lifecycle::attempt_worktree;
use crate::worktrees::{
    ClearItem, FinishedRun, Git, NOT_STARTED, RunWorktree, TaskCheckout, WorktreeError,
    apply_clear, branch_worktree_path, check_free_space, check_run_dir, create_branch, create_run,
    fan_out_branch_name, join, plan_clear, run_branch_name, run_dir, run_worktree_path, runs_dir,
    short_id,
};

const RUN_ID: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
const OTHER_RUN_ID: &str = "01890a5d-ac96-774b-bcce-b3020000aaaa";
const TASK_BRANCH: &str = "task/add-login";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    git: Git,
    repository: PathBuf,
    task: TaskCheckout,
}

impl Fixture {
    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        // Git reports canonical paths; the temporary directory may sit behind a symlink.
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let git = Git::new("git")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        let repository = root.join("repo");
        fs::create_dir_all(&repository).unwrap();
        git_out(&git, &repository, &["init", "-q", "-b", "main"]).await;
        fs::write(repository.join("README.md"), "readme\n").unwrap();
        fs::write(repository.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        git_out(&git, &repository, &["add", "README.md", "shared.txt"]).await;
        git_out(&git, &repository, &["commit", "-q", "-m", "initial"]).await;
        let task_worktree = repository.join(".bongos-worktrees").join("add-login");
        git_out(
            &git,
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                TASK_BRANCH,
                &task_worktree.to_string_lossy(),
            ],
        )
        .await;
        Self {
            _temp: temp,
            root,
            git,
            repository,
            task: TaskCheckout {
                slug: "add-login".to_string(),
                worktree: Some(task_worktree),
                branch: Some(TASK_BRANCH.to_string()),
            },
        }
    }

    fn task_worktree(&self) -> &Path {
        self.task.worktree.as_deref().unwrap()
    }

    /// The isolated home directory the fixture's git uses, never the real one.
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `run`, attempt 1 of `run_id` on the fixture's task, as `clear finished` reads it.
    fn finished(
        &self,
        run_id: &str,
        lifecycle: Lifecycle,
        finished: SystemTime,
        run: &RunWorktree,
    ) -> FinishedRun {
        FinishedRun {
            run_id: run_id.to_string(),
            lifecycle,
            finished,
            worktree: run.clone(),
            attempt: 1,
            checkout: self.task.clone(),
        }
    }

    async fn create_run(&self, run_id: &str) -> RunWorktree {
        create_run(&self.git, &self.task, run_id, 1, &settings(), &plenty)
            .await
            .unwrap()
    }

    fn request(&self, run: &RunWorktree, style: MergeStyle) -> MergeRequest {
        MergeRequest {
            task_worktree: self.task_worktree().to_path_buf(),
            task_branch: TASK_BRANCH.to_string(),
            run_git_dir: run.git_dir.clone(),
            run_branch: run.branch.clone(),
            run_id: RUN_ID.to_string(),
            workflow: "build".to_string(),
            todo: "write tests".to_string(),
            style,
        }
    }
}

fn settings() -> WorkflowsSettings {
    WorkflowsSettings {
        enabled: true,
        worktrees_dir: None,
        min_free_disk_mb: 2048,
        keep_finished_days: 14,
        limits: Default::default(),
        programs: Default::default(),
        ..WorkflowsSettings::default()
    }
}

fn plenty(_: &Path) -> Option<u64> {
    Some(u64::MAX)
}

async fn git_out(git: &Git, directory: &Path, args: &[&str]) -> String {
    git.run(directory, args).await.unwrap()
}

async fn commit_file(git: &Git, directory: &Path, file: &str, text: &str) -> String {
    fs::write(directory.join(file), text).unwrap();
    git_out(git, directory, &["add", file]).await;
    git_out(
        git,
        directory,
        &["commit", "-q", "-m", &format!("write {file}")],
    )
    .await;
    git_out(git, directory, &["rev-parse", "HEAD"]).await
}

async fn rev(git: &Git, directory: &Path, revision: &str) -> String {
    git_out(git, directory, &["rev-parse", revision]).await
}

/// Every file of a worktree (the `.git` link left out) with its bytes, and its HEAD.
async fn snapshot(git: &Git, worktree: &Path) -> (BTreeMap<PathBuf, Vec<u8>>, String) {
    fn walk(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                files.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(worktree, worktree, &mut files);
    (files, rev(git, worktree, "HEAD").await)
}

async fn branch_exists(git: &Git, directory: &Path, branch: &str) -> bool {
    git.run(
        directory,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok()
}

#[test]
fn branch_and_path_names() {
    assert_eq!(short_id(RUN_ID), "099a8057");
    assert_eq!(
        run_branch_name("task/add-login", RUN_ID, 2),
        "task/add-login-run-099a8057-2"
    );
    assert_eq!(
        fan_out_branch_name("task/add-login-run-099a8057-2", "review", 3),
        "task/add-login-run-099a8057-2-review-3"
    );
    let runs = runs_dir(Path::new("/repo"), None);
    assert_eq!(runs, Path::new("/repo/.bongos-worktrees/.runs"));
    assert_eq!(
        runs_dir(Path::new("/repo"), Some(Path::new("/elsewhere"))),
        Path::new("/elsewhere")
    );
    assert_eq!(
        runs_dir(Path::new("/repo"), Some(Path::new("runs/here"))),
        Path::new("/repo/runs/here")
    );
    assert_eq!(
        runs_dir(Path::new("/repo"), Some(Path::new("~/runs"))),
        paths::home_dir().join("runs")
    );
    let dir = run_dir(&runs, "add-login", RUN_ID, 2);
    assert_eq!(
        dir,
        Path::new("/repo/.bongos-worktrees/.runs/add-login-099a8057-2")
    );
    assert_eq!(run_worktree_path(&dir), dir.join("run"));
    assert_eq!(
        branch_worktree_path(&dir, "review", 3),
        dir.join("b-review-3")
    );
}

/// A home directory that is not the real one and does not exist.
const FAKE_HOME: &str = "/home/me";

/// Whether `worktree` passes [`check_run_dir`] as attempt `attempt` of [`RUN_ID`] on task `slug`
/// in the repository `/repo`.
fn is_run_worktree(
    worktree: &RunWorktree,
    worktrees_dir: Option<&Path>,
    slug: &str,
    attempt: u32,
) -> bool {
    check_run_dir(
        worktree,
        Path::new("/repo"),
        worktrees_dir,
        Path::new(FAKE_HOME),
        slug,
        RUN_ID,
        attempt,
    )
    .is_ok()
}

fn worktree_in(run_dir: &Path) -> RunWorktree {
    RunWorktree {
        path: run_worktree_path(run_dir),
        run_dir: run_dir.to_path_buf(),
        branch: "b".into(),
        git_dir: PathBuf::from("/repo/.git/worktrees/b"),
    }
}

#[test]
fn a_recorded_worktree_off_the_run_layout_is_not_a_run_worktree() {
    let runs = runs_dir(Path::new("/repo"), None);
    let dir = run_dir(&runs, "add-login", RUN_ID, 2);
    let real = RunWorktree {
        path: run_worktree_path(&dir),
        run_dir: dir.clone(),
        branch: "b".into(),
        git_dir: PathBuf::from("/repo/.git/worktrees/b"),
    };
    assert!(is_run_worktree(&real, None, "add-login", 2));
    // An unrelated worktree: its parent would be removed with it.
    let stray = RunWorktree {
        path: PathBuf::from("/home/me/project/src"),
        run_dir: PathBuf::from("/home/me/project"),
        branch: "b".into(),
        git_dir: PathBuf::from("/repo/.git/worktrees/b"),
    };
    assert!(!is_run_worktree(&stray, None, "add-login", 2));
    // The right shape in the wrong place.
    let elsewhere = RunWorktree {
        path: PathBuf::from("/home/me/stuff/run"),
        run_dir: PathBuf::from("/home/me/stuff"),
        branch: "b".into(),
        git_dir: PathBuf::from("/repo/.git/worktrees/b"),
    };
    assert!(!is_run_worktree(&elsewhere, None, "add-login", 2));
    // A configured runs directory.
    let custom = Path::new("/elsewhere");
    let moved = RunWorktree {
        path: run_worktree_path(&run_dir(custom, "t", RUN_ID, 1)),
        run_dir: run_dir(custom, "t", RUN_ID, 1),
        branch: "b".into(),
        git_dir: PathBuf::from("/repo/.git/worktrees/b"),
    };
    assert!(is_run_worktree(&moved, Some(custom), "t", 1));
    assert!(!is_run_worktree(&moved, None, "t", 1));
    // A leading `./` in the configured directory is dropped before matching; the relative
    // directory is resolved against the repository, as `runs_dir` does.
    let relative = worktree_in(&run_dir(Path::new("/repo/elsewhere"), "t", RUN_ID, 1));
    assert!(!is_run_worktree(
        &moved,
        Some(Path::new("./elsewhere")),
        "t",
        1
    ));
    assert!(is_run_worktree(
        &relative,
        Some(Path::new("./elsewhere")),
        "t",
        1
    ));
}

#[test]
fn a_run_dir_with_dot_components_is_not_a_run_worktree() {
    for run_dir in [
        "/repo/.bongos-worktrees/.runs/..",
        "/repo/.bongos-worktrees/.runs/../x",
        "./repo/.bongos-worktrees/.runs/x",
        "repo/.bongos-worktrees/.runs/x",
        "/repo/.bongos-worktrees/.runs/.",
    ] {
        let worktree = worktree_in(Path::new(run_dir));
        assert!(
            !is_run_worktree(&worktree, None, "add-login", 2),
            "{worktree:?}"
        );
    }
    // Even with `..` leading back to exactly the expected directory.
    let back = worktree_in(Path::new(
        "/repo/.bongos-worktrees/.runs/x/../add-login-099a8057-2",
    ));
    assert!(!is_run_worktree(&back, None, "add-login", 2), "{back:?}");
}

#[test]
fn an_empty_or_dot_worktrees_dir_refuses_any_run_dir() {
    // The run directory these settings would give: the repository itself, and elsewhere.
    for configured in ["", ".", "./", "..", "/", "~", "~/"] {
        let configured = Path::new(configured);
        for run_dir in [
            run_dir(
                &runs_dir(Path::new("/repo"), Some(configured)),
                "t",
                RUN_ID,
                1,
            ),
            PathBuf::from("/some/project"),
            PathBuf::from("/home/me/stuff"),
        ] {
            let worktree = worktree_in(&run_dir);
            assert!(
                !is_run_worktree(&worktree, Some(configured), "t", 1),
                "{configured:?} {worktree:?}"
            );
        }
    }
    // The home directory the check is given counts, not the real one.
    let runs_in_home = run_dir(Path::new(FAKE_HOME), "t", RUN_ID, 1);
    assert!(!is_run_worktree(
        &worktree_in(&runs_in_home),
        Some(Path::new(FAKE_HOME)),
        "t",
        1
    ));
    assert!(!is_run_worktree(
        &worktree_in(&run_dir(Path::new("/"), "t", RUN_ID, 1)),
        Some(Path::new("/")),
        "t",
        1
    ));
}

#[test]
fn a_symlink_out_of_the_runs_dir_is_refused() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repository = root.join("repo");
    let home = root.join("home");
    let outside = root.join("outside");
    for dir in [&repository, &home, &outside] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    let check = |worktree: &RunWorktree, configured: Option<&Path>| {
        check_run_dir(worktree, &repository, configured, &home, "t", RUN_ID, 1)
    };

    // The run directory itself is a symlink to a directory elsewhere.
    let runs = runs_dir(&repository, None);
    fs::create_dir_all(&runs).unwrap();
    let linked = run_dir(&runs, "t", RUN_ID, 1);
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    assert!(check(&worktree_in(&linked), None).is_err());

    // The configured runs directory is a symlink to the home directory.
    let to_home = root.join("runs-to-home");
    std::os::unix::fs::symlink(&home, &to_home).unwrap();
    let in_home = run_dir(&to_home, "t", RUN_ID, 1);
    fs::create_dir_all(&in_home).unwrap();
    assert!(check(&worktree_in(&in_home), Some(&to_home)).is_err());

    // The configured runs directory is a symlink to the repository.
    let to_repository = root.join("runs-to-repo");
    std::os::unix::fs::symlink(&repository, &to_repository).unwrap();
    let in_repository = run_dir(&to_repository, "t", RUN_ID, 1);
    fs::create_dir_all(&in_repository).unwrap();
    assert!(check(&worktree_in(&in_repository), Some(&to_repository)).is_err());

    // A plain runs directory elsewhere is accepted.
    let plain = root.join("runs");
    let real = run_dir(&plain, "t", RUN_ID, 1);
    fs::create_dir_all(&real).unwrap();
    assert_eq!(check(&worktree_in(&real), Some(&plain)), Ok(()));
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep\n"
    );
}

#[gpui::test]
async fn create_run_names_branches_and_paths(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let expected_dir = fixture
        .repository
        .join(".bongos-worktrees/.runs/add-login-099a8057-1");
    assert_eq!(run.run_dir, expected_dir);
    assert_eq!(run.path, expected_dir.join("run"));
    assert_eq!(run.branch, "task/add-login-run-099a8057-1");
    assert_eq!(
        rev(git, &run.path, "HEAD").await,
        rev(git, fixture.task_worktree(), "HEAD").await
    );
    assert_eq!(
        git_out(git, &run.path, &["symbolic-ref", "--short", "HEAD"]).await,
        run.branch
    );

    let branch = create_branch(git, &run, "review", 0).await.unwrap();
    assert_eq!(branch.path, expected_dir.join("b-review-0"));
    assert_eq!(branch.branch, "task/add-login-run-099a8057-1-review-0");
    assert_eq!(
        git_out(git, &branch.path, &["symbolic-ref", "--short", "HEAD"]).await,
        branch.branch
    );

    let elsewhere = fixture.root.join("runs");
    let custom = WorkflowsSettings {
        worktrees_dir: Some(elsewhere.clone()),
        ..settings()
    };
    let other = create_run(git, &fixture.task, OTHER_RUN_ID, 1, &custom, &plenty)
        .await
        .unwrap();
    assert_eq!(other.path, elsewhere.join("add-login-0000aaaa-1/run"));
}

#[gpui::test]
async fn a_task_without_a_worktree_refuses(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let task = TaskCheckout {
        worktree: None,
        branch: None,
        ..fixture.task.clone()
    };
    let error = create_run(&fixture.git, &task, RUN_ID, 1, &settings(), &plenty)
        .await
        .unwrap_err();
    assert_eq!(error, WorktreeError::NotStarted);
    assert_eq!(error.to_string(), NOT_STARTED);
    assert_eq!(error.to_string(), "start the task first (task: start)");
}

#[gpui::test]
async fn low_free_space_refuses_with_the_setting_name(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let low = |_: &Path| Some(100 * 1024 * 1024);
    let error = create_run(&fixture.git, &fixture.task, RUN_ID, 1, &settings(), &low)
        .await
        .unwrap_err();
    let WorktreeError::LowDiskSpace {
        free_mb, min_mb, ..
    } = &error
    else {
        panic!("expected a free-space refusal, got {error:?}");
    };
    assert_eq!((*free_mb, *min_mb), (100, 2048));
    let message = error.to_string();
    assert!(message.contains("workflows.min_free_disk_mb"), "{message}");
    assert!(message.contains("100 MB free"), "{message}");
    assert!(!fixture.repository.join(".bongos-worktrees/.runs").exists());
    let branch = run_branch_name(TASK_BRANCH, RUN_ID, 1);
    assert!(!branch_exists(&fixture.git, &fixture.repository, &branch).await);

    // Exactly at the minimum is enough, and an unknown free space does not block.
    let exact = |_: &Path| Some(2048 * 1024 * 1024);
    assert!(check_free_space(Path::new("/"), 2048, &exact).is_ok());
    assert!(check_free_space(Path::new("/"), 2048, &|_| None).is_ok());
}

#[gpui::test]
async fn the_task_worktree_is_untouched_until_the_merge_fast_forwards(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let before = snapshot(git, fixture.task_worktree()).await;

    let run = fixture.create_run(RUN_ID).await;
    commit_file(git, &run.path, "login.rs", "fn login() {}\n").await;
    let first = create_branch(git, &run, "review", 0).await.unwrap();
    let second = create_branch(git, &run, "review", 1).await.unwrap();
    commit_file(git, &first.path, "a.txt", "a\n").await;
    commit_file(git, &second.path, "b.txt", "b\n").await;
    let outcome = join(git, &run, "review", &[1, 0]).await.unwrap();
    assert_eq!(outcome.merged, vec![0, 1]);
    assert_eq!(outcome.conflict, None);
    assert_eq!(snapshot(git, fixture.task_worktree()).await, before);
    assert_eq!(
        rev(
            git,
            &fixture.repository,
            &format!("refs/heads/{TASK_BRANCH}")
        )
        .await,
        before.1
    );

    let run_head = rev(git, &run.path, "HEAD").await;
    let merged = merge(git, &fixture.request(&run, MergeStyle::FfOrMerge))
        .await
        .unwrap();
    assert_eq!(
        merged,
        Merged::FastForward {
            head: run_head.clone()
        }
    );
    assert_eq!(rev(git, fixture.task_worktree(), "HEAD").await, run_head);
    for file in ["login.rs", "a.txt", "b.txt"] {
        assert!(fixture.task_worktree().join(file).exists(), "{file}");
    }
}

#[gpui::test]
async fn a_moved_task_branch_gets_a_merge_commit_naming_run_and_todo(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let run_head = commit_file(git, &run.path, "login.rs", "fn login() {}\n").await;
    let task_head = commit_file(git, fixture.task_worktree(), "notes.md", "notes\n").await;

    let refused = merge(git, &fixture.request(&run, MergeStyle::FfOnly))
        .await
        .unwrap_err();
    assert_eq!(refused, MergeRefusal::NotFastForward);
    assert_eq!(rev(git, fixture.task_worktree(), "HEAD").await, task_head);

    let merged = merge(git, &fixture.request(&run, MergeStyle::FfOrMerge))
        .await
        .unwrap();
    let Merged::MergeCommit { head } = merged else {
        panic!("expected a merge commit, got {merged:?}");
    };
    let worktree = fixture.task_worktree();
    assert_eq!(rev(git, worktree, "HEAD").await, head);
    assert_eq!(
        git_out(git, worktree, &["log", "-1", "--format=%s"]).await,
        "Merge run 099a8057 (build) for todo write tests"
    );
    assert_eq!(rev(git, worktree, "HEAD^1").await, task_head);
    assert_eq!(rev(git, worktree, "HEAD^2").await, run_head);
}

#[gpui::test]
async fn merge_commit_style_never_fast_forwards(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let run_head = commit_file(git, &run.path, "login.rs", "fn login() {}\n").await;
    let merged = merge(git, &fixture.request(&run, MergeStyle::MergeCommit))
        .await
        .unwrap();
    assert!(matches!(merged, Merged::MergeCommit { .. }), "{merged:?}");
    assert_eq!(rev(git, fixture.task_worktree(), "HEAD^2").await, run_head);
    assert_eq!(MergeStyle::default(), MergeStyle::FfOrMerge);
    for style in [
        MergeStyle::FfOrMerge,
        MergeStyle::FfOnly,
        MergeStyle::MergeCommit,
    ] {
        assert_eq!(MergeStyle::parse(style.as_str()), Some(style));
    }
    assert_eq!(MergeStyle::parse("rebase"), None);
}

#[gpui::test]
async fn a_conflict_names_the_file_and_leaves_both_branches(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let run_head = commit_file(git, &run.path, "shared.txt", "one\nRUN\nthree\n").await;
    let task_head = commit_file(
        git,
        fixture.task_worktree(),
        "shared.txt",
        "one\nTASK\nthree\n",
    )
    .await;
    let before = snapshot(git, fixture.task_worktree()).await;

    let refused = merge(git, &fixture.request(&run, MergeStyle::FfOrMerge))
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        MergeRefusal::Conflict(vec!["shared.txt".to_string()])
    );
    assert!(refused.to_string().contains("shared.txt"));
    let worktree = fixture.task_worktree();
    assert_eq!(snapshot(git, worktree).await, before);
    assert_eq!(rev(git, worktree, "HEAD").await, task_head);
    assert_eq!(git_out(git, worktree, &["status", "--porcelain"]).await, "");
    assert!(
        git.run(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .await
            .is_err()
    );
    assert_eq!(
        rev(git, worktree, &format!("refs/heads/{}", run.branch)).await,
        run_head
    );
}

#[gpui::test]
async fn a_dirty_task_worktree_refuses_before_touching_anything(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    commit_file(git, &run.path, "login.rs", "fn login() {}\n").await;
    let worktree = fixture.task_worktree();
    fs::write(worktree.join("README.md"), "edited\n").unwrap();
    let before = snapshot(git, worktree).await;

    let refused = merge(git, &fixture.request(&run, MergeStyle::FfOrMerge))
        .await
        .unwrap_err();
    assert_eq!(refused, MergeRefusal::DirtyTaskWorktree);
    assert_eq!(refused.to_string(), DIRTY_TASK_WORKTREE);
    assert_eq!(
        refused.to_string(),
        "the task worktree has uncommitted changes; commit or stash them"
    );
    assert_eq!(snapshot(git, worktree).await, before);

    // An untracked file counts too.
    git_out(git, worktree, &["checkout", "--", "README.md"]).await;
    fs::write(worktree.join("scratch.txt"), "scratch\n").unwrap();
    let refused = merge(git, &fixture.request(&run, MergeStyle::FfOrMerge))
        .await
        .unwrap_err();
    assert_eq!(refused, MergeRefusal::DirtyTaskWorktree);
}

#[gpui::test]
async fn join_merges_in_branch_order_whatever_the_finish_order(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let mut branches = Vec::new();
    for index in 0..3 {
        let branch = create_branch(git, &run, "build", index).await.unwrap();
        commit_file(git, &branch.path, &format!("part{index}.txt"), "part\n").await;
        branches.push(branch);
    }
    let outcome = join(git, &run, "build", &[2, 0, 1]).await.unwrap();
    assert_eq!(outcome.merged, vec![0, 1, 2]);
    assert_eq!(
        git_out(
            git,
            &run.path,
            &["log", "--first-parent", "-3", "--format=%s"]
        )
        .await,
        "Join build branch 2\nJoin build branch 1\nJoin build branch 0"
    );
    for (index, branch) in branches.iter().enumerate() {
        assert!(
            branch_exists(git, &run.path, &branch.branch).await,
            "{index}"
        );
    }
}

#[gpui::test]
async fn a_join_conflict_names_the_file_and_leaves_every_branch(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let first = create_branch(git, &run, "build", 0).await.unwrap();
    let second = create_branch(git, &run, "build", 1).await.unwrap();
    commit_file(git, &first.path, "shared.txt", "one\nFIRST\nthree\n").await;
    let second_head = commit_file(git, &second.path, "shared.txt", "one\nSECOND\nthree\n").await;

    let outcome = join(git, &run, "build", &[1, 0]).await.unwrap();
    assert_eq!(outcome.merged, vec![0]);
    assert_eq!(outcome.conflict, Some((1, vec!["shared.txt".to_string()])));
    assert_eq!(
        git_out(git, &run.path, &["status", "--porcelain"]).await,
        ""
    );
    assert_eq!(
        rev(git, &run.path, &format!("refs/heads/{}", second.branch)).await,
        second_head
    );
    assert!(branch_exists(git, &run.path, &first.branch).await);
}

#[gpui::test]
async fn clear_finished_skips_a_dirty_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let old = SystemTime::now() - Duration::from_secs(20 * 24 * 60 * 60);
    let clean = fixture.create_run(RUN_ID).await;
    let clean_branch = create_branch(git, &clean, "build", 0).await.unwrap();
    let dirty = fixture.create_run(OTHER_RUN_ID).await;
    let dirty_branch = create_branch(git, &dirty, "build", 0).await.unwrap();
    fs::write(dirty_branch.path.join("README.md"), "uncommitted\n").unwrap();
    let recent_id = "01890a5d-ac96-774b-bcce-b302bbbbbbbb";
    let recent = fixture.create_run(recent_id).await;
    let failed_id = "01890a5d-ac96-774b-bcce-b302cccccccc";
    let failed = fixture.create_run(failed_id).await;

    let finished = vec![
        fixture.finished(RUN_ID, Lifecycle::Ended(EndState::Succeeded), old, &clean),
        fixture.finished(OTHER_RUN_ID, Lifecycle::Cancelled, old, &dirty),
        fixture.finished(
            recent_id,
            Lifecycle::Ended(EndState::Succeeded),
            SystemTime::now(),
            &recent,
        ),
        fixture.finished(failed_id, Lifecycle::Ended(EndState::Failed), old, &failed),
    ];
    let plan = plan_clear(git, &finished, None, &fixture.home(), 14, SystemTime::now())
        .await
        .unwrap();
    let listed: Vec<&str> = plan.runs.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(listed, vec![RUN_ID, OTHER_RUN_ID]);
    let skipped: Vec<&PathBuf> = plan.skipped().map(|item| &item.path).collect();
    assert_eq!(skipped, vec![&dirty_branch.path]);
    let summary = plan.summary();
    assert!(
        summary.contains(&clean.path.display().to_string()),
        "{summary}"
    );
    assert!(summary.contains("uncommitted changes"), "{summary}");

    let report = apply_clear(git, &fixture.repository, &plan).await;
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0.path, dirty_branch.path);
    assert!(!clean.run_dir.exists());
    assert!(!branch_exists(git, &fixture.repository, &clean.branch).await);
    assert!(!branch_exists(git, &fixture.repository, &clean_branch.branch).await);
    assert!(!dirty.path.exists());
    assert!(!branch_exists(git, &fixture.repository, &dirty.branch).await);
    assert_eq!(
        fs::read_to_string(dirty_branch.path.join("README.md")).unwrap(),
        "uncommitted\n"
    );
    assert!(branch_exists(git, &fixture.repository, &dirty_branch.branch).await);
    assert!(recent.path.exists());
    assert!(failed.path.exists());
}

#[gpui::test]
async fn clear_finished_keeps_a_worktree_switched_to_a_user_branch(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let old = SystemTime::now() - Duration::from_secs(20 * 24 * 60 * 60);
    let run = fixture.create_run(RUN_ID).await;
    let branch = create_branch(git, &run, "build", 0).await.unwrap();
    git_out(git, &branch.path, &["switch", "-c", "my-work"]).await;
    commit_file(git, &branch.path, "mine.txt", "mine\n").await;
    let finished = vec![fixture.finished(RUN_ID, Lifecycle::Ended(EndState::Succeeded), old, &run)];
    let plan = plan_clear(git, &finished, None, &fixture.home(), 14, SystemTime::now())
        .await
        .unwrap();
    let skipped: Vec<&PathBuf> = plan.skipped().map(|item| &item.path).collect();
    assert_eq!(skipped, vec![&branch.path]);
    assert!(
        plan.summary().contains("on branch my-work"),
        "{}",
        plan.summary()
    );

    let report = apply_clear(git, &fixture.repository, &plan).await;
    assert_eq!(report.skipped.len(), 1);
    assert!(branch.path.exists());
    assert!(branch_exists(git, &fixture.repository, "my-work").await);
}

/// Days as an offset from the epoch: no clock and no calendar date.
fn day(days: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

#[gpui::test]
async fn clear_finished_removes_only_known_leftovers_and_keeps_a_run_dir_with_more(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let tidy = fixture.create_run(RUN_ID).await;
    let cluttered = fixture.create_run(OTHER_RUN_ID).await;
    // What a program node leaves: its result file and its emptied `tmp`.
    for run in [&tidy, &cluttered] {
        let attempt = run.run_dir.join("scratch/check/main/0-1");
        fs::create_dir_all(attempt.join("tmp")).unwrap();
        fs::write(attempt.join("result.json"), "{}").unwrap();
    }
    // Something the app never writes there.
    let extra = cluttered.run_dir.join("notes.txt");
    fs::write(&extra, "mine\n").unwrap();

    let finished = vec![
        fixture.finished(RUN_ID, Lifecycle::Ended(EndState::Succeeded), day(1), &tidy),
        fixture.finished(OTHER_RUN_ID, Lifecycle::Cancelled, day(1), &cluttered),
    ];
    let plan = plan_clear(git, &finished, None, &fixture.home(), 14, day(100))
        .await
        .unwrap();
    assert_eq!(plan.skipped().count(), 0, "{}", plan.summary());

    let report = apply_clear(git, &fixture.repository, &plan).await;
    assert_eq!(report.removed.len(), 2, "{report}");
    assert!(!tidy.run_dir.exists(), "only known leftovers were in it");
    assert!(!branch_exists(git, &fixture.repository, &tidy.branch).await);
    // The run worktree went, the extra file and so the run directory stay, reported.
    assert!(!cluttered.path.exists());
    assert_eq!(fs::read_to_string(&extra).unwrap(), "mine\n");
    assert!(cluttered.run_dir.is_dir());
    assert_eq!(report.skipped.len(), 1, "{report}");
    assert_eq!(report.skipped[0].0.path, cluttered.run_dir);
}

#[gpui::test]
async fn clear_finished_keeps_a_run_whose_recorded_dir_is_not_the_one_the_app_made(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    // A record whose run directory is a directory with other files in it.
    let elsewhere = fixture.root.join("project");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "keep\n").unwrap();
    let forged = RunWorktree {
        path: run_worktree_path(&elsewhere),
        run_dir: elsewhere.clone(),
        branch: run.branch.clone(),
        git_dir: run.git_dir.clone(),
    };
    let finished = vec![fixture.finished(
        RUN_ID,
        Lifecycle::Ended(EndState::Succeeded),
        day(1),
        &forged,
    )];
    for configured in [None, Some(Path::new("")), Some(Path::new("."))] {
        let plan = plan_clear(git, &finished, configured, &fixture.home(), 14, day(100))
            .await
            .unwrap();
        assert_eq!(plan.runs.len(), 1);
        assert!(plan.runs[0].remove.is_empty(), "{}", plan.summary());
        assert_eq!(plan.runs[0].repository, None);
        let skipped: Vec<&ClearItem> = plan.skipped().collect();
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not the run directory")
                    || reason.contains("runs directory")),
            "{skipped:?}"
        );
        let report = apply_clear(git, &fixture.repository, &plan).await;
        assert!(report.removed.is_empty());
    }
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert!(run.path.exists());
    assert!(branch_exists(git, &fixture.repository, &run.branch).await);
}

#[gpui::test]
async fn clear_finished_keeps_a_run_whose_recorded_branch_is_not_the_run_branch(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    // An unrelated branch with work on it that is merged nowhere.
    git_out(git, fixture.task_worktree(), &["branch", "feature/x"]).await;
    let other = fixture.root.join("feature-x");
    git_out(
        git,
        &fixture.repository,
        &[
            "worktree",
            "add",
            "-q",
            &other.to_string_lossy(),
            "feature/x",
        ],
    )
    .await;
    commit_file(git, &other, "x.txt", "x\n").await;
    git_out(
        git,
        &fixture.repository,
        &["worktree", "remove", &other.to_string_lossy()],
    )
    .await;
    // The record has the run directory the app made but names that branch.
    let forged = RunWorktree {
        branch: "feature/x".to_string(),
        ..run.clone()
    };
    let finished = vec![fixture.finished(
        RUN_ID,
        Lifecycle::Ended(EndState::Succeeded),
        day(1),
        &forged,
    )];
    let plan = plan_clear(git, &finished, None, &fixture.home(), 14, day(100))
        .await
        .unwrap();
    assert_eq!(plan.runs.len(), 1);
    assert!(plan.runs[0].remove.is_empty(), "{}", plan.summary());
    assert_eq!(plan.runs[0].repository, None);
    let skipped: Vec<&ClearItem> = plan.skipped().collect();
    assert_eq!(skipped.len(), 1);
    assert!(
        skipped[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("branch feature/x is not the run branch")),
        "{skipped:?}"
    );

    let report = apply_clear(git, &fixture.repository, &plan).await;
    assert!(report.removed.is_empty(), "{report}");
    assert!(branch_exists(git, &fixture.repository, "feature/x").await);
    assert!(run.path.exists());
    assert!(branch_exists(git, &fixture.repository, &run.branch).await);
}

#[gpui::test]
async fn clear_finished_refuses_a_run_dir_swapped_for_a_symlink_after_the_plan(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let finished =
        vec![fixture.finished(RUN_ID, Lifecycle::Ended(EndState::Succeeded), day(1), &run)];
    let plan = plan_clear(git, &finished, None, &fixture.home(), 14, day(100))
        .await
        .unwrap();
    assert_eq!(plan.runs.len(), 1);
    assert_eq!(plan.runs[0].remove.len(), 1, "{}", plan.summary());

    // Between the question and the answer the run directory becomes a symlink elsewhere.
    let elsewhere = fixture.root.join("elsewhere");
    fs::rename(&run.run_dir, &elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "keep\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &run.run_dir).unwrap();

    let report = apply_clear(git, &fixture.repository, &plan).await;
    assert!(report.removed.is_empty(), "{report}");
    assert_eq!(report.skipped.len(), 1, "{report}");
    assert!(report.skipped[0].1.contains("symlink"), "{report}");
    assert!(
        fs::symlink_metadata(&run.run_dir)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert!(elsewhere.join("run").join("README.md").exists());
    assert!(branch_exists(git, &fixture.repository, &run.branch).await);
}

/// Makes `run`'s `.git` a file that names `fake`, a directory whose configuration would run
/// `touch <marker>` as the file system monitor, as a job could write it.
fn plant_git_file(run: &RunWorktree, fake: &Path, marker: &Path) {
    fs::create_dir_all(fake).unwrap();
    // Everything plain git needs to accept it as a repository, so only the code under test keeps
    // the configuration from running.
    fs::create_dir_all(fake.join("objects")).unwrap();
    fs::create_dir_all(fake.join("refs")).unwrap();
    fs::write(fake.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        fake.join("config"),
        // `sh -c` takes the arguments git appends to the hook as positionals, so they create no files.
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tfsmonitor = sh -c 'touch {}'\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::write(
        run.path.join(".git"),
        format!("gitdir: {}\n", fake.display()),
    )
    .unwrap();
}

#[gpui::test]
async fn a_planted_git_file_in_the_run_worktree_runs_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    let marker = fixture.root.join("fsmonitor-ran");
    plant_git_file(&run, &fixture.root.join("fake-git-dir"), &marker);

    // Control: plain git does run the planted configuration, so the plant works.
    let control = git
        .output(&run.path, &["status", "--porcelain"])
        .await
        .unwrap();
    assert!(
        marker.exists(),
        "plain git did not run the planted fsmonitor: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    fs::remove_file(&marker).unwrap();

    // Every host call in the run tree names the recorded git directory.
    assert!(!git.is_dirty(run.at()).await.unwrap());
    let head = git.run_at(run.at(), &["rev-parse", "HEAD"]).await.unwrap();
    assert_eq!(head, rev(git, &fixture.repository, &run.branch).await);
    let branch = create_branch(git, &run, "build", 0).await.unwrap();
    assert!(branch.path.join("README.md").exists());
    assert_eq!(
        git.run_at(branch.at(), &["rev-parse", "HEAD"])
            .await
            .unwrap(),
        head
    );
    let outcome = join(git, &run, "build", &[0]).await.unwrap();
    assert_eq!(outcome.merged, vec![0]);
    assert!(
        !marker.exists(),
        "the planted fsmonitor ran in the run worktree"
    );
}

#[gpui::test]
async fn the_run_diff_runs_no_external_diff_or_text_conversion(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let git = &fixture.git;
    let run = fixture.create_run(RUN_ID).await;
    commit_file(git, &run.path, "shared.txt", "one\ntwo\nthree\nfour\n").await;

    // A driver the repository configuration defines, selected by a `.gitattributes` the job wrote.
    let marker = fixture.root.join("driver-ran");
    let script = fixture.root.join("driver.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\ncat \"$1\"\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let script_text = script.to_string_lossy().into_owned();
    git_out(
        git,
        &fixture.repository,
        &["config", "diff.evil.textconv", &script_text],
    )
    .await;
    git_out(
        git,
        &fixture.repository,
        &["config", "diff.evil.command", &script_text],
    )
    .await;
    git_out(
        git,
        &fixture.repository,
        &["config", "diff.external", &script_text],
    )
    .await;
    fs::write(run.path.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
    let range = format!("{TASK_BRANCH}...{}", run.branch);

    // The same command from a discovered `.git` does run the driver, so this test can fail.
    git_out(git, &run.path, &["diff", "--no-color", &range]).await;
    assert!(marker.exists(), "the control run did not use the driver");
    fs::remove_file(&marker).unwrap();

    let diff = git
        .run_at(run.at(), &["diff", "--no-color", &range])
        .await
        .unwrap();
    assert!(diff.contains("+four"), "{diff}");
    assert!(!marker.exists(), "the run diff ran a configured driver");
}

#[gpui::test]
async fn an_old_record_without_a_git_directory_is_filled_in(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let run = fixture.create_run(RUN_ID).await;
    let attempt = |git_dir: Option<String>| Attempt {
        n: 1,
        created: "2026-11-11T10:00:00Z".to_string(),
        definition_hash: "h".to_string(),
        definition_commit: None,
        worktree: run.path.to_string_lossy().into_owned(),
        branch: run.branch.clone(),
        git_dir,
        loop_bound: None,
    };
    let migrated = attempt_worktree(&attempt(None), &fixture.task).unwrap();
    assert_eq!(migrated, run);
    let recorded = attempt(Some(run.git_dir.to_string_lossy().into_owned()));
    assert_eq!(attempt_worktree(&recorded, &fixture.task).unwrap(), run);
}

#[gpui::test]
async fn an_old_record_whose_git_file_names_an_untrusted_directory_refuses(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let run = fixture.create_run(RUN_ID).await;
    let attempt = Attempt {
        n: 1,
        created: "2026-11-11T10:00:00Z".to_string(),
        definition_hash: "h".to_string(),
        definition_commit: None,
        worktree: run.path.to_string_lossy().into_owned(),
        branch: run.branch.clone(),
        git_dir: None,
        loop_bound: None,
    };
    // A directory that is not under the repository's `worktrees/`.
    let marker = fixture.root.join("fsmonitor-ran");
    plant_git_file(&run, &fixture.root.join("fake-git-dir"), &marker);
    let error = attempt_worktree(&attempt, &fixture.task).unwrap_err();
    assert!(error.contains("worktrees/"), "{error}");

    // Another worktree's git directory, which does sit under `worktrees/`.
    let task_git_dir = git_out(
        &fixture.git,
        fixture.task_worktree(),
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .await;
    fs::write(run.path.join(".git"), format!("gitdir: {task_git_dir}\n")).unwrap();
    let error = attempt_worktree(&attempt, &fixture.task).unwrap_err();
    assert!(error.contains("another worktree"), "{error}");
    assert!(!marker.exists());
}

#[gpui::test]
async fn an_old_record_whose_worktree_is_gone_still_loads(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new().await;
    let gone = fixture.root.join("gone-run").join("run");
    let attempt = Attempt {
        n: 1,
        created: "2026-11-11T10:00:00Z".to_string(),
        definition_hash: "h".to_string(),
        definition_commit: None,
        worktree: gone.to_string_lossy().into_owned(),
        branch: "bongos/workflow-gone".to_string(),
        git_dir: None,
        loop_bound: None,
    };
    let run = attempt_worktree(&attempt, &fixture.task).unwrap();
    assert_eq!(run.path, gone);
    assert_eq!(run.branch, "bongos/workflow-gone");
    assert_eq!(run.git_dir, PathBuf::new());
}
