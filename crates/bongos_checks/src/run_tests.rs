use std::cell::RefCell;
use std::future::Future as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bongos_checks_model::decl::{self, CheckDecl, NeedEntry};
use bongos_checks_model::evidence::{EvidenceRecord, Scope, StartedBy, TreeKind};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_project_repo::{Git, ProjectRepo, StateWriter};
use futures::executor::{LocalPool, block_on};
use futures::task::LocalSpawnExt as _;
use tempfile::TempDir;

use crate::evidence_store::{record_key, scan_records, store_record, task_prefix};
use crate::pin::Pin;
use crate::pools::{CheckClock as _, WaitStatus};
use crate::pools_tests::{FakeClock, FakePool};
use crate::process::StopHandle;
use crate::run::{CheckRun, LiveState, NO_BASE_BRANCH, SUPERSEDED, items_digest};
use crate::settings::TaskChecksSettings;
use crate::test_support::{commit_everything, fixture_project, init_git_repo, write_file};
use crate::trees::EnvBase;

const SCRIPTS: [&str; 3] = ["scope-answer", "plan-count", "part-log"];
const TASK: &str = "l1";

/// Fields drop in order: the pin goes before the folder holding it.
struct Fixture {
    pin: Arc<Pin>,
    repo: ProjectRepo,
    writer: Arc<StateWriter>,
    tree: PathBuf,
    output: PathBuf,
    clock: Arc<AtomicU64>,
    _root: TempDir,
}

