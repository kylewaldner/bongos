//! Tests of the done decider for groups: a group is Done when its closing checks pass on its
//! last child's tree, the decider runs the missing ones itself on that recorded tree (written out
//! under the scratch folder and removed after), once per key, and keeps the trees with refs.
//! Checks run through the engine on real git trees; time is a counter, never the wall clock.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bongos_checks_model::evidence::StartedBy;
use bongos_checks_model::verdict::Verdict;
use bongos_checks_model::{EvidenceRecord, effective_set};
use bongos_project_repo::{LandingResult, ProjectRepo, StateApplyHooks, StateWriter};
use bongos_project_repo_model::{DefinitionTree, TaskStateRecord};
use bongos_work_model::Status;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::definitions::named_sets;
use crate::engine::{ChecksEngine, RunChecksRequest};
use crate::engine_tests::set_kill_grace;
use crate::evidence_store::{DoneRecord, done_record, put_done, scan_records, task_prefix};
use crate::group_decider::{CHILDREN_HELD, SECOND_TREE_RETRY, as_found, tree_gone};
use crate::live::HoldRow;
use crate::owed::{ChecksOwedHook, FolderSets, LastRow, OwedMarker, put_marker};
use crate::pin::master_definitions;
use crate::pools_tests::FakeClock;
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource};
use crate::task_tree::RunWorktrees;
use crate::test_support::{
    FakeSecond, FakeTrees, commit_everything, definition_toml, fixture_project_with_hooks, git_in,
    init_git_repo, land, main_commit, new_copy, person, write_definition, write_file,
};
use crate::trees::{SecondTree, TaskTrees as _, TaskTreesRegistry};
use crate::{LAST_NAMESPACE, OWED_NAMESPACE, read_hold_rows};

const PROJECT: &str = "project-1";
const TESTS: &str = "\n[[checks.check]]\nname = \"tests\"\ncommand = [\"true\"]\n";
/// A closing check that passes only where the files of the tree were written out.
const CLOSING: &str =
    "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"test\", \"-f\", \"src/lib.rs\"]\n";
const FAILING_CLOSING: &str = "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"false\"]\n";

#[derive(Default)]
struct Source {
    tasks: RefCell<BTreeMap<String, TaskChecks>>,
    repositories: RefCell<BTreeMap<String, PathBuf>>,
}

impl TaskSource for Source {
    fn task(&self, task_id: &str, _: &gpui::App) -> Option<TaskChecks> {
        self.tasks.borrow().get(task_id).cloned()
    }

    fn done_tasks(&self, _: &gpui::App) -> HashSet<String> {
        HashSet::new()
    }

    fn code_repository(&self, name: &str, _: &gpui::App) -> Option<PathBuf> {
        self.repositories.borrow().get(name).cloned()
    }
}

struct NoRuns;

impl RunWorktrees for NoRuns {
    fn active(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }

    fn last_ended(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }
}

/// Fields drop in order: the engine and the project go before the folder holding them.
struct World {
    engine: Entity<ChecksEngine>,
    clock: Rc<FakeClock>,
    source: Rc<Source>,
    repo: ProjectRepo,
    /// The primary checkout of the code repository `app`, where every task's tree lives.
    app: PathBuf,
    scratch: PathBuf,
    root: TempDir,
}

impl World {
    /// The project with `tasks` (folder under `definitions/`, id, checks table) under its top
    /// group, the code repository `app`, the verified commit recorded, the owed hook on its
    /// writer, and the engine with the setting on. No task has a tree yet.
    fn new(cx: &mut TestAppContext, tasks: &[(&str, &str, &str)]) -> Self {
        Self::with_clock(cx, tasks, FakeClock::automatic())
    }

