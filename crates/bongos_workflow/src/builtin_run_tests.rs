//! Tests of the `merge` and `wait` built-ins: a temporary git repository, and a fake clock so no
//! wait really sleeps.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bongos_workflow_model::definition::parse;
use bongos_workflow_model::{FailureReason, NodeKey, RunState};
use tempfile::TempDir;

use crate::builtin_merge::DIRTY_TASK_WORKTREE;
use crate::builtin_nodes::{BuiltinNode, BuiltinStart};
use crate::builtin_run::{MergeNode, WaitClock, WaitNode};
use crate::runtime::RunBinding;
use crate::worktrees::{Git, RunWorktree, TaskCheckout};

const RUN: &str = "01890a5d-ac96-774b-bcce-b30200000001";

fn temp() -> TempDir {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|path| path != Path::new("/tmp"));
    match base {
        Some(base) => TempDir::new_in(base).unwrap(),
        None => TempDir::new().unwrap(),
    }
}

fn test_git(home: &Path) -> Git {
    Git::new("git")
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> String {
    futures::executor::block_on(test_git(home).run(dir, args)).unwrap()
}

fn commit(home: &Path, dir: &Path, file: &str, text: &str) {
    fs::write(dir.join(file), text).unwrap();
    git(home, dir, &["add", file]);
    git(home, dir, &["commit", "-q", "-m", file]);
}

/// A repository on branch `task` with a second branch `run` one commit ahead.
struct Repo {
    _temp: TempDir,
    home: PathBuf,
    dir: PathBuf,
}

fn repo() -> Repo {
    let temp = temp();
    let home = temp.path().join("home");
    let dir = temp.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&dir).unwrap();
    git(&home, &dir, &["init", "-q", "-b", "task"]);
    commit(&home, &dir, "base.txt", "base\n");
    git(&home, &dir, &["branch", "run"]);
    git(&home, &dir, &["checkout", "-q", "run"]);
    commit(&home, &dir, "run.txt", "run\n");
    git(&home, &dir, &["checkout", "-q", "task"]);
    Repo {
        _temp: temp,
        home,
        dir,
    }
}

fn binding(dir: &Path) -> RunBinding {
    RunBinding {
        workflow: "ship".to_string(),
        task_id: "todo-1".to_string(),
        task_dir: dir.to_path_buf(),
        run_dir: dir.to_path_buf(),
        attempt_dir: dir.to_path_buf(),
        worktree: RunWorktree {
            run_dir: dir.to_path_buf(),
            path: dir.to_path_buf(),
            branch: "run".to_string(),
        },
        checkout: TaskCheckout {
            slug: "todo".to_string(),
            worktree: Some(dir.to_path_buf()),
            branch: Some("task".to_string()),
        },
        inputs: Vec::new(),
    }
}

fn definition(node: &str, with: &str) -> bongos_workflow_model::Definition {
    parse(&format!(
        r#"
schema = 1
name = "t"
start = "{node}"

[nodes.{node}]
kind = "program"
use = "{node}"
with = {with}
routes = [{{ to = "done" }}]

[ends.done]
kind = "success"
"#
    ))
    .unwrap()
}

fn start(
    node: &dyn BuiltinNode,
    name: &str,
    with: &str,
    binding: Option<&RunBinding>,
) -> Result<(), bongos_workflow_model::Failure> {
    let definition = definition(name, with);
    let state = RunState::default();
    let key = NodeKey::new(name, 1);
    node.start(BuiltinStart {
        run: RUN,
        definition: &definition,
        state: &state,
        key: &key,
        try_number: 1,
        binding,
    })
}

fn merge_node(repo: &Repo) -> MergeNode {
    MergeNode::new(test_git(&repo.home))
}

#[test]
fn merge_fast_forwards_the_task_branch() {
    let repo = repo();
    let node = merge_node(&repo);
    let binding = binding(&repo.dir);
    start(&node, "merge", "{}", Some(&binding)).unwrap();
    let finished = node.wait_finished(1);
    let result = finished[0].outcome.as_ref().unwrap();
    assert_eq!(result["result"], "merged");
    assert_eq!(
        git(&repo.home, &repo.dir, &["rev-parse", "task"]),
        git(&repo.home, &repo.dir, &["rev-parse", "run"])
    );
    assert!(repo.dir.join("run.txt").exists());
    assert!(node.poll().is_empty());
}

#[test]
fn merge_refuses_a_dirty_task_worktree() {
    let repo = repo();
    fs::write(repo.dir.join("scratch.txt"), "dirty").unwrap();
    let node = merge_node(&repo);
    let binding = binding(&repo.dir);
    start(&node, "merge", "{}", Some(&binding)).unwrap();
    let failure = node.wait_finished(1).remove(0).outcome.unwrap_err();
    assert!(failure.detail.contains(DIRTY_TASK_WORKTREE), "{failure:?}");
    assert!(!repo.dir.join("run.txt").exists());
}