/// A project whose master holds the fixture scripts under `definitions/checks/`, pinned at
/// `main`, and a tree that is a git repository on `main` with one committed file and one new one.
fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let repository = repo.location().repository.clone();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-fixtures");
    let checks = repository.join("definitions/checks");
    std::fs::create_dir_all(&checks).unwrap();
    for script in SCRIPTS {
        let target = checks.join(script);
        std::fs::copy(fixtures.join(script), &target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    commit_everything(&repository, "add checks");
    let pin = Arc::new(block_on(Pin::materialize(&repo, "main")).unwrap());
    let tree = root.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    init_git_repo(&tree);
    write_file(&tree, "src/old.rs", "old\n");
    commit_everything(&tree, "start");
    write_file(&tree, "src/new.rs", "new\n");
    let writer = repo.state().unwrap().writer().unwrap().clone();
    Fixture {
        pin,
        writer,
        tree,
        output: root.path().join("output"),
        clock: Arc::default(),
        repo,
        _root: root,
    }
}

fn settings() -> TaskChecksSettings {
    TaskChecksSettings {
        enabled: true,
        output_dir: None,
        output_retention_days: 30,
        output_limit_mb: 1,
        result_file_limit_kb: 64,
        default_budget_minutes: None,
        no_verdict_reruns: 2,
        no_verdict_rerun_minutes: 5,
        pool_wait_limit_hours: 4,
        kill_grace_seconds: 2,
        parallel_parts_max: 4,
        scratch_dir: None,
        tree_key_debounce_ms: 2000,
        live_write_ms: 1000,
    }
}

/// The check `tests`: `part-log` per part, `scope-answer` for its scope and `plan-count` for its
/// part count.
fn planned_check() -> CheckDecl {
    CheckDecl {
        name: "tests".to_string(),
        run: Some("checks/part-log".to_string()),
        scope: Some(decl::Scope::Script {
            script: "checks/scope-answer".to_string(),
        }),
        parts: Some(decl::Parts::Plan {
            plan: "checks/plan-count".to_string(),
        }),
        ..CheckDecl::default()
    }
}

fn items_answer(items: &[&str]) -> String {
    serde_json::json!({ "scope": "items", "items": items, "reason": "changed files map to tests" })
        .to_string()
}

impl Fixture {
    fn check_run(&self, decl: CheckDecl) -> CheckRun {
        let clock = self.clock.clone();
        CheckRun {
            task_id: TASK.to_string(),
            decl,
            digest: "digest-1".to_string(),
            pin: self.pin.clone(),
            master: crate::pin::master_definitions(&self.repo),
            tree: self.tree.clone(),
            other_trees: Vec::new(),
            tree_kind: TreeKind::Tip,
            base: None,
            tree_key: "tree-1".to_string(),
            key_inputs: "inputs-1".to_string(),
            git: Git::locate().expect("git is installed").isolated(),
            base_branch: Some("main".to_string()),
            env: EnvBase::Own,
            output_dir: self.output.clone(),
            evidence: None,
            params: None,
            started_by: StartedBy::Person,
            clean: false,
            settings: settings(),
            writer: self.writer.clone(),
            stop: StopHandle::default(),
            // A counter, never the wall clock.
            now: Arc::new(move || {
                let n = clock.fetch_add(1, Ordering::SeqCst);
                format!(
                    "2026-03-01T{:02}:{:02}:{:02}Z",
                    n / 3600,
                    n / 60 % 60,
                    n % 60
                )
            }),
            on_changed: None,
            project_id: "project-1".to_string(),
            node: None,
            pools: None,
            clock: FakeClock::automatic(),
            on_live: None,
            fail_part_end: None,
        }
    }

    fn run(&self, run: CheckRun) -> EvidenceRecord {
        block_on(run.run()).unwrap()
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.tree.join(name), text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.tree.join(name)).unwrap()
    }

    fn exists(&self, name: &str) -> bool {
        self.tree.join(name).exists()
    }

    /// The part numbers run since `runs` was last removed, in order.
    fn runs(&self) -> Vec<u32> {
        if !self.exists("runs") {
            return Vec::new();
        }
        self.read("runs")
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    fn forget_runs(&self) {
        std::fs::remove_file(self.tree.join("runs")).ok();
    }

    /// Stops `run` once its part `n` blocks (`block-at` holds `n`), from a thread of its own.
    fn stop_when_blocked(&self, run: &CheckRun, n: u32) -> std::thread::JoinHandle<()> {
        let stop = run.stop.clone();
        let blocked = self.tree.join(format!("blocked-{n}"));
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !blocked.exists() {
                assert!(Instant::now() < deadline, "part {n} never started");
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.stop();
        })
    }

    fn records(&self) -> Vec<EvidenceRecord> {
        self.writer
            .read(|read| scan_records(read, &task_prefix(TASK)))
            .unwrap()
    }

    /// Runs `planned_check` with eight parts and stops it while part 5 runs, so parts 1 to 4
    /// passed. Returns the stopped run's record; `runs` is cleared afterwards.
    fn interrupted(&self) -> EvidenceRecord {
        self.write("scope.json", &items_answer(&["a", "b", "c"]));
        self.write("plan-count", "8\n");
        self.write("block-at", "5\n");
        let run = self.check_run(planned_check());
        let watcher = self.stop_when_blocked(&run, 5);
        let record = block_on(run.run()).unwrap();
        watcher.join().unwrap();
        assert_eq!(self.runs(), [1, 2, 3, 4, 5]);
        assert_eq!(
            record.verdict,
            Verdict::NoVerdict(NoVerdictReason::Cancelled)
        );
        let verdicts: Vec<(u32, bool)> = record
            .parts
            .iter()
            .map(|part| (part.n, part.verdict.is_pass()))
            .collect();
        assert_eq!(
            verdicts,
            [(1, true), (2, true), (3, true), (4, true), (5, false)]
        );
        std::fs::remove_file(self.tree.join("block-at")).unwrap();
        self.forget_runs();
        record
    }
}

fn assert_whole_pass(record: &EvidenceRecord, parts: u32) {
    assert!(record.is_finished());
    assert_eq!(record.verdict, Verdict::Pass, "{record:?}");
    assert_eq!(record.variant.parts, parts);
    let numbers: Vec<u32> = record.parts.iter().map(|part| part.n).collect();
    assert_eq!(numbers, (1..=parts).collect::<Vec<_>>());
    assert!(record.parts.iter().all(|part| part.verdict.is_pass()));
}