    /// [`World::new`] with the engine on `clock`.
    fn with_clock(
        cx: &mut TestAppContext,
        tasks: &[(&str, &str, &str)],
        clock: Rc<FakeClock>,
    ) -> Self {
        let root = TempDir::new().unwrap();
        let repository = root.path().join("project/repository");
        let hooks = StateApplyHooks::new();
        let hook = Arc::new(ChecksOwedHook::new());
        hook.set_enabled(true);
        hook.set_named_sets(Arc::new(FolderSets(repository.join("definitions"))));
        hooks.register(hook);
        let repo = fixture_project_with_hooks(&root, &hooks);
        for (folder, id, checks) in tasks {
            write_file(
                &repository.join("definitions"),
                &format!("{folder}/task.toml"),
                &definition_toml(id, id, checks),
            );
        }
        commit_everything(&repository, "add the group");
        let app = root.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        init_git_repo(&app);
        write_file(&app, "src/lib.rs", "pub fn one() {}\n");
        commit_everything(&app, "start");
        let scratch = root.path().join("scratch");
        let source = Rc::new(Source::default());
        source
            .repositories
            .borrow_mut()
            .insert("app".to_string(), app.clone());
        let scratch_setting = scratch.to_string_lossy().to_string();
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            TaskChecksSettings::register(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    let checks = content.task_checks.get_or_insert_default();
                    checks.enabled = Some(true);
                    checks.live_write_ms = Some(0);
                    checks.scratch_dir = Some(scratch_setting);
                });
            });
        });
        let engine = new_engine(cx, &source, &clock, 0);
        let world = Self {
            engine,
            clock,
            source,
            repo,
            app,
            scratch,
            root,
        };
        world.record_verified();
        for (_, id, _) in tasks {
            world.know(id, &|_| false);
        }
        world
    }

    /// Tells the task source about `id`, its set judged with `done` saying which tasks are Done.
    fn know(&self, id: &str, done: &dyn Fn(&str) -> bool) {
        let definitions = master_definitions(&self.repo);
        let tree = DefinitionTree::parse_dir(&definitions).expect("the definitions parse");
        let worktree = self
            .source
            .tasks
            .borrow()
            .get(id)
            .and_then(|task| task.worktree.clone());
        let task = TaskChecks {
            task_id: id.to_string(),
            name: id.to_string(),
            repo: Some("app".to_string()),
            worktree,
            set: effective_set(id, &tree, &named_sets(&definitions), done),
            project: self.repo.clone(),
            base_branch: None,
            output_dir: self.root.path().join("output").join(id),
            index_dir: self.root.path().join("index"),
        };
        self.source.tasks.borrow_mut().insert(id.to_string(), task);
    }

    /// Gives `id` a worktree of `app` holding `contents` in `src/lib.rs`, not committed.
    fn worktree(&self, id: &str, contents: &str) -> PathBuf {
        let path = self.root.path().join("trees").join(id);
        git_in(
            &self.app,
            &["worktree", "add", "-q", "-b", id, path.to_str().unwrap()],
        );
        write_file(&path, "src/lib.rs", contents);
        self.source
            .tasks
            .borrow_mut()
            .get_mut(id)
            .expect("a known task")
            .worktree = Some(path.clone());
        path
    }

    fn record_verified(&self) {
        let commit = main_commit(&self.repo);
        let definitions = Arc::new(
            DefinitionTree::parse_dir(&master_definitions(&self.repo)).expect("they parse"),
        );
        self.writer()
            .transact(|tx| tx.set_verified_commit(&commit, definitions.clone()))
            .unwrap();
    }

    /// Lands a change that touches no check, moving the verified commit.
    fn land_unrelated(&self, name: &str, text: &str) {
        let author = person();
        let copy = new_copy(&self.repo, name, &author);
        write_definition(&copy, "plan.md", text);
        let landed = land(&self.repo, &copy, &author);
        assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
    }

    fn writer(&self) -> Arc<StateWriter> {
        self.repo.state().unwrap().writer().unwrap().clone()
    }

    fn set(&self, ids: &[&str], status: Status) {
        self.writer()
            .transact(|tx| {
                for id in ids {
                    tx.set_task(
                        id,
                        TaskStateRecord {
                            status,
                            ..TaskStateRecord::default()
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn finish(&self, ids: &[&str]) {
        self.set(ids, Status::Done);
    }

    async fn run_checks(&self, id: &str, cx: &mut TestAppContext) {
        let report = self
            .engine
            .update(cx, |engine, cx| {
                engine.run(RunChecksRequest::person(id), cx)
            })
            .await;
        assert!(
            !report.results().is_empty()
                && report
                    .results()
                    .iter()
                    .all(|result| result.verdict == Verdict::Pass),
            "{report:?}"
        );
    }

    async fn decide(&self, cx: &mut TestAppContext) {
        let writer = self.writer();
        self.engine
            .update(cx, |engine, cx| engine.decide_once(PROJECT, writer, cx))
            .await;
    }

    fn group_runs(&self, cx: &mut TestAppContext) -> usize {
        self.engine
            .read_with(cx, |engine, _| engine.decider.group_runs.len())
    }

    /// Decides, lets every group run that started end, and decides again, until a decision
    /// starts nothing.
    async fn settle(&self, cx: &mut TestAppContext) {
        for _ in 0..8 {
            self.decide(cx).await;
            if self.group_runs(cx) == 0 {
                return;
            }
            let ended = self
                .engine
                .read_with(cx, |engine, _| engine.group_runs_ended());
            ended.await;
        }
        panic!("the decisions do not settle");
    }

    fn done(&self, id: &str) -> Option<DoneRecord> {
        self.writer().read(|read| done_record(read, id)).unwrap()
    }

    fn hold(&self, id: &str) -> Option<HoldRow> {
        self.writer().read(read_hold_rows).unwrap().remove(id)
    }

    fn marker(&self, id: &str) -> Option<OwedMarker> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(OWED_NAMESPACE)
                    .get(id)?
                    .map(|value| value.try_into().unwrap()))
            })
            .unwrap()
    }

    fn last(&self, group: &str) -> Option<LastRow> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(LAST_NAMESPACE)
                    .get(group)?
                    .map(|value| value.try_into().unwrap()))
            })
            .unwrap()
    }

    fn records(&self, id: &str) -> Vec<EvidenceRecord> {
        self.writer()
            .read(|read| scan_records(read, &task_prefix(id)))
            .unwrap()
    }

    /// The tree the ref `name` of `app` points at, if it exists.
    fn kept(&self, name: &str) -> Option<String> {
        let refs = git_in(
            &self.app,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        refs.lines().find_map(|line| {
            let (refname, object) = line.split_once(' ')?;
            (refname == name).then(|| object.to_string())
        })
    }

    /// What is left in the scratch folder but the folder of tree key indexes.
    fn scratch_left(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.scratch) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "tree-keys")
            .collect()
    }
}

