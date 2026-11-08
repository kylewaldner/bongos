//! `task: start` against real git repositories in temporary directories, with the real git
//! binary: the branch and its base, the exclude line, refusals, and the setup command.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use bongos_work_model::{StackedOn, TodoState, UnblockPoint, layout, serialize_task};
use gpui::{App, Task, TestAppContext, UpdateGlobal as _, VisualTestContext, Window};
use parking_lot::Mutex;
use settings::{SettingsStore, WorkRepositoryContent};
use tempfile::TempDir;
use workspace::TerminalProvider;

use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::store_tests::write;
use crate::worktrees::{
    EXCLUDE_LINE, branch_name, common_git_dir, ensure_excluded, free_worktree_path, pull_number,
};

/// Runs git in `dir` with no user or system configuration, and returns its trimmed output.
// Blocking is fine here: the fixtures are made before, or between, the steps of a test.
#[allow(clippy::disallowed_methods)]
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository on `main` with one commit.
fn init_repository(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "first"]);
}

fn exclude_lines(git_dir: &Path) -> usize {
    std::fs::read_to_string(git_dir.join("info").join("exclude"))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == EXCLUDE_LINE)
        .count()
}

fn worktree_count(repository: &Path) -> usize {
    git(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

/// Lists the repository `invoicing` (the seeded project's) at `path`.
fn set_repository(cx: &mut VisualTestContext, path: &Path, setup_command: Option<&str>) {
    let path = path.to_string_lossy().into_owned();
    let setup_command = setup_command.map(str::to_string);
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories =
                    Some(vec![WorkRepositoryContent {
                        name: "invoicing".to_string(),
                        path,
                        setup_command,
                        merge_is_ship: None,
                        deployments: None,
                        branch_template: None,
                    }]);
            });
        });
    });
}

/// Records every task spawned in a terminal and reports it ran successfully.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<::task::SpawnInTerminal>>>);

impl TerminalProvider for Recorder {
    fn spawn(
        &self,
        task: ::task::SpawnInTerminal,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Option<anyhow::Result<ExitStatus>>> {
        self.0.lock().push(task);
        Task::ready(Some(Ok(ExitStatus::default())))
    }
}

struct Setup {
    harness: Harness,
    /// The repository as the settings name it: a symbolic link to `real`.
    repository: PathBuf,
    real: PathBuf,
    terminal: Recorder,
    _temp: TempDir,
}

async fn setup<'a>(
    cx: &'a mut TestAppContext,
    setup_command: Option<&str>,
) -> (Setup, &'a mut VisualTestContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let temp = TempDir::new().unwrap();
    let real = std::fs::canonicalize(temp.path())
        .unwrap()
        .join("invoicing");
    init_repository(&real);
    let repository = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &repository).unwrap();
    set_repository(cx, &repository, setup_command);
    let terminal = Recorder::default();
    harness.workspace.update(cx, |workspace, _| {
        workspace.set_terminal_provider(terminal.clone())
    });
    (
        Setup {
            harness,
            repository,
            real,
            terminal,
            _temp: temp,
        },
        cx,
    )
}

#[gpui::test]
async fn start_cuts_the_branch_from_main_runs_the_setup_and_a_repeat_makes_nothing(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, Some("make setup")).await;
    let harness = &setup.harness;
    let main = git(&setup.real, &["rev-parse", "main"]);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;

    let worktree = setup.repository.join(".bongos-worktrees").join("backfill");
    let file = harness.file(cx, &["backfill"]);
    assert_eq!(file.branch.as_deref(), Some("backfill"));
    assert_eq!(file.base.as_deref(), Some("main"));
    assert_eq!(file.base_sha.as_deref(), Some(main.as_str()));
    assert_eq!(file.worktree, Some(worktree.to_string_lossy().into_owned()));
    assert!(file.todos[0].started.is_some());
    assert_eq!(file.todos[0].state, TodoState::Open);
    assert!(file.todos[1..].iter().all(|todo| todo.started.is_none()));
    assert_eq!(file.history.last().unwrap().event, "worktree created");
    assert!(!harness.has_toast(cx), "no refusal");

    // The real repository has the branch, checked out at main's commit in the worktree.
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), main);
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "backfill");
    assert_eq!(exclude_lines(&setup.real.join(".git")), 1);
    assert_eq!(git(&setup.real, &["status", "--porcelain"]), "");

    // The setup command ran in a terminal, in the worktree.
    wait_until(cx, "the setup command", |_| {
        !setup.terminal.0.lock().is_empty()
    })
    .await;
    {
        let spawned = setup.terminal.0.lock();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].command.as_deref(), Some("make setup"));
        assert_eq!(spawned[0].cwd.as_deref(), Some(worktree.as_path()));
    }

    // Starting it again reports the worktree and creates nothing.
    let writes = harness.writes(cx);
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the report", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(worktree_count(&setup.real), 2);
    assert_eq!(setup.terminal.0.lock().len(), 1);
    assert_eq!(exclude_lines(&setup.real.join(".git")), 1);
}

