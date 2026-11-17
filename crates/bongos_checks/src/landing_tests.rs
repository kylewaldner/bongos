//! Tests of task checks at landing: the `check` kind run by the engine on fixture scripts in a
//! real project repository, with master's copy of each script, and the approval hook over
//! loosened checks of started tasks. A landing runs on a thread of its own while the test drives
//! the app's executor, as the app's landing runs on a background thread. No test depends on the
//! calendar.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{
    ApprovalHook as _, ApprovalInput, ApprovalOpinion, CheckOutcome, LandMode, LandRequest,
    LandingResult, LiveFacts, LiveSource, ProjectRepo,
};
use bongos_project_repo_model::{ChangeMeta, ChangeSummary, DefinitionTree};
use futures::channel::oneshot;
use futures::executor::block_on;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::engine::ChecksEngine;
use crate::engine_tests::finish;
use crate::evidence_store::{LANDING_PREFIX, landing_prefix, scan_records};
use crate::landing::{
    CHECKS_OFF, ChecksLoosening, LandingJob, MOVING_TO_HOST, parse_landing_check,
};
use crate::pools_tests::FakeClock;
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource};
use crate::task_tree::RunWorktrees;
use crate::test_support::{
    PROJECT_TOML, definition_toml, fixture_project, keyboard, land, main_commit, new_copy, person,
    start, write_definition,
};

/// The landing check every world declares: `checks/validate` from master, with a result file.
const LANDING_CHECKS: &str = "
[[landing.checks]]
name = \"validate\"
phase = \"before-queue\"
applies = \"always\"
run = { check = { name = \"validate\", run = \"checks/validate\", result_file = true } }
";

/// Fails with a summary and two failing items, and says so in its log.
const FAILING: &str = "#!/bin/sh
echo \"master's copy found 2 problems\"
printf '{\"summary\":\"2 problems\",\"failing\":[\"a.toml\",\"b.toml\"]}' > \"$BONGOS_CHECK_RESULT\"
exit 1
";

/// Passes with a summary.
const PASSING: &str = "#!/bin/sh
echo \"this copy passes\"
printf '{\"summary\":\"all good\"}' > \"$BONGOS_CHECK_RESULT\"
exit 0
";

/// Finds the project of the job, and no task.
struct Projects(ProjectRepo);

impl TaskSource for Projects {
    fn task(&self, _: &str, _: &gpui::App) -> Option<TaskChecks> {
        None
    }

    fn project(&self, project_id: &str, _: &gpui::App) -> Option<ProjectRepo> {
        (self.0.location().id == project_id).then(|| self.0.clone())
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
    repo: ProjectRepo,
    root: TempDir,
}

/// A project whose master declares [`LANDING_CHECKS`] and holds `script` as `checks/validate`,
/// with the engine installed and the kind and the hook registered while `task_checks.enabled` is
/// `enabled`.
fn world(cx: &mut TestAppContext, enabled: bool, script: &str) -> World {
    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_definition_folder(crate::CHECKS_FOLDER, crate::validate_folder);
    // Master gets the declaration and the script through a landing of its own, which checks the
    // declaration but runs nothing of it (master declared no landing check yet).
    repo.add_check_kind(crate::LANDING_KIND, parse_landing_check);
    let author = person();
    let copy = new_copy(&repo, "first-setup-0000", &author);
    write_definition(
        &copy,
        "project.toml",
        &format!("{PROJECT_TOML}{LANDING_CHECKS}"),
    );
    write_script(&copy.path.join("definitions/checks/validate"), script);
    let landed = land(&repo, &copy, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
    let counter = Arc::new(AtomicU64::new(0));
    let engine = cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        TaskChecksSettings::register(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let checks = content.task_checks.get_or_insert_default();
                checks.enabled = Some(enabled);
                checks.no_verdict_reruns = Some(0);
                checks.kill_grace_seconds = Some(1);
            });
        });
        let engine = ChecksEngine::install(
            ChecksEngine::new(
                Rc::new(Projects(repo.clone())),
                Rc::new(NoRuns),
                FakeClock::automatic(),
                // A counter, never the wall clock.
                Arc::new(move || {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    format!("2026-03-01T10:{:02}:{:02}Z", n / 60 % 60, n % 60)
                }),
            ),
            cx,
        );
        crate::landing::register(&engine, cx);
        repo.use_registered_checks(cx);
        engine
    });
    World { engine, repo, root }
}

