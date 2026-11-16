//! Tests of the checks engine: runs through fakes of the task source, the workflow runs, the
//! trees and the pools, with a counter for time. No test starts an agent or calls a model, and
//! none depends on the calendar.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bongos_checks_model::evidence::EvidenceRecord;
use bongos_checks_model::section::Live;
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_project_repo::{ProjectRepo, StateWriter};
use futures::channel::oneshot;
use gpui::{BorrowAppContext as _, Entity, Task, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::command_logic::{
    DUMP_FILE, NO_TASK_MESSAGE, Target, choose_target, report_message, write_dump,
};
use crate::engine::{
    ChecksEngine, ChecksEvent, Refusal, RunChecksRequest, RunOutcome, RunReport, plan_rows,
};
use crate::evidence_store::{LIVE_NAMESPACE, scan_records, task_prefix};
use crate::live::{LiveRow, live_rows};
use crate::pin::master_definitions;
use crate::pools::CheckPoolsRegistry;
use crate::pools_tests::{FakeClock, FakePool};
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource, effective_set_in};
use crate::task_tree::{RunWorktrees, TaskTree};
use crate::test_support::{
    commit_everything, definition_toml, fixture_project, init_git_repo, write_file,
};
use crate::trees::{SecondTree, TaskTrees, TaskTreesRegistry};

const TASK: &str = "l1";
const TASK_NAME: &str = "Backfill";
const GROUP: &str = "g1";
const SCRIPTS: [&str; 3] = ["part-log", "check-log", "where-log"];

/// `tests` runs in three parts; `lint` is accepted by `tests`, so `tests` runs right after it.
const CHECKS: &str = "
[[checks.check]]
name = \"tests\"
run = \"checks/part-log\"
parts = 3

[[checks.check]]
name = \"lint\"
run = \"checks/check-log\"
or_accepted = \"tests\"

[[checks.check]]
name = \"docs\"
run = \"checks/check-log\"