#[test]
fn check_run_a_scope_of_six_items_reaches_the_command_and_the_record() {
    let fixture = fixture();
    let items = ["a", "b", "c", "d", "e", "f"];
    fixture.write("scope.json", &items_answer(&items));
    let decl = CheckDecl {
        parts: None,
        ..planned_check()
    };
    let record = fixture.run(fixture.check_run(decl));

    assert_whole_pass(&record, 1);
    let owned: Vec<String> = items.iter().map(|item| item.to_string()).collect();
    assert_eq!(
        record.variant.scope,
        Scope::Items {
            digest: items_digest(&owned),
            count: 6
        }
    );
    assert_eq!(
        record.scope_reason.as_deref(),
        Some("changed files map to tests")
    );
    assert_eq!(fixture.read("items-1").lines().count(), 6);
    assert_eq!(fixture.read("items-1"), "a\nb\nc\nd\ne\nf\n");
    assert_eq!(fixture.read("scope-1"), "items 1\n");
    // The scope script got the changed files and its own variables, not a part's.
    assert!(
        fixture
            .read("changed-seen")
            .lines()
            .any(|line| line == "src/new.rs"),
        "{}",
        fixture.read("changed-seen")
    );
    let names = fixture.read("scope-env-names");
    let names: Vec<&str> = names.lines().collect();
    for present in [
        "BONGOS_CHANGED_FILES",
        "BONGOS_SCOPE_RESULT",
        "BONGOS_CHECK_PARAMS",
        "BONGOS_CHECK_NAME",
        "BONGOS_CHECK_PINNED",
    ] {
        assert!(names.contains(&present), "{present} missing: {names:?}");
    }
    for absent in [
        "BONGOS_CHECK_PART",
        "BONGOS_CHECK_RESULT",
        "BONGOS_CHECK_ITEMS",
    ] {
        assert!(!names.contains(&absent), "{absent} present");
    }
    // The record is stored as returned, and its output folder holds the part and the items.
    let stored = fixture.records();
    assert_eq!(stored, std::slice::from_ref(&record));
    let folder = fixture.output.join(&record.id);
    assert!(folder.join("part-1.log").exists());
    assert!(folder.join("items.txt").exists());
}