#[test]
fn merge_conflict_aborts_and_names_the_files() {
    let repo = repo();
    // Both branches change base.txt differently.
    git(&repo.home, &repo.dir, &["checkout", "-q", "run"]);
    commit(&repo.home, &repo.dir, "base.txt", "run side\n");
    git(&repo.home, &repo.dir, &["checkout", "-q", "task"]);
    commit(&repo.home, &repo.dir, "base.txt", "task side\n");
    let node = merge_node(&repo);
    let binding = binding(&repo.dir);
    start(&node, "merge", "{}", Some(&binding)).unwrap();
    let failure = node.wait_finished(1).remove(0).outcome.unwrap_err();
    assert_eq!(failure.reason, FailureReason::MergeConflict);
    assert!(failure.detail.contains("base.txt"), "{failure:?}");
    assert_eq!(git(&repo.home, &repo.dir, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.dir.join("base.txt")).unwrap(),
        "task side\n"
    );
}

#[test]
fn a_node_with_no_binding_fails_to_start() {
    let repo = repo();
    let node = merge_node(&repo);
    let failure = start(&node, "merge", "{}", None).unwrap_err();
    assert!(failure.detail.contains("not bound"), "{failure:?}");
}

#[test]
fn recover_aborts_a_half_done_merge() {
    let repo = repo();
    git(&repo.home, &repo.dir, &["checkout", "-q", "run"]);
    commit(&repo.home, &repo.dir, "base.txt", "run side\n");
    git(&repo.home, &repo.dir, &["checkout", "-q", "task"]);
    commit(&repo.home, &repo.dir, "base.txt", "task side\n");
    let node = merge_node(&repo);
    let binding = binding(&repo.dir);
    // The node records the task worktree of a run it started, which recover finds it by.
    start(&node, "merge", "{}", Some(&binding)).unwrap();
    node.wait_finished(1);
    // Half a merge: a conflicted merge nobody aborted.
    let output = futures::executor::block_on(
        test_git(&repo.home).output(&repo.dir, &["merge", "--no-ff", "-m", "m", "run"]),
    )
    .unwrap();
    assert!(!output.status.success());
    assert!(repo.dir.join(".git/MERGE_HEAD").exists());
    node.recover(RUN, &[NodeKey::new("merge", 1)]);
    node.join_recoveries();
    assert!(!repo.dir.join(".git/MERGE_HEAD").exists());
    assert_eq!(git(&repo.home, &repo.dir, &["status", "--porcelain"]), "");
    // With nothing half done, recovering again changes nothing.
    node.recover(RUN, &[NodeKey::new("merge", 1)]);
    node.join_recoveries();
}

/// A clock whose `sleep` moves time forward and runs a hook, without waiting.
struct FakeClock {
    now: Mutex<Duration>,
    sleeps: Mutex<u32>,
    on_sleep: Box<dyn Fn(u32) + Send + Sync>,
}

impl FakeClock {
    fn new(on_sleep: impl Fn(u32) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::default(),
            sleeps: Mutex::default(),
            on_sleep: Box::new(on_sleep),
        })
    }
}

impl WaitClock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        *self.now.lock().unwrap() += duration;
        let count = {
            let mut sleeps = self.sleeps.lock().unwrap();
            *sleeps += 1;
            *sleeps
        };
        (self.on_sleep)(count);
    }
}

fn wait_with(marker: &Path, extra: &str) -> String {
    format!(
        r#"{{ condition = "command", command = ["test", "-f", "{}"], interval_minutes = 5{extra} }}"#,
        marker.display()
    )
}

#[test]
fn wait_reports_waiting_then_satisfied_when_the_clock_advances() {
    let repo = repo();
    let marker = repo.dir.join("ready");
    let created = marker.clone();
    // The marker appears once the clock has moved a few minutes.
    let clock = FakeClock::new(move |count| {
        if count >= 2 {
            fs::write(&created, "").unwrap();
        }
    });
    let node = WaitNode::new(clock.clone(), "gh");
    let binding = binding(&repo.dir);
    start(&node, "wait", &wait_with(&marker, ""), Some(&binding)).unwrap();
    let finished = node.wait_finished(1);
    assert_eq!(finished[0].outcome.as_ref().unwrap()["result"], "satisfied");
    assert!(*clock.sleeps.lock().unwrap() >= 2, "it waited first");
    assert!(clock.now() >= Duration::from_secs(60));
}

#[test]
fn wait_times_out_at_give_up() {
    let repo = repo();
    let marker = repo.dir.join("never");
    let clock = FakeClock::new(|_| {});
    let node = WaitNode::new(clock.clone(), "gh");
    let binding = binding(&repo.dir);
    start(
        &node,
        "wait",
        &wait_with(&marker, ", give_up_minutes = 30"),
        Some(&binding),
    )
    .unwrap();
    let failure = node.wait_finished(1).remove(0).outcome.unwrap_err();
    assert_eq!(failure.reason, FailureReason::WaitTimedOut);
    assert_eq!(failure.detail, "wait timed out");
    assert_eq!(clock.now(), Duration::from_secs(30 * 60));
}
