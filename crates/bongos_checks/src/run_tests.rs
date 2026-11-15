use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bongos_checks_model::decl::{self, CheckDecl};
use bongos_checks_model::evidence::{EvidenceRecord, Scope, StartedBy, TreeKind};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_project_repo::{Git, ProjectRepo, StateWriter};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::evidence_store::{record_key, scan_records, store_record, task_prefix};
use crate::pin::Pin;
use crate::process::StopHandle;
use crate::run::{CheckRun, NO_BASE_BRANCH, SUPERSEDED, items_digest};
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
            tree_kind: TreeKind::Tip,
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
        let stop = run.stop.clone();
        let running = std::thread::spawn(move || block_on(run.run()).unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.exists("blocked-5") {
            assert!(Instant::now() < deadline, "part 5 never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.stop();
        let record = running.join().unwrap();
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