#[test]
fn check_run_scope_all_and_none_carry_their_reasons() {
    let fixture = fixture();
    let one_part = CheckDecl {
        parts: None,
        ..planned_check()
    };

    fixture.write(
        "scope.json",
        r#"{ "scope": "all", "reason": "the build file changed" }"#,
    );
    let all = fixture.run(fixture.check_run(one_part.clone()));
    assert_whole_pass(&all, 1);
    assert_eq!(all.variant.scope, Scope::All);
    assert_eq!(all.scope_reason.as_deref(), Some("the build file changed"));
    assert_eq!(fixture.read("scope-1"), "all 1\n");
    assert!(!fixture.exists("items-1"));
    fixture.forget_runs();

    fixture.write(
        "scope.json",
        r#"{ "scope": "none", "reason": "only documentation changed" }"#,
    );
    let notified = Arc::new(AtomicU64::new(0));
    let seen = notified.clone();
    let mut run = fixture.check_run(one_part.clone());
    run.on_changed = Some(Arc::new(move |task: &str| {
        assert_eq!(task, TASK);
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    let none = fixture.run(run);
    assert!(none.is_finished());
    assert_eq!(none.verdict, Verdict::Pass);
    assert_eq!(
        none.variant.scope,
        Scope::None {
            reason: "only documentation changed".to_string()
        }
    );
    assert_eq!(none.reason.as_deref(), Some("only documentation changed"));
    assert!(none.parts.is_empty());
    assert!(fixture.runs().is_empty(), "a part ran");
    assert_eq!(notified.load(Ordering::SeqCst), 1);

    // Without a base branch the script is not asked: the scope is all, "no base branch".
    std::fs::remove_file(fixture.tree.join("changed-seen")).unwrap();
    let mut run = fixture.check_run(one_part);
    run.base_branch = None;
    // A clean run: the earlier `all` record has the same key and would be kept as it is.
    run.clean = true;
    let unbased = fixture.run(run);
    assert_whole_pass(&unbased, 1);
    assert_eq!(unbased.variant.scope, Scope::All);
    assert_eq!(unbased.scope_reason.as_deref(), Some(NO_BASE_BRANCH));
    assert!(!fixture.exists("changed-seen"));
}

#[test]
fn check_run_eight_parts_stopped_after_four_resume_at_part_five() {
    let fixture = fixture();
    let stopped = fixture.interrupted();

    let resumed = fixture.run(fixture.check_run(planned_check()));
    assert_eq!(fixture.runs(), [5, 6, 7, 8]);
    assert_whole_pass(&resumed, 8);
    assert_ne!(resumed.id, stopped.id);
    // The kept parts are the stopped run's own.
    assert_eq!(resumed.parts[..4], stopped.parts[..4]);
    assert_eq!(fixture.read("scope-6"), "items 8\n");
    assert_eq!(fixture.read("items-8"), "a\nb\nc\n");
    // The plan script saw the stopped run's part durations.
    let durations: serde_json::Value =
        serde_json::from_str(&fixture.read("durations-seen")).unwrap();
    assert_eq!(durations["check"], "tests");
    assert_eq!(durations["records"][0]["id"], stopped.id.as_str());
    assert_eq!(
        durations["records"][0]["durations_ms"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    // Run again on the same key: every part is kept, nothing runs.
    fixture.forget_runs();
    let again = fixture.run(fixture.check_run(planned_check()));
    assert!(fixture.runs().is_empty());
    assert_eq!(again, resumed);
}

#[test]
fn check_run_a_crashed_run_resumes_in_its_own_record_and_a_clean_run_supersedes_it() {
    for clean in [false, true] {
        let fixture = fixture();
        let mut crashed = fixture.interrupted();
        // As if bongos died while part 5 ran: the record never ended and part 5 never finished.
        crashed.ended = None;
        crashed.parts[4].ended = None;
        fixture
            .writer
            .transact(|tx| store_record(tx, &crashed))
            .unwrap();

        let mut run = fixture.check_run(planned_check());
        run.clean = clean;
        let record = fixture.run(run);
        assert_whole_pass(&record, 8);
        let records = fixture.records();
        if clean {
            assert_eq!(fixture.runs(), (1..=8).collect::<Vec<_>>());
            assert_ne!(record.id, crashed.id);
            let closed = records
                .iter()
                .find(|stored| stored.id == crashed.id)
                .unwrap();
            assert!(closed.is_finished());
            assert_eq!(closed.reason.as_deref(), Some(SUPERSEDED));
            assert!(closed.parts.iter().all(|part| part.ended.is_some()));
        } else {
            assert_eq!(fixture.runs(), [5, 6, 7, 8]);
            assert_eq!(record.id, crashed.id);
            assert_eq!(records.len(), 1);
            assert_eq!(record_key(&records[0]), record_key(&crashed));
        }
    }
}

#[test]
fn check_run_any_change_of_key_runs_every_part_again() {
    let changes: [(&str, fn(&Fixture, &mut CheckRun), u32); 6] = [
        ("an edit", |_, run| run.tree_key = "tree-2".to_string(), 8),
        (
            "a key input",
            |_, run| run.key_inputs = "inputs-2".to_string(),
            8,
        ),
        (
            "a replan",
            |fixture, _| fixture.write("plan-count", "9\n"),
            9,
        ),
        (
            "a changed scope",
            |fixture, _| fixture.write("scope.json", &items_answer(&["a", "b", "d"])),
            8,
        ),
        (
            "a changed check",
            |_, run| run.digest = "digest-2".to_string(),
            8,
        ),
        ("a clean run", |_, run| run.clean = true, 8),
    ];
    for (what, change, parts) in changes {
        let fixture = fixture();
        let stopped = fixture.interrupted();
        let mut run = fixture.check_run(planned_check());
        change(&fixture, &mut run);
        let record = fixture.run(run);
        assert_eq!(
            fixture.runs(),
            (1..=parts).collect::<Vec<_>>(),
            "after {what}"
        );
        assert_whole_pass(&record, parts);
        assert_ne!(record.id, stopped.id, "after {what}");
        assert!(
            record.parts[..4]
                .iter()
                .zip(&stopped.parts)
                .all(|(new, old)| new.token != old.token),
            "after {what} a part was kept"
        );
    }
}

#[test]
fn check_run_all_parts_run_after_a_failure_unless_it_stops_at_the_first() {
    let fixture = fixture();
    fixture.write("scope.json", &items_answer(&["a"]));
    fixture.write("fail-at", "2\n");
    let four = CheckDecl {
        parts: Some(decl::Parts::Count(4)),
        ..planned_check()
    };

    let record = fixture.run(fixture.check_run(four.clone()));
    assert_eq!(fixture.runs(), [1, 2, 3, 4]);
    assert_eq!(record.verdict, Verdict::Fail);
    let verdicts: Vec<Verdict> = record
        .parts
        .iter()
        .map(|part| part.verdict.clone())
        .collect();
    assert_eq!(
        verdicts,
        [Verdict::Pass, Verdict::Fail, Verdict::Pass, Verdict::Pass]
    );
    assert_eq!(record.summary.as_deref(), Some("3 of 4 parts passed"));
    fixture.forget_runs();

    let stops = CheckDecl {
        stop_at_first_failure: Some(true),
        ..four
    };
    let mut run = fixture.check_run(stops);
    // Another digest, so the first record is not a checkpoint of this one.
    run.digest = "digest-2".to_string();
    let record = fixture.run(run);
    assert_eq!(fixture.runs(), [1, 2]);
    assert_eq!(record.verdict, Verdict::Fail);
    let numbers: Vec<u32> = record.parts.iter().map(|part| part.n).collect();
    assert_eq!(numbers, [1, 2]);
    assert_eq!(record.variant.parts, 4);
}

const POOL: &str = "test-slots";

/// The check `tests` in `parts` parts with no scope, needing one slot of [`POOL`].
fn pooled_check(parts: i64, parallel: bool) -> CheckDecl {
    CheckDecl {
        name: "tests".to_string(),
        run: Some("checks/part-log".to_string()),
        needs: vec![NeedEntry::Pool(POOL.to_string())],
        parts: Some(decl::Parts::Count(parts)),
        parallel: Some(parallel),
        ..CheckDecl::default()
    }
}

/// Collects what `run` reports for the live rows.
fn watch_live(run: &mut CheckRun) -> Rc<RefCell<Vec<LiveState>>> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    run.on_live = Some(Rc::new(move |state: &LiveState| {
        sink.borrow_mut().push(state.clone())
    }));
    seen
}

/// A run spawned on a local pool, so a test can look at it between steps.
struct Spawned {
    pool: LocalPool,
    result: Rc<RefCell<Option<EvidenceRecord>>>,
}

impl Spawned {
    fn new(run: CheckRun) -> Self {
        let pool = LocalPool::new();
        let result = Rc::new(RefCell::new(None));
        let slot = result.clone();
        pool.spawner()
            .spawn_local(async move {
                *slot.borrow_mut() = Some(run.run().await.unwrap());
            })
            .unwrap();
        let mut spawned = Self { pool, result };
        spawned.pool.run_until_stalled();
        spawned
    }

    fn step(&mut self) {
        self.pool.run_until_stalled();
    }

    fn is_done(&self) -> bool {
        self.result.borrow().is_some()
    }

    /// Runs to the end, waiting for part processes, and returns the record.
    fn finish(mut self) -> EvidenceRecord {
        self.pool.run();
        self.result.borrow_mut().take().expect("the run ended")
    }
}

/// The pids of the parts of `record` that started, in part order.
fn part_pids(record: &EvidenceRecord) -> Vec<u32> {
    record.parts.iter().filter_map(|part| part.pid).collect()
}

fn sorted(mut numbers: Vec<u32>) -> Vec<u32> {
    numbers.sort_unstable();
    numbers
}

#[test]
fn check_run_waiting_on_a_one_slot_pool_shows_pool_place_and_time_and_spends_nothing() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 1, clock.clone());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(1, false));
    run.pools = Some(Rc::new(pool.clone()));
    run.clock = clock.clone();
    let live = watch_live(&mut run);
    let waited_from = clock.now();

    let mut spawned = Spawned::new(run);
    assert!(!spawned.is_done());
    let status = WaitStatus {
        pool: POOL.to_string(),
        position: 1,
        since: waited_from,
        holders: vec!["other run".to_string()],
        blocked: Some("all 1 slots held".to_string()),
    };
    assert_eq!(
        *live.borrow(),
        [LiveState::Waiting {
            part: 1,
            parts: 1,
            status: status
        }]
    );
    // Waiting starts no process and records no part.
    assert!(!fixture.exists("runs"));
    let records = fixture.records();
    assert_eq!(records.len(), 1);
    assert!(!records[0].is_finished());
    assert!(records[0].parts.is_empty());

    // Three hours later it still waits, in the same place, since the same time.
    clock.advance(Duration::from_secs(3 * 3600));
    spawned.step();
    assert!(!spawned.is_done());
    assert!(!fixture.exists("runs"));
    assert_eq!(pool.queued(), 1);
    // The live state is the one written when the wait began; nothing wrote another.
    assert_eq!(live.borrow().len(), 1);

    // The slot comes free: the part runs and its process holds the slot.
    drop(other);
    let record = spawned.finish();
    assert_whole_pass(&record, 1);
    assert_eq!(fixture.runs(), [1]);
    assert_eq!(
        *live.borrow().last().unwrap(),
        LiveState::Running { part: 1, parts: 1 }
    );
    let state = pool.state();
    assert_eq!(state.granted, ["other run", "l1/tests part 1"]);
    let bound: Vec<u32> = state.binds.iter().map(|(_, pid, _)| *pid).collect();
    assert_eq!(bound, part_pids(&record));
    drop(state);
    pool.assert_all_released();
}

#[test]
fn check_run_past_the_pool_wait_limit_is_no_verdict() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 1, clock.clone());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(2, false));
    run.pools = Some(Rc::new(pool.clone()));
    run.clock = clock.clone();
    run.settings.no_verdict_reruns = 0;
    let limit = Duration::from_secs(u64::from(run.settings.pool_wait_limit_hours) * 3600);

    let mut spawned = Spawned::new(run);
    clock.advance(limit - Duration::from_secs(1));
    spawned.step();
    assert!(!spawned.is_done());
    clock.advance(Duration::from_secs(1));
    spawned.step();
    assert!(spawned.is_done());
    let record = spawned.finish();

    let reason = NoVerdictReason::PoolWaitLimit {
        pool: POOL.to_string(),
    };
    assert!(record.is_finished());
    assert_eq!(record.verdict, Verdict::NoVerdict(reason.clone()));
    assert_eq!(
        record.reason.as_deref(),
        Some("resource never became available (pool test-slots)")
    );
    // Part 1 ended without running; part 2 never waited.
    assert_eq!(record.parts.len(), 1);
    assert_eq!(record.parts[0].verdict, Verdict::NoVerdict(reason));
    assert_eq!(record.parts[0].pid, None);
    assert!(!fixture.exists("runs"));
    // The wait left the line: the freed slot goes to nobody.
    assert_eq!(pool.queued(), 0);
    drop(other);
    assert!(pool.holders().is_empty());
    pool.assert_all_released();
}

