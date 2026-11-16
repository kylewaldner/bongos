use std::sync::Arc;

use bongos_checks_model::EvidenceRecord;
use bongos_checks_model::evidence::{Output, StartedBy, TreeKind, Variant};
use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{
    Git, Identity, LandMode, LandRequest, LandingResult, Location, MAIN_BRANCH, ProjectRepo,
    StateApplyHooks, StateHandle, StateStore, StateWriter, WorkingCopy,
};
use bongos_project_repo_model::{Author, ChangeMeta, PersonAtKeyboard};
use bongos_work_model::commands::Context;
use futures::executor::block_on;
use tempfile::TempDir;

/// A project location whose parts are all inside `root`, with an empty state directory.
pub(crate) fn location(root: &TempDir) -> Location {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    }
}

/// A writer with its own hook registry, so tests never share the global one.
pub(crate) fn writer(root: &TempDir) -> Arc<StateWriter> {
    let location = location(root);
    Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap())
}

pub(crate) const PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n";

/// The text of a task definition of `kind` other, followed by `extra`.
pub(crate) fn definition_toml(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
         created = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// A new-format project in a real git repository inside `root`: the top group `g1` with the leaf
/// `backfill` (`l1`) committed on `main`, and the state store open with its writer.
pub(crate) fn fixture_project(root: &TempDir) -> ProjectRepo {
    fixture_project_with_hooks(root, &StateApplyHooks::new())
}

/// [`fixture_project`] whose writer runs `hooks`.
pub(crate) fn fixture_project_with_hooks(root: &TempDir, hooks: &StateApplyHooks) -> ProjectRepo {
    let location = location(root);
    let definitions = location.repository.join("definitions");
    for (path, text) in [
        ("project.toml", PROJECT_TOML.to_string()),
        ("task.toml", definition_toml("g1", "Invoicing", "")),
        ("plan.md", String::new()),
        (
            "tasks/backfill/task.toml",
            definition_toml("l1", "Backfill", ""),
        ),
    ] {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    block_on(async {
        git.init_repo(&location.repository, MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer = Arc::new(StateStore::open_writer_with_hooks(&location, hooks).unwrap());
    ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer))
}

fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn keyboard() -> Context {
    Context::at_keyboard(PersonAtKeyboard::for_test())
}

pub(crate) fn person() -> Author {
    keyboard().author
}

pub(crate) fn new_copy(repo: &ProjectRepo, name: &str, author: &Author) -> WorkingCopy {
    block_on(repo.create_working_copy(name, "work/", author, start())).unwrap()
}

/// Writes `text` at `relative` under the working copy's `definitions/`.
pub(crate) fn write_definition(copy: &WorkingCopy, relative: &str, text: &str) {
    let path = copy.path.join("definitions").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Lands the working copy as the person at the keyboard.
pub(crate) fn land(repo: &ProjectRepo, copy: &WorkingCopy, author: &Author) -> LandingResult {
    block_on(repo.land_as_person(
        &copy.change_id,
        LandRequest {
            meta: ChangeMeta::new("land it", author.clone()),
            mode: LandMode::Land,
        },
        keyboard(),
        start(),
    ))
    .unwrap()
}

pub(crate) fn main_commit(repo: &ProjectRepo) -> String {
    block_on(
        repo.git()
            .run(&repo.location().repository, &["rev-parse", "main"]),
    )
    .unwrap()
}

/// The reasons of a refused landing, or a panic naming the result.
pub(crate) fn refusals(result: LandingResult) -> Vec<String> {
    match result {
        LandingResult::Refused { reasons } => reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A finished record of task `task_id`'s check `check` on tree `tree_key`.
pub(crate) fn record(task_id: &str, tree_key: &str, check: &str, id: &str) -> EvidenceRecord {
    EvidenceRecord {
        id: id.to_string(),
        task_id: task_id.to_string(),
        check: check.to_string(),
        digest: "d1".to_string(),
        tree_key: tree_key.to_string(),
        tree: TreeKind::Tip,
        base: None,
        key_inputs: "k1".to_string(),
        variant: Variant {
            scope: bongos_checks_model::evidence::Scope::Items {
                digest: "s1".to_string(),
                count: 6,
            },
            parts: 1,
            params: None,
        },
        pin_commit: "c1".to_string(),
        master_path: "/pin/master".to_string(),
        command: vec!["make".to_string(), "test".to_string()],
        started: "2026-03-01T10:00:00Z".to_string(),
        ended: Some("2026-03-01T10:00:02Z".to_string()),
        duration_ms: Some(2000),
        verdict: Verdict::Pass,
        reason: None,
        exit: Some(bongos_checks_model::Exit::Code(0)),
        summary: Some("6 passed".to_string()),
        failing: Vec::new(),
        scope_reason: None,
        started_by: StartedBy::Person,
        parts: Vec::new(),
        output: Output::Path("checks/out.txt".to_string()),
        cut: 0,
    }
}

/// Runs the system `git` in `directory` with no global or system configuration and a pinned
/// author, committer and date, so commits never depend on the machine or the clock. Returns its
/// trimmed standard output and panics when git fails.
pub(crate) fn git_in(directory: &std::path::Path, args: &[&str]) -> String {
    let mut command = util::command::new_command("git");
    command
        .current_dir(directory)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T10:00:00Z")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_DATE", "2026-01-01T10:00:00Z");
    let output = block_on(command.output()).unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A new repository on branch `main` in `directory`.
pub(crate) fn init_git_repo(directory: &std::path::Path) {
    git_in(directory, &["init", "-q", "-b", "main"]);
}

/// Writes `contents` to `name` inside `directory`, creating parent folders.
pub(crate) fn write_file(directory: &std::path::Path, name: &str, contents: &str) {
    let path = directory.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Stages everything and commits it as `message`.
pub(crate) fn commit_everything(directory: &std::path::Path, message: &str) {
    git_in(directory, &["add", "-A"]);
    git_in(directory, &["commit", "-q", "-m", message]);
}

/// Set in the child process of [`rerun_with_hostile_git_env`].
pub(crate) const HOSTILE_CHILD: &str = "BONGOS_TEST_HOSTILE_GIT_ENV_CHILD";

/// Runs the test `module::name` again in a child process whose environment points git at
/// `decoy` (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`) and carries `extra`, and panics unless
/// the child passes. The process-wide environment of the running test is never changed, so
/// other tests running in threads are not affected. The test calls this when `HOSTILE_CHILD` is
/// unset and does its checks, with the values `extra` carries, when it is set.
pub(crate) fn rerun_with_hostile_git_env(
    module: &str,
    name: &str,
    decoy: &std::path::Path,
    extra: &[(&str, String)],
) {
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    let output = block_on(
        util::command::new_command(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{name}"), "--nocapture"])
            .env(HOSTILE_CHILD, "1")
            .env("GIT_DIR", decoy.join(".git"))
            .env("GIT_WORK_TREE", decoy)
            .env("GIT_INDEX_FILE", decoy.join(".git/index"))
            .envs(extra.iter().map(|(key, value)| (*key, value.as_str())))
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

/// What [`FakeTrees::second_tree`] answers for one repository.
#[derive(Clone, Debug)]
pub(crate) enum FakeSecond {
    /// Lends the tree at `path` with `env`; `Busy` while that lease lives.
    Lease {
        path: std::path::PathBuf,
        env: Vec<(String, String)>,
    },
    Busy,
    Unavailable,
}

/// The state of a [`FakeTrees`], set and read by the test.
#[derive(Default)]
pub(crate) struct FakeTreesState {
    pub tips: std::collections::BTreeMap<String, std::path::PathBuf>,
    pub envs: std::collections::BTreeMap<String, Vec<(String, String)>>,
    /// A repository with no entry is `Unavailable`.
    pub second: std::collections::BTreeMap<String, FakeSecond>,
    /// Every repository `second_tree` was asked for, in order.
    pub asked: Vec<String>,
    /// Every repository a lease was handed out for, in order.
    pub leased: Vec<String>,
    /// Every repository whose lease was dropped, in order, once per lease.
    pub released: Vec<String>,
    /// The repositories leased now.
    pub held: std::collections::BTreeSet<String>,
}

/// A [`crate::trees::TaskTrees`] over the repositories the test names; a lease records its
/// release when it is dropped.
#[derive(Clone, Default)]
pub(crate) struct FakeTrees(pub std::rc::Rc<std::cell::RefCell<FakeTreesState>>);

impl FakeTrees {
    pub(crate) fn state(&self) -> std::cell::RefMut<'_, FakeTreesState> {
        self.0.borrow_mut()
    }
}

impl crate::trees::TaskTrees for FakeTrees {
    fn tip(&self, _: &str, repo: &str) -> Option<std::path::PathBuf> {
        self.0.borrow().tips.get(repo).cloned()
    }

    fn second_tree(&self, _: &str, repo: &str) -> crate::trees::SecondTree {
        let mut state = self.0.borrow_mut();
        state.asked.push(repo.to_string());
        if state.held.contains(repo) {
            return crate::trees::SecondTree::Busy;
        }
        match state.second.get(repo).cloned() {
            Some(FakeSecond::Lease { path, env }) => {
                state.held.insert(repo.to_string());
                state.leased.push(repo.to_string());
                crate::trees::SecondTree::Lease(Box::new(FakeLease {
                    repo: repo.to_string(),
                    path,
                    env,
                    state: self.0.clone(),
                }))
            }
            Some(FakeSecond::Busy) => crate::trees::SecondTree::Busy,
            Some(FakeSecond::Unavailable) | None => crate::trees::SecondTree::Unavailable,
        }
    }

    fn env(&self, _: &str, repo: &str) -> Vec<(String, String)> {
        self.0.borrow().envs.get(repo).cloned().unwrap_or_default()
    }
}

struct FakeLease {
    repo: String,
    path: std::path::PathBuf,
    env: Vec<(String, String)>,
    state: std::rc::Rc<std::cell::RefCell<FakeTreesState>>,
}

impl crate::trees::SecondTreeLease for FakeLease {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn env(&self) -> Vec<(String, String)> {
        self.env.clone()
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.held.remove(&self.repo);
        state.released.push(self.repo.clone());
    }
}

/// Every file of the working tree at `tree` with its bytes, outside `.git`, plus `.git/HEAD`,
/// `.git/index` and the commit `HEAD` names, so two snapshots are equal only when the tree's
/// files and `HEAD` are byte-identical.
pub(crate) fn tree_snapshot(
    tree: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                if path.file_name() != Some(std::ffi::OsStr::new(".git")) {
                    walk(root, &path, out);
                }
            } else {
                let bytes = if kind.is_symlink() {
                    std::fs::read_link(&path)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes()
                } else {
                    std::fs::read(&path).unwrap()
                };
                out.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(tree, tree, &mut out);
    for name in [".git/HEAD", ".git/index"] {
        out.insert(name.into(), std::fs::read(tree.join(name)).unwrap());
    }
    out.insert(
        "HEAD commit".into(),
        git_in(tree, &["rev-parse", "HEAD"]).into_bytes(),
    );
    out
}