#[gpui::test]
async fn a_stacked_task_is_refused_while_blocked_then_branches_from_its_parent(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let beta_dir = harness.task_dir(&["phase", "beta"]);
    let mut beta = harness.file(cx, &["phase", "beta"]);
    beta.stacked_on = Some(StackedOn {
        task: "alpha".to_string(),
        unblock: UnblockPoint::Implemented,
        extra: Default::default(),
    });
    write(
        &layout::task_file(&beta_dir),
        &serialize_task(&beta).unwrap(),
    );
    wait_until(cx, "the edge to load", |cx| {
        harness.file(cx, &["phase", "beta"]).stacked_on.is_some()
    })
    .await;

    // Blocked: alpha has not reached Implemented.
    harness.clear_toasts(cx);
    let writes = harness.writes(cx);
    harness.run(cx, "task::Start", &[Input::Pick("beta")]).await;
    wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(harness.file(cx, &["phase", "beta"]).worktree, None);
    assert!(!setup.real.join(".bongos-worktrees").join("beta").exists());

    // Alpha starts from main, gains a commit and is implemented.
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("alpha")])
        .await;
    let alpha_worktree = setup.real.join(".bongos-worktrees").join("alpha");
    git(
        &alpha_worktree,
        &["commit", "-q", "--allow-empty", "-m", "alpha work"],
    );
    let alpha_head = git(&setup.real, &["rev-parse", "alpha"]);
    assert_ne!(alpha_head, git(&setup.real, &["rev-parse", "main"]));
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("alpha")])
        .await;

    // Beta now branches from alpha's branch, at its head.
    harness.clear_toasts(cx);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("beta")])
        .await;
    let beta = harness.file(cx, &["phase", "beta"]);
    assert_eq!(beta.branch.as_deref(), Some("beta"));
    assert_eq!(beta.base.as_deref(), Some("alpha"));
    assert_eq!(beta.base_sha.as_deref(), Some(alpha_head.as_str()));
    let beta_worktree = setup.real.join(".bongos-worktrees").join("beta");
    assert_eq!(git(&beta_worktree, &["rev-parse", "HEAD"]), alpha_head);
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn an_occupied_path_or_a_taken_branch_is_refused_and_left_alone(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, Some("make setup")).await;
    let harness = &setup.harness;
    let occupied = setup.real.join(".bongos-worktrees").join("backfill");
    write(&occupied.join("notes.txt"), "keep me");
    git(&setup.real, &["branch", "alpha"]);

    for slug in ["backfill", "alpha"] {
        harness.clear_toasts(cx);
        let writes = harness.writes(cx);
        dispatch(cx, "task::Start");
        harness.answer(cx, &Input::Pick(slug)).await;
        wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
        assert_eq!(harness.writes(cx), writes, "{slug}");
    }
    assert_eq!(
        std::fs::read_to_string(occupied.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(harness.file(cx, &["backfill"]).worktree, None);
    assert_eq!(harness.file(cx, &["phase", "alpha"]).worktree, None);
    assert_eq!(worktree_count(&setup.real), 1);
    assert_eq!(
        git(&setup.real, &["branch", "--list", "backfill"]),
        "",
        "no branch was made"
    );
    assert!(setup.terminal.0.lock().is_empty());
}

#[test]
fn the_exclude_line_is_added_once_also_through_a_linked_worktree() {
    let temp = TempDir::new().unwrap();
    let main = std::fs::canonicalize(temp.path()).unwrap().join("main");
    init_repository(&main);
    let git_dir = main.join(".git");
    std::fs::write(git_dir.join("info").join("exclude"), "*.log").unwrap();

    ensure_excluded(&main).unwrap();
    ensure_excluded(&main).unwrap();
    let text = std::fs::read_to_string(git_dir.join("info").join("exclude")).unwrap();
    assert_eq!(text, format!("*.log\n{EXCLUDE_LINE}\n"));

    // A repository that is itself a linked worktree writes to the common dir, creating `info/`.
    std::fs::remove_dir_all(git_dir.join("info")).unwrap();
    let linked = main.parent().unwrap().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::canonicalize(common_git_dir(&linked).unwrap()).unwrap(),
        git_dir
    );
    ensure_excluded(&linked).unwrap();
    ensure_excluded(&linked).unwrap();
    assert_eq!(exclude_lines(&git_dir), 1);
    assert_eq!(exclude_lines(&git_dir.join("worktrees").join("linked")), 0);
    assert!(!linked.join(".gitignore").exists() && !main.join(".gitignore").exists());
}

#[test]
fn branch_names_paths_and_pull_numbers() {
    assert_eq!(branch_name("{slug}", "backfill", None), "backfill");
    assert_eq!(
        branch_name("kyle/{slug}", "backfill", Some("INV-7")),
        "kyle/backfill"
    );
    assert_eq!(
        branch_name("{linear}-{slug}", "backfill", Some("inv-7")),
        "inv-7-backfill"
    );
    assert_eq!(branch_name("{linear}-{slug}", "backfill", None), "backfill");
    assert_eq!(
        branch_name("kyle/{linear}/{slug}", "backfill", None),
        "kyle/backfill"
    );
    assert_eq!(branch_name("{linear}", "backfill", None), "backfill");

    let repository = Path::new("/src/invoicing");
    let held = [repository.join(".bongos-worktrees/backfill")];
    assert_eq!(
        free_worktree_path(repository, "backfill", &held),
        (
            repository.join(".bongos-worktrees/backfill-2"),
            "backfill-2".to_string()
        )
    );
    assert_eq!(
        free_worktree_path(repository, "alpha", &held).0,
        repository.join(".bongos-worktrees/alpha")
    );

    assert_eq!(pull_number("https://github.com/o/r/pull/12"), Some(12));
    assert_eq!(
        pull_number("https://github.com/o/r/pull/12/files"),
        Some(12)
    );
    assert_eq!(pull_number("https://example.com/o/r/issues/12"), None);
}