#[test]
fn check_run_parallel_parts_take_only_slots_free_now() {
    let fixture = fixture();
    let pool = FakePool::new(POOL, 3, FakeClock::automatic());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(4, true));
    run.pools = Some(Rc::new(pool.clone()));
    let record = fixture.run(run);

    assert_whole_pass(&record, 4);
    assert_eq!(sorted(fixture.runs()), [1, 2, 3, 4]);
    {
        let state = pool.state();
        // Part 1 waited in line; part 2 took the last free slot at once, part 3 found none.
        assert_eq!(state.max_used, 3);
        assert!(state.granted_now >= 1, "{:?}", state.log);
        assert!(state.refused_now >= 1, "{:?}", state.log);
        assert_eq!(
            state.log[1..4],
            [
                "grant l1/tests part 1",
                "grant l1/tests part 2",
                "refused now l1/tests part 3"
            ]
        );
        let mut bound: Vec<u32> = state.binds.iter().map(|(_, pid, _)| *pid).collect();
        bound.sort_unstable();
        assert_eq!(bound, sorted(part_pids(&record)));
    }
    drop(other);
    pool.assert_all_released();
}

#[test]
fn check_run_a_failed_part_write_stops_and_drains_the_other_parts() {
    let fixture = fixture();
    fixture.write("block-at", "2\n3\n");
    let pool = FakePool::new(POOL, 3, FakeClock::automatic());
    let mut run = fixture.check_run(pooled_check(3, true));
    run.pools = Some(Rc::new(pool.clone()));
    run.fail_part_end = Some(1);
    let stop = run.stop.clone();

    let result = block_on(run.run());
    assert!(result.is_err(), "{result:?}");
    assert!(stop.is_stopped());
    // The other parts' processes were stopped and ended, and each slot was released once.
    let records = fixture.records();
    assert_eq!(records.len(), 1);
    let ended: Vec<(u32, Verdict)> = records[0]
        .parts
        .iter()
        .filter(|part| part.n != 1)
        .map(|part| (part.n, part.verdict.clone()))
        .collect();
    assert_eq!(
        ended,
        [
            (2, Verdict::NoVerdict(NoVerdictReason::Cancelled)),
            (3, Verdict::NoVerdict(NoVerdictReason::Cancelled))
        ]
    );
    pool.assert_all_released();
}