fn write_script(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl World {
    /// A new working copy with `files` (relative to `definitions/`) written in it; its change id.
    fn change(&self, name: &str, files: &[(&str, &str)]) -> String {
        let copy = new_copy(&self.repo, name, &person());
        for (path, text) in files {
            write_definition(&copy, path, text);
        }
        copy.change_id
    }

    /// Lands (or only checks) `change_id` as the person at the keyboard on a thread of its own,
    /// driving the app's executor until it ends.
    fn land_change(
        &self,
        cx: &mut TestAppContext,
        change_id: &str,
        mode: LandMode,
    ) -> LandingResult {
        let repo = self.repo.clone();
        let change_id = change_id.to_string();
        let landing = std::thread::spawn(move || {
            block_on(repo.land_as_person(
                &change_id,
                LandRequest {
                    meta: ChangeMeta::new("land it", person()),
                    mode,
                },
                keyboard(),
                start(),
            ))
            .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        while !landing.is_finished() {
            cx.run_until_parked();
            assert!(Instant::now() < deadline, "the landing never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        landing.join().unwrap()
    }

    /// The landing records of check `validate` on `tree`.
    fn records_on(&self, tree: &str) -> Vec<bongos_checks_model::EvidenceRecord> {
        self.repo
            .state()
            .unwrap()
            .writer()
            .unwrap()
            .read(|read| scan_records(read, &landing_prefix(tree, "validate")))
            .unwrap()
    }

    /// The stored result of the `validate` entry for `change_id`, newest tree last.
    fn results(&self, change_id: &str) -> Vec<bongos_project_repo::CheckResult> {
        self.repo
            .check_results(change_id)
            .unwrap()
            .into_iter()
            .filter(|result| result.check == "validate")
            .collect()
    }

    fn pins(&self) -> Vec<PathBuf> {
        let pins = self.repo.location().state.join("pins");
        std::fs::read_dir(pins)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    }
}

#[gpui::test]
fn landing_a_failing_check_refuses_the_change_with_its_output(cx: &mut TestAppContext) {
    let world = world(cx, true, FAILING);
    let old = main_commit(&world.repo);
    let change = world.change("amber-otter-0001", &[("documents/note.md", "a note\n")]);

    let result = world.land_change(cx, &change, LandMode::Land);
    assert_eq!(
        result,
        LandingResult::Refused {
            reasons: vec!["validate: 2 problems; failing: a.toml, b.toml".to_string()]
        }
    );
    assert_eq!(main_commit(&world.repo), old, "nothing landed");

    let results = world.results(&change);
    assert_eq!(results.len(), 1, "{results:?}");
    let CheckOutcome::Fail { output, .. } = &results[0].outcome else {
        panic!("not a fail: {results:?}");
    };
    let output = output.as_ref().expect("the fail names its log");
    let log = std::fs::read_to_string(output).unwrap();
    assert!(log.contains("master's copy found 2 problems"), "{log}");

    // The record is the landing's, keyed by the exact tree it judged.
    let records = world.records_on(&results[0].tree);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].verdict, Verdict::Fail);
    assert_eq!(records[0].task_id, LANDING_PREFIX);
    assert_eq!(records[0].pin_commit, old, "scripts came from master");
}

#[gpui::test]
fn landing_a_change_to_the_check_script_is_judged_by_masters_copy(cx: &mut TestAppContext) {
    // Master's copy fails: a change that makes its own copy pass is still refused.
    let world = world(cx, true, FAILING);
    let old = main_commit(&world.repo);
    let change = world.change("amber-otter-0001", &[("checks/validate", PASSING)]);
    let result = world.land_change(cx, &change, LandMode::Land);
    assert_eq!(
        result,
        LandingResult::Refused {
            reasons: vec!["validate: 2 problems; failing: a.toml, b.toml".to_string()]
        }
    );
    assert_eq!(main_commit(&world.repo), old);
}

#[gpui::test]
fn landing_a_change_that_breaks_the_check_script_is_still_judged_by_masters_copy(
    cx: &mut TestAppContext,
) {
    // Master's copy passes: a change that breaks its own copy is judged by master's and lands.
    let world = world(cx, true, PASSING);
    let change = world.change("bold-crane-0002", &[("checks/validate", FAILING)]);
    let result = world.land_change(cx, &change, LandMode::Land);
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
}

/// Fails only when the tree holds both documents, so each change passes alone.
const BOTH_FAIL: &str = "#!/bin/sh
if [ -f definitions/documents/x.md ] && [ -f definitions/documents/y.md ]; then
    printf '{\"summary\":\"x and y together\"}' > \"$BONGOS_CHECK_RESULT\"
    exit 1
fi
printf '{\"summary\":\"fine\"}' > \"$BONGOS_CHECK_RESULT\"
exit 0
";

#[gpui::test]
fn landing_a_pass_for_one_tree_does_not_count_after_master_moved(cx: &mut TestAppContext) {
    let world = world(cx, true, BOTH_FAIL);
    let x = world.change("amber-otter-0001", &[("documents/x.md", "x\n")]);
    let checked = world.land_change(cx, &x, LandMode::CheckOnly);
    let LandingResult::Checked { tree: first, .. } = &checked else {
        panic!("not checked: {checked:?}");
    };
    assert_eq!(world.records_on(first)[0].verdict, Verdict::Pass);

    // Master moves under the change.
    let y = world.change("bold-crane-0002", &[("documents/y.md", "y\n")]);
    let landed = world.land_change(cx, &y, LandMode::Land);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");

    // The pass on the first tree is not the tree that would land now.
    let result = world.land_change(cx, &x, LandMode::Land);
    assert_eq!(
        result,
        LandingResult::Refused {
            reasons: vec!["validate: x and y together".to_string()]
        }
    );
    let results = world.results(&x);
    let second = results
        .iter()
        .find(|result| &result.tree != first)
        .expect("a result on the rebased tree");
    assert_eq!(world.records_on(&second.tree)[0].verdict, Verdict::Fail);
    assert_eq!(
        world.records_on(first).len(),
        1,
        "the first tree's pass stays its own"
    );
}

#[gpui::test]
fn landing_no_verdict_leaves_the_change_waiting_as_could_not_check(cx: &mut TestAppContext) {
    let world = world(cx, true, "#!/bin/sh\necho 'cannot tell'\nexit 3\n");
    let old = main_commit(&world.repo);
    let change = world.change("amber-otter-0001", &[("documents/note.md", "a note\n")]);
    let result = world.land_change(cx, &change, LandMode::Land);
    assert_eq!(
        result,
        LandingResult::NoVerdict {
            reason: "validate: undeclared exit code 3".to_string()
        }
    );
    assert_eq!(main_commit(&world.repo), old);
    let results = world.results(&change);
    let CheckOutcome::NoVerdict { output, .. } = &results[0].outcome else {
        panic!("not a no verdict: {results:?}");
    };
    let log = std::fs::read_to_string(output.as_ref().expect("the log")).unwrap();
    assert!(log.contains("cannot tell"), "{log}");
}

#[gpui::test]
fn landing_project_check_runs_the_checks_and_lands_nothing(cx: &mut TestAppContext) {
    let world = world(cx, true, PASSING);
    let old = main_commit(&world.repo);
    let change = world.change("amber-otter-0001", &[("documents/note.md", "a note\n")]);
    let result = world.land_change(cx, &change, LandMode::CheckOnly);
    let LandingResult::Checked { tree, checks } = &result else {
        panic!("not checked: {result:?}");
    };
    let validate = checks
        .iter()
        .find(|check| check.check == "validate")
        .expect("the landing check ran");
    assert_eq!(validate.outcome, CheckOutcome::Pass);
    assert_eq!(&validate.tree, tree);
    assert_eq!(main_commit(&world.repo), old, "a check lands nothing");
    assert_eq!(world.records_on(tree).len(), 1);
}

#[gpui::test]
fn landing_with_the_feature_off_the_kind_answers_no_verdict(cx: &mut TestAppContext) {
    let world = world(cx, false, PASSING);
    let old = main_commit(&world.repo);
    let change = world.change("amber-otter-0001", &[("documents/note.md", "a note\n")]);
    let result = world.land_change(cx, &change, LandMode::Land);
    assert_eq!(
        result,
        LandingResult::NoVerdict {
            reason: format!("validate: {CHECKS_OFF}")
        }
    );
    assert_eq!(main_commit(&world.repo), old);
    assert!(
        world
            .repo
            .state()
            .unwrap()
            .writer()
            .unwrap()
            .read(|read| scan_records(read, &format!("{LANDING_PREFIX}/")))
            .unwrap()
            .is_empty(),
        "no record is written while the feature is off"
    );

    // The engine also refuses a job it is handed while the setting is off.
    let (reply, answer) = oneshot::channel();
    let guard = block_on(world.repo.materialize("main")).unwrap();
    world.engine.update(cx, |engine, cx| {
        engine.start_landing(job(&world, guard.path(), reply), cx)
    });
    assert_eq!(answer_of(cx, answer), CheckOutcome::no_verdict(CHECKS_OFF));
}

/// Passes only while the change's tree and master's pin are both still there, and says where
/// it ran.
const SEES_ITS_TREES: &str = "#!/bin/sh
for file in \"$BONGOS_CHECK_PINNED/checks/validate\" definitions/project.toml \\
    definitions/documents/note.md; do
    test -f \"$file\" || { echo \"missing $file in $(pwd)\"; exit 1; }
done
echo \"tree $(pwd)\"
printf '{\"summary\":\"both trees there\"}' > \"$BONGOS_CHECK_RESULT\"
exit 0
";

#[gpui::test]
fn landing_the_pins_stay_materialized_while_the_check_runs(cx: &mut TestAppContext) {
    let world = world(cx, true, SEES_ITS_TREES);
    let change = world.change("amber-otter-0001", &[("documents/note.md", "a note\n")]);
    let result = world.land_change(cx, &change, LandMode::CheckOnly);
    let LandingResult::Checked { checks, tree } = &result else {
        panic!("not checked: {result:?}");
    };
    let outcome = checks
        .iter()
        .find(|check| check.check == "validate")
        .map(|check| check.outcome.clone());
    if let Some(
        CheckOutcome::Fail {
            output: Some(output),
            ..
        }
        | CheckOutcome::NoVerdict {
            output: Some(output),
            ..
        },
    ) = &outcome
    {
        panic!("{}", std::fs::read_to_string(output).unwrap_or_default());
    }
    assert_eq!(outcome, Some(CheckOutcome::Pass));
    let record = &world.records_on(tree)[0];
    let log = std::fs::read_to_string(crate::process::part_output_path(
        &world.root.path().join("project/files/landing/checks"),
        &record.id,
        1,
    ))
    .unwrap();
    let pins = world.repo.location().state.join("pins");
    assert!(
        log.contains(&format!("tree {}", pins.display())),
        "the check ran in the landing's copy of the tree: {log}"
    );
    // Both copies are gone once the check answered and the landing let go of them.
    assert_eq!(world.pins(), Vec::<PathBuf>::new());
}

/// The answer to a job, once the engine sent it; the process it stopped may take a moment to end.
fn answer_of(cx: &mut TestAppContext, mut answer: oneshot::Receiver<CheckOutcome>) -> CheckOutcome {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        cx.run_until_parked();
        if let Ok(Some(outcome)) = answer.try_recv() {
            return outcome;
        }
        assert!(Instant::now() < deadline, "the job was never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Says it started, then runs until it is stopped.
fn blocking_script(mark: &Path) -> String {
    format!(
        "#!/bin/sh\necho started > '{}'\nwhile :; do sleep 0.05; done\n",
        mark.display()
    )
}

fn job(world: &World, root: &Path, reply: oneshot::Sender<CheckOutcome>) -> LandingJob {
    LandingJob {
        decl: bongos_checks_model::CheckDecl {
            name: "validate".to_string(),
            run: Some("checks/validate".to_string()),
            ..Default::default()
        },
        master: main_commit(&world.repo),
        tree: "t1".to_string(),
        root: root.to_path_buf(),
        project_id: world.repo.location().id.clone(),
        reply,
    }
}

#[gpui::test]
fn landing_drain_answers_a_pending_landing_job_with_no_verdict(cx: &mut TestAppContext) {
    let mark_dir = TempDir::new().unwrap();
    let mark = mark_dir.path().join("started");
    let world = world(cx, true, &blocking_script(&mark));
    let guard = block_on(world.repo.materialize("main")).unwrap();

    let (reply, answer) = oneshot::channel();
    world.engine.update(cx, |engine, cx| {
        engine.start_landing(job(&world, guard.path(), reply), cx)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !mark.exists() {
        cx.run_until_parked();
        assert!(Instant::now() < deadline, "the check never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    let stopped = finish(cx, drain);
    assert!(stopped.is_empty(), "a landing check is no workflow node");
    let answered = answer_of(cx, answer);
    let CheckOutcome::NoVerdict { reason, output } = &answered else {
        panic!("not a no verdict: {answered:?}");
    };
    assert_eq!(reason, MOVING_TO_HOST);
    assert!(output.is_some(), "the stopped check's log is named");
    assert!(
        world
            .engine
            .read_with(cx, |engine, _| engine.landing.is_empty())
    );

    // A job that arrives while drained is answered at once.
    let (reply, answer) = oneshot::channel();
    world.engine.update(cx, |engine, cx| {
        engine.start_landing(job(&world, guard.path(), reply), cx)
    });
    assert_eq!(
        answer_of(cx, answer),
        CheckOutcome::no_verdict(MOVING_TO_HOST)
    );
}

#[test]
fn landing_the_kind_parses_one_declaration_and_refuses_a_bad_one() {
    let value = |text: &str| toml::from_str::<toml::Value>(text).unwrap();
    let check =
        parse_landing_check(&value("run = \"checks/validate\"\nresult_file = true\n")).unwrap();
    // With no name of its own, the check is named by what it runs.
    assert_eq!(check.name(), "checks/validate");
    let check =
        parse_landing_check(&value("name = \"lint\"\ncommand = [\"make\", \"lint\"]\n")).unwrap();
    assert_eq!(check.name(), "lint");

    let error = parse_landing_check(&value("run = \"checks/a\"\ncommand = [\"b\"]\n"))
        .err()
        .unwrap();
    assert!(error.contains("both run and command"), "{error}");
    let error = parse_landing_check(&toml::Value::String("checks/a".to_string()))
        .err()
        .unwrap();
    assert!(error.contains("must be a table"), "{error}");
}

/// Says the tasks it names are started.
struct Started(BTreeSet<String>);

impl LiveSource for Started {
    fn name(&self) -> &str {
        "test"
    }

    fn started(&self, task_id: &str) -> bool {
        self.0.contains(task_id)
    }
}

static ON: AtomicBool = AtomicBool::new(true);
static OFF: AtomicBool = AtomicBool::new(false);

const REQUIRED_TESTS: &str = "\n[[checks.check]]\nname = \"tests\"\nrun = \"checks/tests\"\n";

/// A tree with the top group and two leaves, `l1` ("Invoice export") and `l2` ("Backfill"), whose
/// checks tables are `l1_checks` and `l2_checks`; its root folder.
fn tree(root: &Path, l1_checks: &str, l2_checks: &str) -> PathBuf {
    let definitions = root.join("definitions");
    for (path, text) in [
        ("project.toml", PROJECT_TOML.to_string()),
        ("task.toml", definition_toml("g1", "Invoicing", "")),
        (
            "tasks/invoice-export/task.toml",
            definition_toml("l1", "Invoice export", l1_checks),
        ),
        (
            "tasks/backfill/task.toml",
            definition_toml("l2", "Backfill", l2_checks),
        ),
    ] {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    root.to_path_buf()
}

/// What `hook` says of a change from master (both leaves with [`REQUIRED_TESTS`]) to a change
/// whose leaf `edited` has `checks` instead, with only `l1` started.
fn judge(hook: &ChecksLoosening, edited: &str, checks: &str) -> ApprovalOpinion {
    let folder = TempDir::new().unwrap();
    let master_root = tree(
        &folder.path().join("master"),
        REQUIRED_TESTS,
        REQUIRED_TESTS,
    );
    let (l1, l2) = if edited == "l1" {
        (checks, REQUIRED_TESTS)
    } else {
        (REQUIRED_TESTS, checks)
    };
    let change_root = tree(&folder.path().join("change"), l1, l2);
    let master = DefinitionTree::parse_dir(&master_root.join("definitions")).unwrap();
    let change = DefinitionTree::parse_dir(&change_root.join("definitions")).unwrap();
    let mut live = LiveFacts::default();
    live.register(Arc::new(Started(BTreeSet::from(["l1".to_string()]))));
    let summary = ChangeSummary::default();
    let meta = ChangeMeta::new("edit checks", person());
    hook.judge(&ApprovalInput {
        change_id: "c1",
        project_id: "project-1",
        summary: &summary,
        meta: &meta,
        live: &live,
        master: &master,
        change: &change,
        master_root: &master_root,
        change_root: &change_root,
    })
}

#[test]
fn landing_loosening_a_started_tasks_required_check_needs_a_person() {
    let hook = ChecksLoosening::new(&ON);
    let removed = "";
    let advised = format!("{REQUIRED_TESTS}required = false\n");
    let widened = format!("{REQUIRED_TESTS}pass = [0, 2]\n");
    for (checks, details) in [
        (removed, "removed"),
        (advised.as_str(), "made advisory"),
        (widened.as_str(), "pass codes 0 -> 0, 2"),
    ] {
        assert_eq!(
            judge(&hook, "l1", checks),
            ApprovalOpinion::NeedsPerson(format!(
                "loosens check tests of started task Invoice export: {details}"
            )),
            "{checks}"
        );
        // The same edit on a task that has not started is not judged.
        assert_eq!(
            judge(&hook, "l2", checks),
            ApprovalOpinion::NoOpinion,
            "{checks}"
        );
    }
    // Tightening a started task's check needs nobody.
    assert_eq!(
        judge(
            &hook,
            "l1",
            &format!("{REQUIRED_TESTS}budget_minutes = 5\n")
        ),
        ApprovalOpinion::NoOpinion
    );
}

#[test]
fn landing_with_the_feature_off_the_loosening_hook_has_no_opinion() {
    let widened = format!("{REQUIRED_TESTS}pass = [0, 2]\n");
    assert_eq!(
        judge(&ChecksLoosening::new(&OFF), "l1", &widened),
        ApprovalOpinion::NoOpinion
    );
}