/// A new engine over `source` on `clock`, installed as the app's; its times count from `start`
/// seconds after a fixed hour, never from the wall clock.
fn new_engine(
    cx: &mut TestAppContext,
    source: &Rc<Source>,
    clock: &Rc<FakeClock>,
    start: u64,
) -> Entity<ChecksEngine> {
    let counter = Arc::new(AtomicU64::new(start));
    let source = source.clone();
    let clock = clock.clone();
    cx.update(|cx| {
        ChecksEngine::install(
            ChecksEngine::new(
                source,
                Rc::new(NoRuns),
                clock,
                Arc::new(move || {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    format!("2026-03-01T10:{:02}:{:02}Z", n / 60 % 60, n % 60)
                }),
            ),
            cx,
        )
    })
}

impl World {
    /// Replaces the engine with a new one, as a restart of the process holding the writer
    /// does: the decider remembers nothing of what it saw.
    fn restart(&mut self, cx: &mut TestAppContext) {
        self.engine = new_engine(cx, &self.source, &self.clock, 600);
    }

    /// A second tree of `app` a lane could lend: a detached worktree at `HEAD` with a stray
    /// file an earlier job left and ignored build output.
    fn second_tree(&self) -> PathBuf {
        let exclude = self.app.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, "ignored/\n").unwrap();
        let path = self.root.path().join("second-app");
        git_in(
            &self.app,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        write_file(&path, "stray.txt", "left by an earlier job\n");
        write_file(&path, "ignored/build.out", "build output\n");
        path
    }

    /// Lets the group runs started so far end.
    async fn runs_ended(&self, cx: &mut TestAppContext) {
        let ended = self
            .engine
            .read_with(cx, |engine, _| engine.group_runs_ended());
        ended.await;
    }
}

/// A closing check that passes where the files of the tree were written out, and prints where
/// it ran and the lane mark of its environment (`own` without one).
const WHERE_CLOSING: &str = "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"sh\", \"-c\", \"test -f src/lib.rs && echo pwd=$(pwd -P) && echo mark=${LANE_MARK:-own}\"]\n";

/// What the group's one run printed: where it ran and its lane mark.
fn ran_where(world: &World, group: &str) -> (String, String) {
    let records = world.records(group);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].verdict, Verdict::Pass, "{records:?}");
    let folder = match &records[0].output {
        bongos_checks_model::evidence::Output::Path(path) => PathBuf::from(path),
        other => panic!("no output folder: {other:?}"),
    };
    let text = std::fs::read_to_string(folder.join("part-1.log")).unwrap();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_default()
            .to_string()
    };
    (field("pwd"), field("mark"))
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

/// Lane trees for `app`: `second` answers its second tree; registered as the app's.
fn lane_trees(cx: &mut TestAppContext, second: FakeSecond) -> FakeTrees {
    let trees = FakeTrees::default();
    trees.state().second.insert("app".to_string(), second);
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    trees
}

fn lane_env(mark: &str) -> Vec<(String, String)> {
    vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("LANE_MARK".to_string(), mark.to_string()),
    ]
}

/// The world of a group whose children owe nothing and whose last child c2 has the tree
/// `pub fn c2() {}`, both finished with the decider following: its closing check is ready to run.
fn finished_group(cx: &mut TestAppContext, clock: Rc<FakeClock>) -> (World, PathBuf) {
    let world = World::with_clock(cx, &group(WHERE_CLOSING, ""), clock);
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    (world, tree)
}