#[test]
fn check_run_a_grant_seen_with_a_stop_starts_no_process() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 1, clock.clone());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(1, false));
    run.pools = Some(Rc::new(pool.clone()));
    run.clock = clock;
    let stop = run.stop.clone();

    let mut spawned = Spawned::new(run);
    assert!(!spawned.is_done());
    // The slot comes free and the stop comes in the same poll: the grant wins the race.
    stop.stop();
    drop(other);
    spawned.step();
    let record = spawned.finish();
    assert_eq!(
        record.verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert!(!fixture.exists("runs"));
    pool.assert_all_released();
}

#[test]
fn check_run_parallel_parts_never_jump_ahead_of_a_waiter() {
    let fixture = fixture();
    let pool = FakePool::new(POOL, 4, FakeClock::automatic());
    // Once part 1 has its slot, another run asks for all four and waits in line.
    pool.state().queue_after_first_grant = Some(4);
    let mut run = fixture.check_run(pooled_check(4, true));
    run.pools = Some(Rc::new(pool.clone()));
    let record = fixture.run(run);

    assert_whole_pass(&record, 4);
    let state = pool.state();
    let at = |entry: &str| {
        state
            .log
            .iter()
            .position(|logged| logged == entry)
            .unwrap_or_else(|| panic!("{entry} missing from {:?}", state.log))
    };
    // Three slots were free, but the waiter came first: part 2 starts only after it was served.
    assert_eq!(at("grant l1/tests part 1"), 0);
    assert_eq!(at("refused now l1/tests part 2"), 1);
    assert!(at("served other run") < at("grant l1/tests part 2"));
    drop(state);
    pool.assert_all_released();
}