[[checks.check]]
name = \"gpu\"
run = \"checks/check-log\"
needs = [\"gpu\"]
";

#[derive(Default)]
pub(crate) struct FakeSource {
    tasks: RefCell<std::collections::BTreeMap<String, TaskChecks>>,
}

impl TaskSource for FakeSource {
    fn task(&self, task_id: &str, _: &gpui::App) -> Option<TaskChecks> {
        self.tasks.borrow().get(task_id).cloned()
    }
}

/// The workflow engine's answers, set by the test; it is only ever asked.
#[derive(Default)]
struct FakeRuns {
    active: RefCell<Option<(String, PathBuf)>>,
    ended: RefCell<Option<(String, PathBuf)>>,
    asked: Cell<u32>,
}

impl RunWorktrees for FakeRuns {
    fn active(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        self.asked.set(self.asked.get() + 1);
        self.active.borrow().clone()
    }

    fn last_ended(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        self.ended.borrow().clone()
    }
}

struct FakeTips(PathBuf);

impl TaskTrees for FakeTips {
    fn tip(&self, _: &str, _: &str) -> Option<PathBuf> {
        Some(self.0.clone())
    }

    fn second_tree(&self, _: &str, _: &str) -> SecondTree {
        SecondTree::Unavailable
    }

    fn env(&self, _: &str, _: &str) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Fields drop in order: the engine and the project go before the folder holding them.
pub(crate) struct World {
    source: Rc<FakeSource>,
    runs: Rc<FakeRuns>,
    pub(crate) events: Rc<RefCell<Vec<ChecksEvent>>>,
    pub(crate) engine: Entity<ChecksEngine>,
    repo: ProjectRepo,
    pub(crate) tree: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) index: PathBuf,
    _root: TempDir,
}

/// A project whose master holds the fixture scripts and the checks of task `l1`.
fn project(root: &TempDir, checks: &str) -> ProjectRepo {
    let repo = fixture_project(root);
    let repository = repo.location().repository.clone();
    let task_file = repository.join("definitions/tasks/backfill/task.toml");
    std::fs::write(&task_file, definition_toml(TASK, TASK_NAME, checks)).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-fixtures");
    let checks = repository.join("definitions/checks");
    std::fs::create_dir_all(&checks).unwrap();
    for script in SCRIPTS {
        let target = checks.join(script);
        std::fs::copy(fixtures.join(script), &target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    commit_everything(&repository, "add checks");
    repo
}

/// A git working tree whose test output is ignored, so a run never changes its key.
fn work_tree(root: &TempDir) -> PathBuf {
    let tree = root.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    init_git_repo(&tree);
    write_file(
        &tree,
        ".gitignore",
        "runs\norder\nblock-at\nignore-term\nblocked-*\nscope-*\nitems-*\n",
    );
    write_file(&tree, "src/lib.rs", "pub fn one() {}\n");
    commit_everything(&tree, "start");
    tree
}

impl World {
    /// A world with the project, the task `l1` (with `worktree` as its file's worktree when
    /// `with_tree`) and the group `g1`, which has no checks.
    pub(crate) fn new(cx: &mut TestAppContext, with_tree: bool) -> Self {
        Self::with_clock(cx, with_tree, FakeClock::automatic())
    }

    fn with_clock(cx: &mut TestAppContext, with_tree: bool, clock: Rc<FakeClock>) -> Self {
        Self::with_checks(cx, with_tree, clock, CHECKS)
    }

    /// A world whose task `l1` declares `checks`.
    pub(crate) fn with_checks(
        cx: &mut TestAppContext,
        with_tree: bool,
        clock: Rc<FakeClock>,
        checks: &str,
    ) -> Self {
        let root = TempDir::new().unwrap();
        let repo = project(&root, checks);
        let tree = work_tree(&root);
        let output = root.path().join("output");
        let index = root.path().join("index");
        let source = Rc::new(FakeSource::default());
        let runs = Rc::new(FakeRuns::default());
        let events = Rc::new(RefCell::new(Vec::new()));
        let counter = Arc::new(AtomicU64::new(0));
        let engine = cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            TaskChecksSettings::register(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    let checks = content.task_checks.get_or_insert_default();
                    checks.enabled = Some(true);
                    checks.kill_grace_seconds = Some(1);
                });
            });
            let engine = ChecksEngine::new(
                source.clone(),
                runs.clone(),
                clock.clone(),
                // A counter, never the wall clock.
                Arc::new(move || {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    format!(
                        "2026-03-01T{:02}:{:02}:{:02}Z",
                        n / 3600,
                        n / 60 % 60,
                        n % 60
                    )
                }),
            );
            let engine = ChecksEngine::install(engine, cx);
            let seen = events.clone();
            cx.subscribe(&engine, move |_, event: &ChecksEvent, _| {
                seen.borrow_mut().push(event.clone())
            })
            .detach();
            engine
        });
        let world = Self {
            source,
            runs,
            events,
            engine,
            repo,
            tree: tree.clone(),
            output,
            index,
            _root: root,
        };
        for (id, name) in [(TASK, TASK_NAME), (GROUP, "Invoicing")] {
            let task = world.task(id, name, with_tree.then(|| tree.clone()));
            world.source.tasks.borrow_mut().insert(id.to_string(), task);
        }
        world
    }

    fn task(&self, id: &str, name: &str, worktree: Option<PathBuf>) -> TaskChecks {
        TaskChecks {
            task_id: id.to_string(),
            name: name.to_string(),
            repo: Some("app".to_string()),
            worktree,
            set: effective_set_in(&master_definitions(&self.repo), id, &|_| false)
                .expect("the definitions parse"),
            project: self.repo.clone(),
            base_branch: Some("main".to_string()),
            output_dir: self.output.clone(),
            index_dir: self.index.clone(),
        }
    }

    pub(crate) fn run(
        &self,
        cx: &mut TestAppContext,
        request: RunChecksRequest,
    ) -> Task<RunReport> {
        self.engine.update(cx, |engine, cx| engine.run(request, cx))
    }

    pub(crate) fn only(&self, checks: &[&str]) -> RunChecksRequest {
        RunChecksRequest {
            checks: Some(checks.iter().map(|check| check.to_string()).collect()),
            ..RunChecksRequest::person(TASK)
        }
    }

    /// The folder holding everything of the world.
    pub(crate) fn root(&self) -> &Path {
        self._root.path()
    }

    pub(crate) fn writer(&self) -> Arc<StateWriter> {
        self.repo.state().unwrap().writer().unwrap().clone()
    }

    /// The records of `check`, oldest first.
    pub(crate) fn records(&self, check: &str) -> Vec<EvidenceRecord> {
        let mut records: Vec<EvidenceRecord> = self
            .writer()
            .read(|read| scan_records(read, &task_prefix(TASK)))
            .unwrap()
            .into_iter()
            .filter(|record| record.check == check)
            .collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        records
    }

    pub(crate) fn write(&self, name: &str, text: &str) {
        std::fs::write(self.tree.join(name), text).unwrap();
    }

    pub(crate) fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.tree.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub(crate) fn forget(&self, name: &str) {
        std::fs::remove_file(self.tree.join(name)).ok();
    }

    /// The checks that ended, in order, with their verdicts.
    fn finished(&self) -> Vec<(String, Verdict)> {
        self.events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                ChecksEvent::Finished { check, verdict, .. } => {
                    Some((check.clone(), verdict.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn task_tree(&self, cx: &mut TestAppContext, task: &TaskChecks) -> Option<TaskTree> {
        self.engine
            .read_with(cx, |engine, cx| engine.task_tree(task, cx))
    }
}

fn names(report: &RunReport) -> Vec<&str> {
    report
        .results()
        .iter()
        .map(|result| result.check.as_str())
        .collect()
}

fn class(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::NoVerdict(_) => "no verdict",
    }
}

#[gpui::test]
async fn engine_run_checks_runs_in_set_order_and_needs_no_agent_or_model(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);

    let report = world.run(cx, RunChecksRequest::person(TASK)).await;

    // The set says tests, lint, docs, gpu; lint names tests in `or_accepted`, so tests runs right
    // after lint.
    assert_eq!(names(&report), ["lint", "tests", "docs", "gpu"]);
    assert!(
        report
            .results()
            .iter()
            .all(|result| result.verdict == Verdict::Pass && result.record_id.is_some()),
        "{report:?}"
    );
    assert!(!report.cancelled());
    // One after another: the one-part checks logged their names in that order, and the three
    // parts of `tests` ran once each.
    assert_eq!(world.lines("order"), ["lint", "docs", "gpu"]);
    assert_eq!(world.lines("runs"), ["1", "2", "3"]);
    for check in ["lint", "tests", "docs", "gpu"] {
        assert_eq!(world.records(check).len(), 1, "{check}");
    }
    // No agent and no model: nothing but the seams was consulted, and no workflow engine, the
    // only thing that starts an agent, exists in this app.
    assert!(world.runs.asked.get() > 0, "the run asked for the tree");
    cx.update(|cx| {
        assert!(cx.try_global::<bongos_workflow::WorkflowEngine>().is_none());
    });
    // The section reads the records the run wrote.
    let status = world
        .engine
        .read_with(cx, |engine, cx| engine.status(TASK, cx))
        .expect("the task is known");
    assert_eq!(status.section.header, "all required checks pass");
    assert_eq!(status.section.rows.len(), 4);
    assert!(!status.running);
}

#[gpui::test]
async fn engine_events_report_each_part_and_the_verdict_of_each_check(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);

    let report = world.run(cx, world.only(&["tests"])).await;
    assert_eq!(names(&report), ["tests"]);

    let events = world.events.borrow().clone();
    let at = events
        .iter()
        .position(|event| matches!(event, ChecksEvent::Finished { .. }))
        .expect("a Finished event");
    assert_eq!(
        events[at],
        ChecksEvent::Finished {
            task_id: TASK.to_string(),
            check: "tests".to_string(),
            verdict: Verdict::Pass,
        }
    );
    // The run started, each of the three parts started, and the record ended: all before the
    // verdict, each telling the view to redraw.
    let before = events[..at]
        .iter()
        .filter(|event| matches!(event, ChecksEvent::Changed { task_id } if task_id == TASK))
        .count();
    assert!(before >= 5, "{before} Changed events: {events:?}");
    assert!(matches!(events.last(), Some(ChecksEvent::Changed { .. })));
    assert_eq!(world.finished().len(), 1);
}