#[gpui::test]
async fn a_lent_second_tree_runs_the_closing_check_there_with_its_environment(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (world, tree) = finished_group(cx, FakeClock::automatic());
    let second = world.second_tree();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: second.clone(),
            env: lane_env("lent"),
        },
    );
    let head = git_in(&tree, &["rev-parse", "HEAD"]);
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    let done = world.done("grp").expect("the group is Done");
    assert_eq!(done.tree_key, key_of(&world, &tree));
    assert_eq!(
        ran_where(&world, "grp"),
        (canonical(&second), "lent".to_string())
    );
    // The recorded tree was written out in the lent tree: the stray file went, build output
    // stayed, and the last child's own tree was not touched.
    assert_eq!(
        std::fs::read_to_string(second.join("src/lib.rs")).unwrap(),
        "pub fn c2() {}\n"
    );
    assert!(!second.join("stray.txt").exists());
    assert!(second.join("ignored/build.out").exists());
    assert_eq!(
        std::fs::read_to_string(tree.join("src/lib.rs")).unwrap(),
        "pub fn c2() {}\n"
    );
    assert_eq!(git_in(&tree, &["rev-parse", "HEAD"]), head);
    assert!(!tree.join("ignored").exists() && !tree.join("stray.txt").exists());
    let state = trees.state();
    assert_eq!(state.leased, ["app"]);
    assert_eq!(state.released, ["app"], "released after the run");
    drop(state);
    assert_eq!(
        world.scratch_left(),
        Vec::<String>::new(),
        "nothing ran cold"
    );
}

#[gpui::test]
async fn a_busy_second_tree_is_awaited_and_taken_once_the_lane_releases_it(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let (world, _) = finished_group(cx, clock.clone());
    let second = world.second_tree();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: second.clone(),
            env: lane_env("lent"),
        },
    );
    // The lane holds its second tree now.
    let SecondTree::Lease(held) = trees.second_tree("a-lane-job", "app") else {
        panic!("the lane leases its tree");
    };
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    world.decide(cx).await;
    cx.run_until_parked();

    assert_eq!(world.group_runs(cx), 1, "the run waits");
    assert!(world.records("grp").is_empty(), "nothing ran yet");
    assert_eq!(trees.state().leased, ["app"], "only the lane's own lease");
    assert_eq!(clock.sleeps(), [SECOND_TREE_RETRY]);

    // The lane lets the tree go; the next ask takes it.
    drop(held);
    clock.advance(SECOND_TREE_RETRY);
    world.runs_ended(cx).await;

    assert_eq!(
        ran_where(&world, "grp"),
        (canonical(&second), "lent".to_string())
    );
    let state = trees.state();
    assert_eq!(state.leased, ["app", "app"]);
    assert_eq!(state.released, ["app", "app"]);
    assert!(state.held.is_empty());
}

#[gpui::test]
async fn a_second_tree_busy_past_the_wait_limit_runs_cold(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let clock = FakeClock::automatic();
    let (world, _) = finished_group(cx, clock.clone());
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .task_checks
                    .get_or_insert_default()
                    .pool_wait_limit_hours = Some(1);
            });
        });
    });
    let trees = lane_trees(cx, FakeSecond::Busy);
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    let (pwd, mark) = ran_where(&world, "grp");
    assert!(pwd.starts_with(&canonical(&world.scratch)), "{pwd}");
    assert_eq!(mark, "own", "no lane variables");
    assert!(world.done("grp").is_some());
    // Asked every retry for an hour, then once more at the bound.
    let retries = clock
        .sleeps()
        .iter()
        .filter(|sleep| **sleep == SECOND_TREE_RETRY)
        .count();
    assert_eq!(retries, 240);
    assert_eq!(trees.state().asked.len(), 241);
    assert_eq!(world.scratch_left(), Vec::<String>::new());
}

#[gpui::test]
async fn an_unavailable_second_tree_runs_cold_at_once(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    // The clock never moves: a run that waited on it would never end.
    let clock = FakeClock::manual();
    let (world, _) = finished_group(cx, clock.clone());
    let trees = lane_trees(cx, FakeSecond::Unavailable);
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    let (pwd, mark) = ran_where(&world, "grp");
    assert!(pwd.starts_with(&canonical(&world.scratch)), "{pwd}");
    assert_eq!(mark, "own", "no lane variables");
    assert!(world.done("grp").is_some());
    assert_eq!(trees.state().asked, ["app"]);
    assert!(clock.sleeps().is_empty(), "no wait");
    assert_eq!(world.scratch_left(), Vec::<String>::new());
}