#[test]
fn check_run_with_no_pool_implementation_runs_at_once() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let mut run = fixture.check_run(pooled_check(3, true));
    run.clock = clock.clone();
    let live = watch_live(&mut run);
    let record = fixture.run(run);

    assert_whole_pass(&record, 3);
    assert_eq!(sorted(fixture.runs()), [1, 2, 3]);
    assert!(
        !live
            .borrow()
            .iter()
            .any(|state| matches!(state, LiveState::Waiting { .. })),
        "{:?}",
        live.borrow()
    );
    assert!(clock.sleeps().is_empty(), "nothing waited");
}

#[test]
fn check_run_a_no_verdict_check_reruns_twice_keeping_its_parts_then_reports() {
    let fixture = fixture();
    fixture.write("no-verdict-at", "2\n");
    let clock = FakeClock::automatic();
    let notified = Arc::new(AtomicU64::new(0));
    let seen = notified.clone();
    let mut run = fixture.check_run(CheckDecl {
        needs: Vec::new(),
        ..pooled_check(3, false)
    });
    run.clock = clock.clone();
    run.on_changed = Some(Arc::new(move |_: &str| {
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    let live = watch_live(&mut run);
    let record = fixture.run(run);

    let reason = NoVerdictReason::UndeclaredExit { code: 7 };
    assert_eq!(record.verdict, Verdict::NoVerdict(reason.clone()));
    // Parts 1 and 3 passed once and were kept; part 2 ran three times.
    assert_eq!(fixture.runs(), [1, 2, 3, 2, 2]);
    let pause = Duration::from_secs(5 * 60);
    assert_eq!(clock.sleeps(), [pause, pause]);
    let reruns: Vec<(u32, u32)> = live
        .borrow()
        .iter()
        .filter_map(|state| match state {
            LiveState::Rerun {
                reason: why,
                rerun,
                reruns,
                ..
            } => {
                assert_eq!(*why, reason);
                Some((*rerun, *reruns))
            }
            _ => None,
        })
        .collect();
    assert_eq!(reruns, [(1, 2), (2, 2)]);
    assert_eq!(notified.load(Ordering::SeqCst), 3);
    let mut records = fixture.records();
    records.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(records.len(), 3);
    assert_eq!(records[2], record);
    for kept in [0, 2] {
        assert_eq!(record.parts[kept].token, records[0].parts[kept].token);
    }
    assert_ne!(record.parts[1].token, records[1].parts[1].token);
}

#[test]
fn check_run_dropped_while_waiting_leaves_the_line() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 1, clock.clone());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(1, false));
    run.pools = Some(Rc::new(pool.clone()));
    run.clock = clock;

    let mut future = Box::pin(run.run());
    let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(pool.queued(), 1);
    drop(future);
    assert_eq!(pool.queued(), 0);
    drop(other);
    assert!(pool.holders().is_empty());
    pool.assert_all_released();
    assert!(!fixture.exists("runs"));
}