#[gpui::test]
async fn engine_run_check_runs_one_named_check_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);

    let report = world.run(cx, world.only(&["docs"])).await;

    assert_eq!(names(&report), ["docs"]);
    assert_eq!(world.lines("order"), ["docs"]);
    assert!(world.records("lint").is_empty());
    assert!(world.records("tests").is_empty());
}

#[gpui::test]
async fn engine_a_task_with_no_tree_is_refused_with_the_reason(cx: &mut TestAppContext) {
    let world = World::new(cx, false);

    let report = world.run(cx, RunChecksRequest::person(TASK)).await;

    let refusal = report.refusal().expect("refused").clone();
    assert_eq!(
        refusal,
        Refusal::NoTree {
            task: TASK_NAME.to_string()
        }
    );
    assert_eq!(
        refusal.to_string(),
        "Backfill has no working tree; checks need one"
    );
    assert!(matches!(
        refusal.no_verdict(),
        Some(NoVerdictReason::TreeUnavailable(_))
    ));
    assert!(report.results().is_empty());
    assert!(world.records("tests").is_empty());
    assert!(world.events.borrow().is_empty());
    // The section says the same.
    let status = world
        .engine
        .read_with(cx, |engine, cx| engine.status(TASK, cx))
        .unwrap();
    assert_eq!(status.section.header, "not done: no tree to check");
}