#[gpui::test]
async fn a_lent_tree_that_is_not_usable_runs_cold_and_is_released(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = finished_group(cx, FakeClock::automatic());
    // A folder that is no working tree fails the lent tree's checks.
    let plain = world.root.path().join("not-a-tree");
    std::fs::create_dir_all(&plain).unwrap();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: plain.clone(),
            env: lane_env("lent"),
        },
    );
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    let (pwd, mark) = ran_where(&world, "grp");
    assert!(pwd.starts_with(&canonical(&world.scratch)), "{pwd}");
    assert_eq!(mark, "own", "no lane variables");
    assert!(world.done("grp").is_some());
    let state = trees.state();
    assert_eq!(state.leased, ["app"]);
    assert_eq!(state.released, ["app"], "released once");
    assert!(state.held.is_empty());
    drop(state);
    assert_eq!(
        std::fs::read_dir(&plain).unwrap().count(),
        0,
        "nothing written"
    );
    assert_eq!(world.scratch_left(), Vec::<String>::new());
}

/// A closing check that ignores SIGTERM, marks `ignored/blocked` in the tree it runs in and
/// waits there until `ignored/go` exists.
const BLOCKING_CLOSING: &str = "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"sh\", \"-c\", \"trap '' TERM; touch ignored/blocked; while [ ! -f ignored/go ]; do sleep 0.05; done\"]\n";

#[gpui::test]
async fn a_drain_releases_the_lease_a_group_run_holds_while_its_check_still_runs(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_clock(cx, &group(BLOCKING_CLOSING, ""), clock.clone());
    world.worktree("c1", "pub fn c1() {}\n");
    world.worktree("c2", "pub fn c2() {}\n");
    // SIGKILL comes in real time after the stop: the check outlives SIGTERM, so only the
    // engine's clock can end the drain first.
    set_kill_grace(cx, 5);
    let second = world.second_tree();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: second.clone(),
            env: lane_env("lent"),
        },
    );
    let blocked = second.join("ignored/blocked");
    let (reached, reaching) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !blocked.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the check never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reached.send(()).ok();
    });
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    world.decide(cx).await;
    reaching.await.unwrap();
    assert_eq!(trees.state().leased, ["app"]);
    assert!(
        trees.state().released.is_empty(),
        "held while the check runs"
    );

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    clock.advance(std::time::Duration::from_secs(5));
    drain.await;

    // The grace won: the group run has not ended, and the drain itself let the lease go.
    assert_eq!(world.group_runs(cx), 1);
    assert_eq!(trees.state().released, ["app"]);
    assert!(trees.state().held.is_empty());
    std::fs::write(second.join("ignored/go"), "").unwrap();
    world.runs_ended(cx).await;
    assert_eq!(world.group_runs(cx), 0);
    assert_eq!(trees.state().released, ["app"], "released once");
}

/// Waits on a thread, up to a minute, until `path` exists.
fn reached(path: PathBuf) -> futures::channel::oneshot::Receiver<()> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "{path:?} never came");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        sender.send(()).ok();
    });
    receiver
}

#[gpui::test]
async fn a_drain_while_git_writes_the_lent_tree_releases_the_lease_only_after_git_and_reruns_after_undrain(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let (world, _) = finished_group(cx, clock.clone());
    let second = world.second_tree();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: second.clone(),
            env: lane_env("lent"),
        },
    );
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    // Writing out a Rust file in the lent tree waits until `go` exists.
    let marks = world.root.path().join("filter");
    std::fs::create_dir_all(&marks).unwrap();
    let marks_arg = marks.to_str().unwrap();
    git_in(
        &world.app,
        &[
            "config",
            "filter.slow.smudge",
            &format!(
                "touch {marks_arg}/writing; while [ ! -f {marks_arg}/go ]; do sleep 0.05; done; cat"
            ),
        ],
    );
    std::fs::write(world.app.join(".git/info/attributes"), "*.rs filter=slow\n").unwrap();
    world.decide(cx).await;
    reached(marks.join("writing")).await.unwrap();

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    drain.await;

    // Git still writes in the lent tree: its lease is held there, out of the drain's reach.
    assert_eq!(trees.state().leased, ["app"]);
    assert!(trees.state().released.is_empty(), "held while git runs");
    std::fs::write(marks.join("go"), "").unwrap();
    world.runs_ended(cx).await;
    assert_eq!(trees.state().released, ["app"], "released once git ended");
    assert!(world.records("grp").is_empty(), "nothing ran");
    let started = world
        .engine
        .read_with(cx, |engine, _| engine.decider.started.clone());
    assert!(started.is_empty(), "{started:?}");

    // After the undrain the group runs its closing check after all.
    world.engine.update(cx, |engine, _| engine.undrain());
    world.settle(cx).await;
    assert_eq!(
        ran_where(&world, "grp"),
        (canonical(&second), "lent".to_string())
    );
    assert!(world.done("grp").is_some());
}

