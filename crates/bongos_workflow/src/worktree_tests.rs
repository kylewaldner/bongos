//! Tests of run worktrees, joins, the built-in merge and `clear finished` over a real temporary git
//! repository. Git runs with an isolated home and no global or system config, so nothing here reads
//! or writes a real checkout.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bongos_workflow_model::record::{EndState, Lifecycle};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::builtin_merge::{
    DIRTY_TASK_WORKTREE, MergeRefusal, MergeRequest, MergeStyle, Merged, merge,
};
use crate::worktrees::{
    FinishedRun, Git, NOT_STARTED, RunWorktree, TaskCheckout, WorktreeError, apply_clear,
    branch_worktree_path, check_free_space, create_branch, create_run, fan_out_branch_name, join,
    plan_clear, run_branch_name, run_dir, run_worktree_path, runs_dir, short_id,
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

    async fn create_run(&self, run_id: &str) -> RunWorktree {
        create_run(&self.git, &self.task, run_id, 1, &settings(), &plenty)
            .await
            .unwrap()
    }

    fn request(&self, run: &RunWorktree, style: MergeStyle) -> MergeRequest {
        MergeRequest {
            task_worktree: self.task_worktree().to_path_buf(),
            task_branch: TASK_BRANCH.to_string(),
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
        FinishedRun {
            run_id: RUN_ID.to_string(),
            lifecycle: Lifecycle::Ended(EndState::Succeeded),
            finished: old,
            worktree: clean.clone(),
        },
        FinishedRun {
            run_id: OTHER_RUN_ID.to_string(),
            lifecycle: Lifecycle::Cancelled,
            finished: old,
            worktree: dirty.clone(),
        },
        FinishedRun {
            run_id: recent_id.to_string(),
            lifecycle: Lifecycle::Ended(EndState::Succeeded),
            finished: SystemTime::now(),
            worktree: recent.clone(),
        },
        FinishedRun {
            run_id: failed_id.to_string(),
            lifecycle: Lifecycle::Ended(EndState::Failed),
            finished: old,
            worktree: failed.clone(),
        },
    ];
    let plan = plan_clear(git, &finished, 14, SystemTime::now())
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
    let finished = vec![FinishedRun {
        run_id: RUN_ID.to_string(),
        lifecycle: Lifecycle::Ended(EndState::Succeeded),
        finished: old,
        worktree: run.clone(),
    }];
    let plan = plan_clear(git, &finished, 14, SystemTime::now())
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