#[gpui::test]
async fn engine_a_task_with_no_checks_is_refused_with_the_reason(cx: &mut TestAppContext) {
    let world = World::new(cx, true);

    let group = world.run(cx, RunChecksRequest::person(GROUP)).await;
    let unknown = world.run(cx, RunChecksRequest::person("nobody")).await;
    let missing = world.run(cx, world.only(&["nothing"])).await;

    assert_eq!(
        group.refusal().map(ToString::to_string).as_deref(),
        Some("Invoicing has no checks")
    );
    assert_eq!(
        unknown.refusal().map(ToString::to_string).as_deref(),
        Some("nobody has no checks")
    );
    assert_eq!(
        missing.refusal().map(ToString::to_string).as_deref(),
        Some("Backfill has no check named nothing")
    );
    assert!(world.events.borrow().is_empty());
}

#[gpui::test]
async fn engine_cancel_keeps_the_finished_parts_and_the_next_run_resumes_there(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);
    world.write("block-at", "2\n");
    let blocked = world.tree.join("blocked-2");
    let (reached, reaching) = oneshot::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !blocked.exists() {
            assert!(std::time::Instant::now() < deadline, "part 2 never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reached.send(()).ok();
    });

    let first = world.run(cx, world.only(&["tests"]));
    reaching.await.unwrap();
    // One run per task: a second request while one is live is refused.
    let second = world.run(cx, world.only(&["tests"])).await;
    assert_eq!(
        second.refusal(),
        Some(&Refusal::Running {
            task: TASK_NAME.to_string()
        })
    );
    assert!(world.engine.update(cx, |engine, _| engine.cancel(TASK)));
    let report = first.await;

    assert!(report.cancelled());
    assert_eq!(
        report.results()[0].verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    let records = world.records("tests");
    assert_eq!(records.len(), 1);
    let parts: Vec<(u32, bool)> = records[0]
        .parts
        .iter()
        .map(|part| (part.n, part.verdict.is_pass()))
        .collect();
    assert_eq!(
        parts,
        [(1, true), (2, false)],
        "part 1 passed before the stop; part 2 ended cancelled and is run again"
    );
    assert!(
        !world
            .engine
            .read_with(cx, |engine, _| engine.is_running(TASK))
    );
    // Nothing is left to cancel.
    assert!(!world.engine.update(cx, |engine, _| engine.cancel(TASK)));

    world.forget("block-at");
    world.forget("runs");
    let resumed = world.run(cx, world.only(&["tests"])).await;

    assert_eq!(resumed.results()[0].verdict, Verdict::Pass);
    assert_eq!(world.lines("runs"), ["2", "3"], "part 1 was not run again");
    let records = world.records("tests");
    assert_eq!(records.len(), 2);
    let parts: Vec<u32> = records[1].parts.iter().map(|part| part.n).collect();
    assert_eq!(parts, [1, 2, 3]);
    assert_eq!(records[1].parts[0].started, records[0].parts[0].started);
}

#[gpui::test]
async fn engine_the_same_tree_twice_gives_the_same_verdict_class_and_two_records(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);

    let first = world.run(cx, world.only(&["tests"])).await;
    // Without `clean` a finished record of the same key is the answer: no part runs again.
    let again = world.run(cx, world.only(&["tests"])).await;
    assert_eq!(first.results()[0].record_id, again.results()[0].record_id);
    assert_eq!(world.lines("runs"), ["1", "2", "3"]);
    assert_eq!(world.records("tests").len(), 1);

    // A clean run starts over on the same tree: a second record, the same class of verdict.
    let clean = world
        .run(
            cx,
            RunChecksRequest {
                clean: true,
                ..world.only(&["tests"])
            },
        )
        .await;

    assert_eq!(world.lines("runs"), ["1", "2", "3", "1", "2", "3"]);
    let records = world.records("tests");
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].id, records[1].id);
    assert_eq!(records[0].tree_key, records[1].tree_key);
    assert_eq!(class(&records[0].verdict), class(&records[1].verdict));
    assert_eq!(
        class(&first.results()[0].verdict),
        class(&clean.results()[0].verdict)
    );
}

#[gpui::test]
async fn engine_waiting_for_a_pool_emits_waiting_and_shows_the_live_row(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_clock(cx, true, clock.clone());
    let pool = FakePool::new("gpu", 1, clock);
    let other = pool.hold("other run", 1);
    cx.update(|cx| CheckPoolsRegistry::set(Rc::new(pool.clone()), cx));

    let (waiting, waited) = oneshot::channel();
    let waiting = RefCell::new(Some(waiting));
    cx.update(|cx| {
        cx.subscribe(&world.engine, move |_, event: &ChecksEvent, _| {
            if matches!(event, ChecksEvent::Waiting { .. })
                && let Some(waiting) = waiting.borrow_mut().take()
            {
                waiting.send(()).ok();
            }
        })
        .detach()
    });

    let report = world.run(cx, world.only(&["gpu"]));
    waited.await.unwrap();

    assert!(world.events.borrow().contains(&ChecksEvent::Waiting {
        task_id: TASK.to_string(),
        check: "gpu".to_string(),
        pool: "gpu".to_string(),
        position: 1,
    }));
    let status = world
        .engine
        .read_with(cx, |engine, cx| engine.status(TASK, cx))
        .unwrap();
    assert!(status.running);
    assert_eq!(
        status.live.get("gpu"),
        Some(&Live::Waiting {
            pool: "gpu".to_string(),
            place: 1,
            secs: 0
        })
    );
    assert!(
        world.lines("order").is_empty(),
        "no process ran while waiting"
    );

    drop(other);
    let report = report.await;
    assert_eq!(report.results()[0].verdict, Verdict::Pass);
    assert_eq!(world.lines("order"), ["gpu"]);
    pool.assert_all_released();
}