#[gpui::test]
async fn a_drain_while_waiting_for_a_busy_tree_starts_nothing_and_reruns_after_undrain(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let (world, _) = finished_group(cx, clock.clone());
    let second = world.second_tree();
    let trees = lane_trees(
        cx,
        FakeSecond::Lease {
            path: second.clone(),
            env: lane_env("lent"),
        },
    );
    let SecondTree::Lease(held) = trees.second_tree("a-lane-job", "app") else {
        panic!("the lane leases its tree");
    };
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    world.decide(cx).await;
    cx.run_until_parked();
    assert_eq!(world.group_runs(cx), 1, "the run waits");

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    drain.await;
    drop(held);
    clock.advance(SECOND_TREE_RETRY);
    world.runs_ended(cx).await;

    assert!(world.records("grp").is_empty(), "nothing ran");
    assert_eq!(trees.state().leased, ["app"], "only the lane's own lease");
    let started = world
        .engine
        .read_with(cx, |engine, _| engine.decider.started.clone());
    assert!(started.is_empty(), "{started:?}");

    world.engine.update(cx, |engine, _| engine.undrain());
    world.settle(cx).await;
    assert_eq!(
        ran_where(&world, "grp"),
        (canonical(&second), "lent".to_string())
    );
    assert!(world.done("grp").is_some());
}

#[gpui::test]
async fn a_restart_before_the_last_key_is_filled_keys_the_tree_as_found(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    // Closed by hand; the process stops before the decider sees it.
    world.finish(&["c1", "c2"]);
    assert_eq!(world.last("grp").and_then(|last| last.tree_key), None);
    world.restart(cx);

    world.decide(cx).await;

    let last = world.last("grp").expect("the last child row");
    assert_eq!(last.id, "c2");
    assert_eq!(
        last.tree_key.as_deref(),
        Some(key_of(&world, &tree).as_str())
    );
    let at = last.found_at.clone().expect("the tree as found");
    let hold = world.hold("grp").expect("held while its check runs");
    assert!(hold.reason.ends_with(&as_found("c2", &at)), "{hold:?}");
    world.settle(cx).await;
    assert_eq!(world.done("grp").map(|done| done.tree_key), last.tree_key);
}

#[gpui::test]
async fn a_restart_after_the_last_childs_tree_went_shows_it_is_gone(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    git_in(
        &world.app,
        &["worktree", "remove", "--force", tree.to_str().unwrap()],
    );
    world.restart(cx);

    world.settle(cx).await;

    assert_eq!(world.last("grp").and_then(|last| last.tree_key), None);
    assert_eq!(world.done("grp"), None);
    assert_eq!(world.hold("grp"), Some(HoldRow::new(tree_gone("c2"))));
    assert!(tree_gone("c2").ends_with("gone; reopen it"));
    assert!(world.records("grp").is_empty(), "nothing ran");
}

#[gpui::test]
async fn at_writer_start_the_ref_of_a_task_deleted_from_the_definitions_is_removed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let mut world = World::new(
        cx,
        &[
            ("tasks/grp", "grp", FAILING_CLOSING),
            ("tasks/grp/tasks/c1", "c1", TESTS),
            ("tasks/grp/tasks/c2", "c2", ""),
        ],
    );
    let first = world.worktree("c1", "pub fn c1() {}\n");
    world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.run_checks("c1", cx).await;
    // Closing together, neither carries the group's check; c2 is recorded last.
    world.finish(&["c1", "c2"]);
    world.settle(cx).await;
    assert!(world.done("c1").is_some(), "c1 is Done");
    assert_eq!(world.last("grp").map(|last| last.id).as_deref(), Some("c2"));
    assert_eq!(world.done("grp"), None, "its closing check fails");
    let key = key_of(&world, &first);
    assert_eq!(world.kept("refs/bongos/checks/c1"), Some(key));
    assert!(world.kept("refs/bongos/checks/c2").is_some());

    // c1 is deleted from the definitions; the process stops before the decider sees it.
    let author = person();
    let copy = new_copy(&world.repo, "calm-heron-0003", &author);
    std::fs::remove_dir_all(copy.path.join("definitions/tasks/grp/tasks/c1")).unwrap();
    let landed = land(&world.repo, &copy, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
    assert_eq!(world.done("c1"), None, "its done record went with it");
    world.restart(cx);

    world.decide(cx).await;

    assert_eq!(world.kept("refs/bongos/checks/c1"), None, "deleted");
    assert!(
        world.kept("refs/bongos/checks/c2").is_some(),
        "the ref of the last child stays"
    );
}

fn missing(count: u32) -> HoldRow {
    HoldRow {
        reason: crate::decider::missing_reason(count as usize),
        unsatisfied: Some(count),
    }
}