#[test]
fn check_run_stopped_while_waiting_ends_cancelled_without_a_slot() {
    let fixture = fixture();
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 1, clock.clone());
    let other = pool.hold("other run", 1);
    let mut run = fixture.check_run(pooled_check(2, false));
    run.pools = Some(Rc::new(pool.clone()));
    run.clock = clock.clone();
    let stop = run.stop.clone();

    let mut spawned = Spawned::new(run);
    assert!(!spawned.is_done());
    stop.stop();
    spawned.step();
    assert!(spawned.is_done());
    let record = spawned.finish();
    assert_eq!(
        record.verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert!(record.parts.is_empty());
    // A cancel is never rerun.
    assert_eq!(
        clock.sleeps().len(),
        1,
        "only the wait limit: {:?}",
        clock.sleeps()
    );
    assert_eq!(pool.queued(), 0);
    drop(other);
    pool.assert_all_released();
}

#[test]
fn check_run_stopped_while_a_part_runs_releases_its_slot() {
    let fixture = fixture();
    fixture.write("block-at", "2\n");
    let pool = FakePool::new(POOL, 2, FakeClock::automatic());
    let mut run = fixture.check_run(pooled_check(3, false));
    run.pools = Some(Rc::new(pool.clone()));
    let watcher = fixture.stop_when_blocked(&run, 2);
    let record = fixture.run(run);
    watcher.join().unwrap();

    assert_eq!(
        record.verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert_eq!(fixture.runs(), [1, 2]);
    let state = pool.state();
    assert_eq!(state.granted, ["l1/tests part 1", "l1/tests part 2"]);
    let bound: Vec<u32> = state.binds.iter().map(|(_, pid, _)| *pid).collect();
    assert_eq!(bound, part_pids(&record));
    drop(state);
    pool.assert_all_released();
}

#[test]
fn check_run_releases_its_slot_when_a_part_cannot_start_or_the_pool_refuses() {
    let fixture = fixture();
    let pool = FakePool::new(POOL, 1, FakeClock::automatic());
    let mut run = fixture.check_run(CheckDecl {
        run: None,
        command: Some(vec!["no-such-check-tool-for-pools".to_string()]),
        ..pooled_check(1, false)
    });
    run.pools = Some(Rc::new(pool.clone()));
    run.settings.no_verdict_reruns = 0;
    let record = fixture.run(run);
    assert!(
        matches!(
            record.verdict,
            Verdict::NoVerdict(NoVerdictReason::ToolNotFound(_))
        ),
        "{record:?}"
    );
    assert_eq!(pool.state().granted, ["l1/tests part 1"]);
    assert!(pool.state().binds.is_empty(), "no process started");
    pool.assert_all_released();

    // A pool the implementation does not know ends the part with its reason; nothing is held.
    let mut run = fixture.check_run(CheckDecl {
        needs: vec![NeedEntry::Pool("no-such-pool".to_string())],
        ..pooled_check(1, false)
    });
    run.digest = "digest-2".to_string();
    run.pools = Some(Rc::new(pool.clone()));
    run.settings.no_verdict_reruns = 0;
    let record = fixture.run(run);
    assert_eq!(
        record.verdict,
        Verdict::NoVerdict(NoVerdictReason::StartFailed(
            "pool request refused: unknown pool no-such-pool".to_string()
        ))
    );
    assert!(!fixture.exists("runs"));
    assert_eq!(pool.state().granted.len(), 1);
    pool.assert_all_released();
}

#[test]
fn check_runs_racing_for_two_slots_never_hold_more() {
    let fixture = fixture();
    // A manual clock: the runs wait for each other, never for the wait limit.
    let clock = FakeClock::manual();
    let pool = FakePool::new(POOL, 2, clock.clone());
    let runs: Vec<CheckRun> = ["tests-a", "tests-b", "tests-c"]
        .into_iter()
        .map(|name| {
            let mut run = fixture.check_run(CheckDecl {
                name: name.to_string(),
                ..pooled_check(3, true)
            });
            run.pools = Some(Rc::new(pool.clone()));
            run.clock = clock.clone();
            run
        })
        .collect();
    let records = block_on(futures::future::join_all(
        runs.into_iter().map(CheckRun::run),
    ));

    for record in records {
        assert_whole_pass(&record.unwrap(), 3);
    }
    assert_eq!(fixture.runs().len(), 9);
    let state = pool.state();
    assert_eq!(state.max_used, 2);
    assert_eq!(state.granted.len(), 9);
    assert_eq!(state.binds.len(), 9);
    drop(state);
    pool.assert_all_released();
}