#[gpui::test]
fn engine_the_tree_is_the_active_runs_worktree_and_falls_back_in_order(cx: &mut TestAppContext) {
    let world = World::new(cx, false);
    let folders = TempDir::new().unwrap();
    let dir = |name: &str| {
        let path = folders.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    };
    let (active, tip, ended, file) = (dir("active"), dir("tip"), dir("ended"), dir("file"));
    let own = |worktree: Option<&Path>, repo: Option<&str>| TaskChecks {
        repo: repo.map(str::to_string),
        worktree: worktree.map(Path::to_path_buf),
        ..world.task(TASK, TASK_NAME, None)
    };
    let tree = |path: &Path, run: Option<&str>| {
        Some(TaskTree {
            path: path.to_path_buf(),
            run: run.map(str::to_string),
        })
    };

    // No source has a tree.
    assert_eq!(world.task_tree(cx, &own(None, Some("app"))), None);
    // A worktree that is gone is no tree either.
    let gone = folders.path().join("gone");
    assert_eq!(world.task_tree(cx, &own(Some(&gone), Some("app"))), None);
    // The `worktree` of the task's file.
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), Some("app"))),
        tree(&file, None)
    );
    // The last ended run's worktree while it exists, before the file's.
    *world.runs.ended.borrow_mut() = Some(("run-1".to_string(), ended.clone()));
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), Some("app"))),
        tree(&ended, Some("run-1"))
    );
    *world.runs.ended.borrow_mut() = Some(("run-1".to_string(), gone));
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), Some("app"))),
        tree(&file, None)
    );
    *world.runs.ended.borrow_mut() = Some(("run-1".to_string(), ended.clone()));
    // The tree a lane serves, before an ended run's; a task that names no repository has none.
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(FakeTips(tip.clone())), cx));
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), Some("app"))),
        tree(&tip, None)
    );
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), None)),
        tree(&ended, Some("run-1"))
    );
    // The active run's worktree before everything.
    *world.runs.active.borrow_mut() = Some(("run-2".to_string(), active.clone()));
    assert_eq!(
        world.task_tree(cx, &own(Some(&file), Some("app"))),
        tree(&active, Some("run-2"))
    );
    assert_eq!(
        world.task_tree(cx, &own(None, None)),
        tree(&active, Some("run-2"))
    );
}

#[gpui::test]
fn engine_the_order_puts_an_accepting_check_after_the_check_that_names_it(cx: &mut TestAppContext) {
    let world = World::new(cx, true);
    let task = world.source.tasks.borrow()[TASK].clone();
    let order = |only: Option<&[&str]>| -> Result<Vec<String>, Refusal> {
        let only: Option<Vec<String>> =
            only.map(|names| names.iter().map(|name| name.to_string()).collect());
        plan_rows(&task, only.as_deref())
            .map(|rows| rows.into_iter().map(|row| row.decl.name).collect())
    };

    assert_eq!(order(None).unwrap(), ["lint", "tests", "docs", "gpu"]);
    assert_eq!(order(Some(&["gpu", "tests"])).unwrap(), ["tests", "gpu"]);
    assert_eq!(
        order(Some(&["tests", "nothing"])),
        Err(Refusal::UnknownCheck {
            task: TASK_NAME.to_string(),
            check: "nothing".to_string()
        })
    );
    let group = world.source.tasks.borrow()[GROUP].clone();
    assert_eq!(
        plan_rows(&group, None),
        Err(Refusal::NoChecks {
            task: "Invoicing".to_string()
        })
    );
}