/// `grp` with the closing check `closing` over the leaves `c1` and `c2`, each with `checks`.
fn group(
    closing: &'static str,
    checks: &'static str,
) -> [(&'static str, &'static str, &'static str); 3] {
    [
        ("tasks/grp", "grp", closing),
        ("tasks/grp/tasks/c1", "c1", checks),
        ("tasks/grp/tasks/c2", "c2", checks),
    ]
}

/// The tree key of `path` as the decider computes it.
fn key_of(world: &World, path: &Path) -> String {
    futures::executor::block_on(crate::tree_key::tree_key(
        world.repo.git(),
        path,
        &world.root.path().join("index"),
    ))
    .unwrap()
    .as_str()
    .to_string()
}

fn assert_group_run(world: &World, group: &str, verdict: Verdict) {
    let records = world.records(group);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].started_by, StartedBy::GroupDecision);
    assert_eq!(records[0].verdict, verdict);
}

#[gpui::test]
async fn a_group_whose_last_open_child_was_dropped_shows_the_missing_closing_check(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    world.decide(cx).await;
    world.finish(&["c1"]);
    world.decide(cx).await;
    assert_eq!(
        world.marker("c1"),
        None,
        "c2 is still open: c1 carries nothing"
    );
    let keyed = world.last("grp").expect("c1 is recorded");
    assert_eq!(keyed.id, "c1");

    // Dropping the last open child finishes the group, which nobody carried.
    world.set(&["c2"], Status::Cancelled);
    let marker = world.marker("grp").expect("the group is marked");
    assert_eq!(marker.last_child.map(|last| last.id).as_deref(), Some("c1"));
    world.decide(cx).await;
    assert_eq!(world.done("grp"), None);
    assert_eq!(world.hold("grp"), Some(missing(1)));

    world.settle(cx).await;
    let done = world
        .done("grp")
        .expect("Done after the group's run passed");
    assert_eq!(Some(done.tree_key.clone()), keyed.tree_key);
    assert_eq!(
        done.tree_key,
        key_of(&world, &world.root.path().join("trees/c1"))
    );
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(world.hold("grp"), None);
}

#[gpui::test]
async fn a_group_whose_last_child_carried_its_closing_check_is_done_after_the_pass(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c2", "pub fn c2() {}\n");
    world.finish(&["c1"]);
    // c2 finishes last while c1 is Done: it carries the closing check and passes it.
    world.know("c2", &|id| id == "c1");
    world.run_checks("c2", cx).await;
    world.finish(&["c2"]);
    assert_eq!(
        world.marker("c2").map(|marker| marker.closing_from),
        Some(vec!["grp".to_string()])
    );

    world.settle(cx).await;

    let child = world.done("c2").expect("c2 is Done");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(done.tree_key, child.tree_key);
    assert_eq!(done.evidence, child.evidence);
    assert!(world.records("grp").is_empty(), "the group ran nothing");
}

#[gpui::test]
async fn siblings_closing_together_leave_the_closing_check_to_a_group_run_with_no_person(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, TESTS));
    world.worktree("c1", "pub fn c1() {}\n");
    world.worktree("c2", "pub fn c2() {}\n");
    world.run_checks("c1", cx).await;
    world.run_checks("c2", cx).await;
    world.finish(&["c1", "c2"]);
    for id in ["c1", "c2"] {
        assert_eq!(
            world.marker(id).map(|marker| marker.closing_from),
            Some(Vec::new()),
            "{id} carries nothing"
        );
    }

    world.settle(cx).await;

    let last = world.done("c2").expect("c2 is Done");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(done.tree_key, last.tree_key, "run on the last child's tree");
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(done.evidence, vec![world.records("grp")[0].id.clone()]);
    assert_eq!(world.scratch_left(), Vec::<String>::new());
}

#[gpui::test]
async fn a_group_whose_children_owe_nothing_records_its_last_child_and_runs_on_its_tree(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    assert_eq!(world.marker("c1"), None);
    assert_eq!(world.marker("c2"), None);
    let marker = world.marker("grp").expect("the group is marked");
    assert_eq!(marker.last_child.map(|last| last.id).as_deref(), Some("c2"));

    world.settle(cx).await;

    let last = world.last("grp").expect("the last child row");
    assert_eq!(last.id, "c2");
    assert_eq!(
        last.tree_key.as_deref(),
        Some(key_of(&world, &tree).as_str())
    );
    assert_eq!(last.found_at, None, "computed right when c2 was recorded");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(Some(done.tree_key), last.tree_key);
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(
        world.scratch_left(),
        Vec::<String>::new(),
        "the scratch folder is removed"
    );
    // The tree is kept for the group by a ref on c2.
    assert_eq!(world.kept("refs/bongos/checks/c2"), last.tree_key);
}

#[gpui::test]
async fn a_failing_group_run_holds_the_group_and_later_verified_commit_changes_start_nothing(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(FAILING_CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    assert_eq!(world.done("grp"), None);
    assert_eq!(world.hold("grp"), Some(missing(1)));
    assert_eq!(world.scratch_left(), Vec::<String>::new());
    let key = key_of(&world, &tree);
    assert_eq!(world.kept("refs/bongos/checks/c2"), Some(key.clone()));
    // A ref no record backs, left behind.
    git_in(
        &world.app,
        &["update-ref", "refs/bongos/checks/ghost", &key],
    );

    for (name, text) in [("amber-otter-0001", "one\n"), ("bold-crane-0002", "two\n")] {
        let before = main_commit(&world.repo);
        world.land_unrelated(name, text);
        assert_ne!(main_commit(&world.repo), before);
        world.decide(cx).await;
        assert_eq!(world.group_runs(cx), 0, "nothing starts on the same key");
    }

    assert_group_run(&world, "grp", Verdict::Fail);
    assert_eq!(world.hold("grp"), Some(missing(1)));
    assert_eq!(
        world.kept("refs/bongos/checks/ghost"),
        None,
        "the orphan ref is deleted"
    );
    assert_eq!(
        world.kept("refs/bongos/checks/c2"),
        Some(key),
        "a backed ref stays"
    );
}

#[gpui::test]
async fn with_the_last_childs_worktree_removed_the_group_still_becomes_done_unattended(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(
        cx,
        &[
            ("tasks/grp", "grp", CLOSING),
            ("tasks/grp/tasks/c1", "c1", TESTS),
            ("tasks/grp/tasks/c2", "c2", ""),
        ],
    );
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    let key = key_of(&world, &tree);
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    world.decide(cx).await;
    // c1 still owes its checks; c2's tree was keyed as it finished.
    assert_eq!(world.hold("grp"), Some(HoldRow::new(CHILDREN_HELD)));
    assert_eq!(
        world.last("grp").and_then(|last| last.tree_key),
        Some(key.clone())
    );

    // c2's branch merges and its worktree goes, before the group's checks run.
    git_in(
        &world.app,
        &["worktree", "remove", "--force", tree.to_str().unwrap()],
    );
    assert!(!tree.exists());
    world.run_checks("c1", cx).await;

    world.settle(cx).await;

    let done = world.done("grp").expect("Done on the kept tree");
    assert_eq!(done.tree_key, key);
    assert_group_run(&world, "grp", Verdict::Pass);
}

#[gpui::test]
async fn an_empty_done_key_is_no_key_and_the_tree_is_keyed_as_found(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.finish(&["c1", "c2"]);
    // c2 owed checks once, and its done record says no checks were left: no key.
    world
        .writer()
        .transact(|tx| {
            put_marker(tx, "c2", &OwedMarker::default())?;
            put_done(
                tx,
                "c2",
                &DoneRecord {
                    tree_key: String::new(),
                    generation: 9,
                    at: "2026-03-01T09:00:00Z".to_string(),
                    evidence: Vec::new(),
                },
            )
        })
        .unwrap();

    world.decide(cx).await;

    let last = world.last("grp").expect("the last child row");
    let key = key_of(&world, &tree);
    assert_eq!(last.tree_key.as_deref(), Some(key.as_str()));
    let at = last.found_at.clone().expect("keyed as found");
    let hold = world.hold("grp").expect("held while its check runs");
    assert!(hold.reason.ends_with(&as_found("c2", &at)), "{hold:?}");

    world.settle(cx).await;
    assert_eq!(world.done("grp").map(|done| done.tree_key), Some(key));
}

#[gpui::test]
async fn a_nested_group_uses_its_own_last_childs_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let inner_closing =
        "\n[[checks.closing]]\nname = \"inner\"\ncommand = [\"test\", \"-f\", \"src/lib.rs\"]\n";
    let world = World::new(
        cx,
        &[
            ("tasks/outer", "outer", CLOSING),
            ("tasks/outer/tasks/a1", "a1", ""),
            ("tasks/outer/tasks/b-in", "b-in", inner_closing),
            ("tasks/outer/tasks/b-in/tasks/c1", "c1", ""),
            ("tasks/outer/tasks/b-in/tasks/c2", "c2", ""),
        ],
    );
    world.worktree("a1", "pub fn a1() {}\n");
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["a1", "c1", "c2"]);
    assert_eq!(
        world.last("b-in").map(|last| last.id).as_deref(),
        Some("c2")
    );
    assert_eq!(
        world.last("outer").map(|last| last.id).as_deref(),
        Some("b-in")
    );

    world.settle(cx).await;

    let key = key_of(&world, &tree);
    let inner = world.done("b-in").expect("the inner group is Done");
    let outer = world.done("outer").expect("the outer group is Done");
    assert_eq!(inner.tree_key, key);
    assert_eq!(outer.tree_key, key, "the outer group ran on c2's tree");
    assert_group_run(&world, "b-in", Verdict::Pass);
    assert_group_run(&world, "outer", Verdict::Pass);
}