#[gpui::test]
async fn engine_dump_names_the_checks_rows_verdicts_and_key_inputs_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);
    world.run(cx, world.only(&["docs"])).await;

    let text = world
        .engine
        .read_with(cx, |engine, cx| engine.dump(TASK, cx))
        .expect("the task is known");

    assert!(text.starts_with("task: Backfill (l1)\n"), "{text}");
    assert!(text.contains(&format!("tree: {}", world.tree.display())));
    let docs = text
        .lines()
        .find(|line| line.starts_with("check docs |"))
        .expect("the docs line");
    assert!(docs.contains("last verdict pass"), "{docs}");
    assert!(docs.contains("tree key "), "{docs}");
    assert!(docs.contains("key inputs "), "{docs}");
    assert!(docs.contains("fresh"), "{docs}");
    let lint = text
        .lines()
        .find(|line| line.starts_with("check lint |"))
        .expect("the lint line");
    assert!(lint.contains("last verdict none"), "{lint}");
    assert!(text.contains("\nrows:\n"), "{text}");
    assert!(text.contains("\n  docs | own | required | pass"), "{text}");
    // Nothing of the process environment is written.
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(path.is_empty() || !text.contains(&path));

    let logs = TempDir::new().unwrap();
    let written = write_dump(logs.path(), &text).unwrap();
    assert_eq!(written, logs.path().join(DUMP_FILE));
    assert_eq!(std::fs::read_to_string(written).unwrap(), text);
    assert_eq!(DUMP_FILE, "task-checks.txt");
}

#[test]
fn commands_act_on_the_task_at_point_else_pick_among_those_with_checks() {
    let some = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    assert_eq!(
        choose_target(Some("a".to_string()), some(&["b", "c"])),
        Target::Task("a".to_string())
    );
    assert_eq!(
        choose_target(None, some(&["b", "c"])),
        Target::Pick(some(&["b", "c"]))
    );
    assert_eq!(choose_target(None, Vec::new()), Target::NoTasks);
    assert_eq!(NO_TASK_MESSAGE, "No task with checks to act on");
}

#[test]
fn commands_say_how_a_run_ended_or_why_it_was_refused() {
    let report = |outcome| RunReport {
        task_id: TASK.to_string(),
        outcome,
    };
    let result = |check: &str, verdict| crate::engine::CheckResult {
        check: check.to_string(),
        verdict,
        record_id: None,
    };
    let ran = |results, cancelled| report(RunOutcome::Ran { results, cancelled });

    assert_eq!(
        report_message(
            TASK_NAME,
            &report(RunOutcome::Refused(Refusal::NoChecks {
                task: TASK_NAME.to_string()
            }))
        ),
        "Backfill has no checks"
    );
    assert_eq!(
        report_message(
            TASK_NAME,
            &ran(
                vec![result("a", Verdict::Pass), result("b", Verdict::Pass)],
                false
            )
        ),
        "Checks of Backfill: all 2 pass"
    );
    assert_eq!(
        report_message(
            TASK_NAME,
            &ran(
                vec![
                    result("a", Verdict::Fail),
                    result("b", Verdict::NoVerdict(NoVerdictReason::Interrupted)),
                    result("c", Verdict::Pass),
                ],
                false
            )
        ),
        "Checks of Backfill: failing: a; no verdict: b"
    );
    assert_eq!(
        report_message(TASK_NAME, &ran(Vec::new(), true)),
        "Checks of Backfill stopped"
    );
}

/// `suite` runs in four parts, one after another, each holding a slot of the pool `gpu`.
const DRAIN_CHECKS: &str = "
[[checks.check]]
name = \"suite\"
run = \"checks/part-log\"
parts = 4
needs = [\"gpu\"]

[[checks.check]]
name = \"docs\"
run = \"checks/check-log\"
";

/// The rows of `checks.live` in the world's store.
fn stored_live(world: &World) -> std::collections::BTreeMap<String, LiveRow> {
    world.writer().read(|read| live_rows(read)).unwrap()
}

/// Every key of `checks.live`, `part/` rows or not.
fn live_keys(world: &World) -> Vec<String> {
    world
        .writer()
        .read(|read| read.kv(LIVE_NAMESPACE).scan(""))
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// A receiver that resolves once part `part` of the tree's blocking script has started.
fn when_blocked(world: &World, part: u32) -> oneshot::Receiver<()> {
    let blocked = world.tree.join(format!("blocked-{part}"));
    let (reached, reaching) = oneshot::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !blocked.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the part never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reached.send(()).ok();
    });
    reaching
}

fn set_kill_grace(cx: &mut TestAppContext, seconds: u32) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .task_checks
                    .get_or_insert_default()
                    .kill_grace_seconds = Some(seconds);
            });
        });
    });
}

type Stopped = Vec<(bongos_workflow::RunId, bongos_workflow_model::NodeKey)>;

/// Starts `request` (of `suite`) and waits until its third part blocks (parts 1 and 2 passed).
async fn start_blocked_at_part_three(
    world: &World,
    cx: &mut TestAppContext,
    request: RunChecksRequest,
) -> Task<RunReport> {
    world.write("block-at", "3\n");
    let blocked = when_blocked(world, 3);
    let run = world.run(cx, request);
    blocked.await.unwrap();
    cx.run_until_parked();
    assert!(
        !live_keys(world).is_empty(),
        "the running suite has a live row"
    );
    run
}

/// Starts `suite`, waits until its third part blocks (parts 1 and 2 passed), drains, and returns
/// the run's report, what the drain returned and how long the drain took.
async fn drain_during_part_three(
    world: &World,
    cx: &mut TestAppContext,
) -> (RunReport, Stopped, std::time::Duration) {
    drain_request_during_part_three(world, cx, world.only(&["suite"])).await
}

async fn drain_request_during_part_three(
    world: &World,
    cx: &mut TestAppContext,
    request: RunChecksRequest,
) -> (RunReport, Stopped, std::time::Duration) {
    let run = start_blocked_at_part_three(world, cx, request).await;

    // Real time only measures the drain against the grace; no outcome depends on the calendar.
    let began = std::time::Instant::now();
    let drained = world.engine.update(cx, |engine, cx| engine.drain(cx)).await;
    let took = began.elapsed();
    (run.await, drained, took)
}

#[gpui::test]
async fn engine_live_rows_show_part_states_at_most_once_per_task_per_interval(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);
    // A row left from before the store was opened by this process: gone at the engine's first
    // use of the store.
    world
        .writer()
        .transact(|tx| {
            tx.kv(LIVE_NAMESPACE)
                .put("part/l1/stale", toml::Value::String("old".to_string()))
        })
        .unwrap();

    let report = world.run(cx, world.only(&["tests"])).await;
    cx.run_until_parked();
    assert_eq!(report.results()[0].verdict, Verdict::Pass);

    let writes = world.engine.read_with(cx, |engine, _| engine.live_writes());
    assert!(writes.len() >= 2, "{writes:?}");
    // Part 1 of 3 running was written, with when it began and never how long it ran.
    assert!(writes.iter().any(|write| matches!(
        write.rows.get("tests"),
        Some(LiveRow::Running { part: 1, of: 3, kept: 0, since }) if !since.is_empty()
    )));
    // At most one write per task per `live_write_ms` (1000) on the engine's clock.
    for pair in writes.windows(2) {
        assert!(
            pair[1].at.duration_since(pair[0].at) >= std::time::Duration::from_millis(1000),
            "{writes:?}"
        );
    }
    // The last state won: the check ended with its verdict.
    let rows = stored_live(&world);
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["part/l1/tests"]);
    assert!(matches!(
        &rows["part/l1/tests"],
        LiveRow::Ended {
            verdict: Verdict::Pass,
            ..
        }
    ));
    assert!(!live_keys(&world).contains(&"part/l1/stale".to_string()));
}

#[gpui::test]
async fn engine_drain_stops_a_four_part_check_releases_its_pool_and_clears_live_rows(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_checks(cx, true, clock.clone(), DRAIN_CHECKS);
    set_kill_grace(cx, 3);
    let pool = FakePool::new("gpu", 1, clock);
    cx.update(|cx| CheckPoolsRegistry::set(Rc::new(pool.clone()), cx));

    let (report, drained, took) = drain_during_part_three(&world, cx).await;

    // Within the grace, and no workflow node: the run was a manual one.
    assert!(took < std::time::Duration::from_secs(3), "{took:?}");
    assert!(drained.is_empty());
    assert!(report.cancelled());
    assert_eq!(
        report.results()[0].verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    // Parts 1 and 2 finished and stay in the unfinished record; part 3 was stopped.
    let records = world.records("suite");
    assert_eq!(records.len(), 1);
    let parts: Vec<(u32, bool)> = records[0]
        .parts
        .iter()
        .map(|part| (part.n, part.verdict.is_pass()))
        .collect();
    assert_eq!(parts, [(1, true), (2, true), (3, false)]);
    assert_eq!(world.lines("runs"), ["1", "2", "3"], "part 4 never started");
    // Every lease went back to the pool.
    pool.assert_all_released();
    assert_eq!(pool.state().released.len(), 3);
    // No live row survives the drain.
    cx.run_until_parked();
    assert!(live_keys(&world).is_empty(), "{:?}", live_keys(&world));
    // While drained nothing starts.
    assert!(world.engine.read_with(cx, |engine, _| engine.is_drained()));
    let refused = world.run(cx, world.only(&["docs"])).await;
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::Draining {
            task: TASK_NAME.to_string()
        })
    );
    assert!(world.lines("order").is_empty());
}

#[gpui::test]
async fn engine_after_undrain_runs_start_again_and_resume_from_the_kept_parts(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::manual(), DRAIN_CHECKS);
    set_kill_grace(cx, 3);
    let (report, _, _) = drain_during_part_three(&world, cx).await;
    assert!(report.cancelled());

    world.engine.update(cx, |engine, _| engine.undrain());

    // A new run starts normally.
    let docs = world.run(cx, world.only(&["docs"])).await;
    assert_eq!(docs.results()[0].verdict, Verdict::Pass);
    assert_eq!(world.lines("order"), ["docs"]);

    // The next manual run of the same key resumes after the two parts that finished.
    world.forget("block-at");
    world.forget("runs");
    let resumed = world.run(cx, world.only(&["suite"])).await;
    assert_eq!(resumed.results()[0].verdict, Verdict::Pass);
    assert_eq!(world.lines("runs"), ["3", "4"], "parts 1 and 2 were kept");
    let records = world.records("suite");
    assert_eq!(records.len(), 2);
    let parts: Vec<u32> = records[1].parts.iter().map(|part| part.n).collect();
    assert_eq!(parts, [1, 2, 3, 4]);
    assert_eq!(records[1].parts[0].started, records[0].parts[0].started);
}

#[gpui::test]
async fn engine_drain_returns_the_workflow_node_of_each_run_it_stopped(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::manual(), DRAIN_CHECKS);
    set_kill_grace(cx, 3);
    let node = (
        "run-1".to_string(),
        bongos_workflow_model::NodeKey::new("checks", 1),
    );
    let request = RunChecksRequest {
        node: Some(node.clone()),
        ..world.only(&["suite"])
    };

    let (report, drained, _) = drain_request_during_part_three(&world, cx, request).await;

    assert_eq!(drained, [node]);
    assert!(report.cancelled());
}

#[gpui::test]
async fn engine_drain_resolves_at_the_grace_on_its_clock_while_a_run_has_not_ended(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_checks(cx, true, clock.clone(), DRAIN_CHECKS);
    // SIGKILL comes one second before the end of the grace, in real time: the part outlives
    // SIGTERM for about two seconds, so only the engine's clock can end the drain first.
    set_kill_grace(cx, 3);
    let pool = FakePool::new("gpu", 1, clock.clone());
    cx.update(|cx| CheckPoolsRegistry::set(Rc::new(pool.clone()), cx));
    world.write("ignore-term", "");
    let run = start_blocked_at_part_three(&world, cx, world.only(&["suite"])).await;

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    clock.advance(std::time::Duration::from_secs(3));
    let drained = drain.await;

    // The grace won: the run is still active, and no live row is left.
    assert!(drained.is_empty());
    assert!(
        world
            .engine
            .read_with(cx, |engine, _| engine.is_running(TASK))
    );
    assert!(live_keys(&world).is_empty(), "{:?}", live_keys(&world));

    // The run ends after the SIGKILL, still writing no live row while drained.
    let report = run.await;
    cx.run_until_parked();
    assert!(report.cancelled());
    assert!(
        !world
            .engine
            .read_with(cx, |engine, _| engine.is_running(TASK))
    );
    pool.assert_all_released();
    assert!(live_keys(&world).is_empty(), "{:?}", live_keys(&world));
}

#[gpui::test]
async fn engine_an_undrain_before_the_drain_resolves_lets_new_runs_write_live_rows(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_checks(cx, true, clock.clone(), DRAIN_CHECKS);
    let run = start_blocked_at_part_three(&world, cx, world.only(&["suite"])).await;
    // The clock stands still, so a later part's row waits for a scheduled write.
    assert!(
        world
            .engine
            .read_with(cx, |engine, _| engine.keeps_live_task(TASK))
    );

    let drain = world.engine.update(cx, |engine, cx| {
        let drain = engine.drain(cx);
        engine.undrain();
        drain
    });
    let report = run.await;
    drain.await;
    cx.run_until_parked();
    assert!(report.cancelled());
    // No row of the run from before the drain is left: the stopped suite shows how it ended.
    let rows = stored_live(&world);
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["part/l1/suite"]);
    assert!(
        matches!(&rows["part/l1/suite"], LiveRow::Ended { verdict, .. } if !verdict.is_pass()),
        "{rows:?}"
    );

    // A new run writes its rows again, the throttle's last write included.
    let docs = world.run(cx, world.only(&["docs"])).await;
    assert_eq!(docs.results()[0].verdict, Verdict::Pass);
    clock.advance(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let rows = stored_live(&world);
    assert!(
        matches!(
            &rows.get("part/l1/docs"),
            Some(LiveRow::Ended {
                verdict: Verdict::Pass,
                ..
            })
        ),
        "{rows:?}"
    );
    // The task's live state is let go once its run ended and its rows were written.
    assert!(
        !world
            .engine
            .read_with(cx, |engine, _| engine.keeps_live_task(TASK))
    );
}
