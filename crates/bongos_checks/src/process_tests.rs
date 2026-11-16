use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_checks_model::decl::CheckDecl;
use bongos_checks_model::evidence::{PartRecord, TreeKind};
use bongos_checks_model::verdict::{Exit, NoVerdictReason, Verdict};
use bongos_project_repo::ProjectRepo;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::output::{OutputSink, cut_marker};
use crate::pin::Pin;
use crate::process::{
    PartOutcome, PartScope, PartSpec, PartStart, StopHandle, kill_orphan_parts, part_env, run_part,
};
use crate::test_support::{commit_everything, fixture_project, new_copy, person, write_definition};
use crate::trees::{EnvBase, SecondTree, TaskTrees};

const SCRIPTS: [&str; 6] = [
    "exit-code",
    "write-env",
    "show-pinned",
    "slow-output",
    "ignore-term",
    "print-300mb",
];

/// Fields drop in order: the pin goes before the folder holding it.
struct Fixture {
    pin: Arc<Pin>,
    repo: ProjectRepo,
    tree: PathBuf,
    root: TempDir,
}

/// A project whose master holds every fixture script under `definitions/checks/` and the
/// registry `definitions/registry/flaky-tests.md`, pinned at `main`, and an empty tree.
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
    let registry = repository.join("definitions/registry");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::write(registry.join("flaky-tests.md"), "registry version 1\n").unwrap();
    commit_everything(&repository, "add checks");
    let pin = Arc::new(block_on(Pin::materialize(&repo, "main")).unwrap());
    let tree = root.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    Fixture {
        root,
        repo,
        tree,
        pin,
    }
}

fn script(name: &str, script: &str) -> CheckDecl {
    CheckDecl {
        name: name.to_string(),
        run: Some(format!("checks/{script}")),
        ..CheckDecl::default()
    }
}

impl Fixture {
    fn spec(&self, decl: CheckDecl) -> PartSpec {
        PartSpec {
            output: self
                .root
                .path()
                .join("output")
                .join(&decl.name)
                .join("part-1.log"),
            decl,
            pin: self.pin.clone(),
            master: crate::pin::master_definitions(&self.repo),
            tree: self.tree.clone(),
            tree_kind: TreeKind::Tip,
            tree_key: "tree-key-1".to_string(),
            part: 1,
            parts: 1,
            scope: PartScope::Unscoped,
            role: crate::process::ProcessRole::Part,
            stderr: None,
            env: EnvBase::Own,
            evidence: None,
            output_limit: Some(1024 * 1024),
            result_limit: 64 * 1024,
            budget: None,
            kill_grace: Duration::from_secs(5),
            stop: StopHandle::default(),
            on_start: None,
        }
    }

    fn run(&self, decl: CheckDecl) -> PartOutcome {
        block_on(run_part(self.spec(decl)))
    }

    fn write_tree(&self, name: &str, text: &str) {
        std::fs::write(self.tree.join(name), text).unwrap();
    }

    fn env_names(&self) -> Vec<String> {
        std::fs::read_to_string(self.tree.join("env-names"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn output_text(spec_output: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(spec_output).unwrap()).into_owned()
}

/// Waits (polling, bounded) until `ready` holds.
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let rest = &stat[stat.rfind(')').unwrap() + 1..];
            rest.split_whitespace().next() != Some("Z")
        }
        Err(_) => false,
    }
}

#[test]
fn each_exit_class_gets_its_verdict() {
    let fixture = fixture();
    let summary = |decl: CheckDecl| CheckDecl {
        summary: Some("^test result: ok$".to_string()),
        ..decl
    };
    let cases: Vec<(&str, CheckDecl, Verdict)> = vec![
        ("0", summary(script("pass", "exit-code")), Verdict::Pass),
        ("1", script("fail", "exit-code"), Verdict::Fail),
        (
            "3",
            CheckDecl {
                environment: Some(vec![3]),
                ..script("environment", "exit-code")
            },
            Verdict::NoVerdict(NoVerdictReason::Environment { code: 3 }),
        ),
        (
            "7",
            script("undeclared", "exit-code"),
            Verdict::NoVerdict(NoVerdictReason::UndeclaredExit { code: 7 }),
        ),
        (
            "kill",
            script("killed", "exit-code"),
            Verdict::NoVerdict(NoVerdictReason::Killed { signal: 9 }),
        ),
        (
            "0",
            CheckDecl {
                summary: Some("^all green$".to_string()),
                ..script("no-summary", "exit-code")
            },
            Verdict::NoVerdict(NoVerdictReason::MissingSummary),
        ),
    ];
    for (code, decl, expected) in cases {
        fixture.write_tree("exit-code", code);
        let name = decl.name.clone();
        let outcome = fixture.run(decl);
        assert_eq!(outcome.verdict, expected, "check {name}");
        assert!(outcome.start.is_some(), "check {name} started");
    }

    fixture.write_tree("exit-code", "0");
    let with_result = || CheckDecl {
        result_file: Some(true),
        ..script("result", "exit-code")
    };
    let outcome = fixture.run(with_result());
    assert!(
        matches!(
            outcome.verdict,
            Verdict::NoVerdict(NoVerdictReason::InvalidResult(_))
        ),
        "a declared result file that was never written is no verdict: {:?}",
        outcome.verdict
    );

    fixture.write_tree("result.json", r#"{"summary": "12 passed", "bogus": 1}"#);
    let outcome = fixture.run(with_result());
    assert!(matches!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::InvalidResult(_))
    ));

    fixture.write_tree(
        "result.json",
        r#"{"summary": "1 failed", "failing": ["billing::totals"]}"#,
    );
    fixture.write_tree("exit-code", "1");
    let outcome = fixture.run(with_result());
    assert_eq!(outcome.verdict, Verdict::Fail);
    assert_eq!(outcome.exit, Some(Exit::Code(1)));
    assert_eq!(
        outcome.result.unwrap().failing,
        vec!["billing::totals".to_string()]
    );

    fixture.write_tree("result.json", r#"{"summary": "12 passed"}"#);
    fixture.write_tree("exit-code", "0");
    let outcome = fixture.run(with_result());
    assert_eq!(outcome.verdict, Verdict::Pass);
    assert_eq!(outcome.result.unwrap().summary, "12 passed");
}

#[test]
fn the_script_and_registry_are_read_from_the_pin_not_the_working_copy() {
    let fixture = fixture();
    let copy = new_copy(&fixture.repo, "agent", &person());
    write_definition(
        &copy,
        "checks/show-pinned",
        "#!/bin/sh\necho \"script version 2\"\nexit 0\n",
    );
    std::fs::set_permissions(
        copy.path.join("definitions/checks/show-pinned"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    write_definition(&copy, "registry/flaky-tests.md", "registry version 2\n");

    let mut spec = fixture.spec(CheckDecl {
        pinned: vec!["registry/flaky-tests.md".to_string()],
        ..script("pinned", "show-pinned")
    });
    spec.tree = copy.path;
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass);
    let text = output_text(&output);
    assert!(text.contains("script version 1"), "{text}");
    assert!(text.contains("registry version 1"), "{text}");
    assert!(!text.contains("version 2"), "{text}");
    assert!(
        Path::new(&outcome.command[0])
            .starts_with(fixture.pin.definitions().canonicalize().unwrap()),
        "the program runs from the pin: {:?}",
        outcome.command
    );
}

#[test]
fn the_pin_cannot_be_written() {
    let fixture = fixture();
    let definitions = fixture.pin.definitions();
    // SAFETY: geteuid only reads this process's id.
    if unsafe { libc::geteuid() } == 0 {
        // Root ignores file modes.
        return;
    }
    assert!(std::fs::write(definitions.join("checks/new-script"), "x").is_err());
    assert!(std::fs::remove_file(definitions.join("checks/exit-code")).is_err());
    assert!(std::fs::write(definitions.join("checks/exit-code"), "x").is_err());
    let pin_root = definitions.parent().unwrap().to_path_buf();
    drop(fixture.pin);
    assert!(!pin_root.exists(), "dropping the pin removes the copy");
}

#[test]
fn paths_leaving_the_definitions_are_refused() {
    let fixture = fixture();
    for path in ["../repository/definitions/checks/exit-code", "/bin/true"] {
        let outcome = fixture.run(CheckDecl {
            name: "escape".to_string(),
            run: Some(path.to_string()),
            ..CheckDecl::default()
        });
        assert!(
            matches!(
                outcome.verdict,
                Verdict::NoVerdict(NoVerdictReason::StartFailed(_))
            ),
            "{path}: {:?}",
            outcome.verdict
        );
        assert!(outcome.start.is_none());
    }
    let outcome = fixture.run(CheckDecl {
        pinned: vec!["../secrets".to_string()],
        ..script("pinned-escape", "exit-code")
    });
    assert!(matches!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::StartFailed(_))
    ));
}

#[test]
fn a_missing_command_or_script_is_tool_not_found() {
    let fixture = fixture();
    let outcome = fixture.run(CheckDecl {
        name: "missing".to_string(),
        command: Some(vec!["bongos-no-such-tool-anywhere".to_string()]),
        ..CheckDecl::default()
    });
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::ToolNotFound(
            "bongos-no-such-tool-anywhere".to_string()
        ))
    );
    let outcome = fixture.run(script("missing-script", "no-such-script"));
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::ToolNotFound(
            "checks/no-such-script".to_string()
        ))
    );
}

#[test]
fn a_script_that_cannot_start_names_no_environment_value() {
    let fixture = fixture();
    let repository = fixture.repo.location().repository.clone();
    let script_path = repository.join("definitions/checks/not-executable");
    std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    commit_everything(&repository, "add a script without its executable bit");
    let pin = Arc::new(block_on(Pin::materialize(&fixture.repo, "main")).unwrap());
    let secret = "a-distinctive-fake-secret-4417";
    let mut spec = fixture.spec(script("not-executable", "not-executable"));
    spec.pin = pin;
    spec.env = EnvBase::Lane(vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap()),
        ("DEPLOY_TOKEN".to_string(), secret.to_string()),
    ]);
    let outcome = block_on(run_part(spec));
    let Verdict::NoVerdict(NoVerdictReason::StartFailed(reason)) = &outcome.verdict else {
        panic!("{:?}", outcome.verdict);
    };
    assert!(!reason.contains(secret), "{reason}");
    assert!(!reason.contains("DEPLOY_TOKEN"), "{reason}");
    assert!(!reason.contains("BONGOS_CHECK_TOKEN"), "{reason}");
    assert!(reason.contains("not-executable"), "{reason}");
}

#[test]
fn a_command_is_found_on_the_path_and_gets_its_arguments_unchanged() {
    let fixture = fixture();
    let outcome = fixture.run(CheckDecl {
        name: "command".to_string(),
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "test \"$0\" = '$HOME;x'".to_string(),
            "$HOME;x".to_string(),
        ]),
        ..CheckDecl::default()
    });
    assert_eq!(outcome.verdict, Verdict::Pass);
    assert!(outcome.command[0].ends_with("/sh"), "{:?}", outcome.command);
}

#[test]
fn an_unset_variable_is_absent_and_the_check_variables_are_set() {
    let fixture = fixture();
    let path = std::env::var("PATH").unwrap();
    let mut spec = fixture.spec(CheckDecl {
        unset: vec!["TEST_FILTER".to_string()],
        ..script("env", "write-env")
    });
    spec.env = EnvBase::Lane(vec![
        ("PATH".to_string(), path),
        ("TEST_FILTER".to_string(), "narrow".to_string()),
        ("KEEP_ME".to_string(), "1".to_string()),
        ("BONGOS_CHECK_SCOPE".to_string(), "stale".to_string()),
    ]);
    spec.scope = PartScope::All;
    assert_eq!(block_on(run_part(spec)).verdict, Verdict::Pass);
    let names = fixture.env_names();
    assert!(!names.contains(&"TEST_FILTER".to_string()), "{names:?}");
    assert!(names.contains(&"KEEP_ME".to_string()), "{names:?}");
    for name in [
        "BONGOS_CHECK_NAME",
        "BONGOS_CHECK_PART",
        "BONGOS_CHECK_PARTS",
        "BONGOS_CHECK_PARAMS",
        "BONGOS_CHECK_RESULT",
        "BONGOS_CHECK_TREE",
        "BONGOS_CHECK_TREE_KEY",
        "BONGOS_CHECK_PINNED",
        "BONGOS_CHECK_MASTER",
        "BONGOS_CHECK_TOKEN",
        "BONGOS_CHECK_SCOPE",
    ] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }

    // On bongos's own environment too; `unset` cannot remove the check's own variables.
    assert!(std::env::var_os("HOME").is_some());
    let mut spec = fixture.spec(CheckDecl {
        unset: vec![
            "HOME".to_string(),
            "BONGOS_CHECK_TOKEN".to_string(),
            "BONGOS_CHECK_RESULT".to_string(),
            "BONGOS_CHECK_PARAMS".to_string(),
        ],
        ..script("env", "write-env")
    });
    spec.env = EnvBase::Own;
    assert_eq!(block_on(run_part(spec)).verdict, Verdict::Pass);
    let names = fixture.env_names();
    assert!(!names.contains(&"HOME".to_string()), "{names:?}");
    for name in [
        "BONGOS_CHECK_TOKEN",
        "BONGOS_CHECK_RESULT",
        "BONGOS_CHECK_PARAMS",
        "BONGOS_CHECK_NAME",
    ] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }
}

#[test]
fn unset_removes_inherited_variables_but_never_the_check_variables() {
    let base = EnvBase::Lane(vec![
        ("A".to_string(), "1".to_string()),
        ("B".to_string(), "2".to_string()),
        ("BONGOS_CHECK_ITEMS".to_string(), "/stale".to_string()),
        ("BONGOS_SCOPE_RESULT".to_string(), "/stale".to_string()),
    ]);
    let env = part_env(
        &base,
        &[
            ("BONGOS_CHECK_NAME".to_string(), OsString::from("tests")),
            ("BONGOS_CHECK_TOKEN".to_string(), OsString::from("t")),
        ],
        &[
            "A".to_string(),
            "BONGOS_CHECK_NAME".to_string(),
            "BONGOS_CHECK_TOKEN".to_string(),
        ],
    );
    let names: Vec<_> = env.keys().map(|name| name.to_string_lossy()).collect();
    assert_eq!(
        names,
        ["B", "BONGOS_CHECK_NAME", "BONGOS_CHECK_TOKEN"],
        "{env:?}"
    );
}

struct FakeTrees {
    env: Vec<(String, String)>,
}

impl TaskTrees for FakeTrees {
    fn tip(&self, _task_id: &str, _repo: &str) -> Option<PathBuf> {
        None
    }

    fn second_tree(&self, _task_id: &str, _repo: &str) -> SecondTree {
        SecondTree::Unavailable
    }

    fn env(&self, _task_id: &str, _repo: &str) -> Vec<(String, String)> {
        self.env.clone()
    }
}

#[test]
fn a_variable_missing_from_the_lane_environment_is_absent() {
    let fixture = fixture();
    assert!(
        std::env::var_os("HOME").is_some(),
        "the test needs HOME in its own environment"
    );
    let trees = FakeTrees {
        env: vec![
            ("PATH".to_string(), std::env::var("PATH").unwrap()),
            ("LANE_ONLY".to_string(), "1".to_string()),
        ],
    };
    let mut spec = fixture.spec(script("env", "write-env"));
    spec.env = EnvBase::for_tip(Some(&trees), "l1", "app");
    assert_eq!(block_on(run_part(spec)).verdict, Verdict::Pass);
    let names = fixture.env_names();
    assert!(!names.contains(&"HOME".to_string()), "{names:?}");
    assert!(names.contains(&"LANE_ONLY".to_string()), "{names:?}");

    let empty = FakeTrees { env: Vec::new() };
    assert_eq!(EnvBase::for_tip(Some(&empty), "l1", "app"), EnvBase::Own);
    assert_eq!(EnvBase::for_tip(None, "l1", "app"), EnvBase::Own);
}

#[test]
fn output_is_readable_while_the_part_runs() {
    let fixture = fixture();
    let spec = fixture.spec(script("slow", "slow-output"));
    let output = spec.output.clone();
    let stop = spec.stop.clone();
    let running = std::thread::spawn(move || block_on(run_part(spec)));
    wait_for("the first line", || {
        std::fs::read_to_string(&output).is_ok_and(|text| text.contains("first line"))
    });
    stop.stop();
    let outcome = running.join().unwrap();
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert!(!output_text(&output).contains("late line"));
}

#[test]
fn output_past_the_limit_is_cut_in_the_middle_with_a_marker() {
    let fixture = fixture();
    let limit = 1024 * 1024;
    let mut spec = fixture.spec(script("big", "print-300mb"));
    spec.output_limit = Some(limit);
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass);
    let printed = 314_572_800 + "the end\n".len() as u64;
    assert_eq!(outcome.output.total_bytes, printed);
    let marker = cut_marker(outcome.output.cut_bytes);
    let size = std::fs::metadata(&output).unwrap().len();
    assert!(size <= limit + marker.len() as u64, "{size}");
    assert_eq!(
        size,
        printed - outcome.output.cut_bytes + marker.len() as u64
    );
    let text = output_text(&output);
    assert!(text.starts_with("a line of check output that repeats\n"));
    assert!(text.contains(&marker));
    assert!(text.ends_with("the end\n"));
    let leftovers: Vec<_> = std::fs::read_dir(output.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().contains("tail"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn the_output_sink_keeps_head_and_tail_around_the_marker() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("part-1.log");
    let mut sink = OutputSink::create(
        &path,
        Some(8),
        Some(regex::Regex::new("^test result: ok$").unwrap()),
    )
    .unwrap();
    for chunk in ["0123", "456", "789ab", "cdef"] {
        sink.write(chunk.as_bytes());
    }
    let summary = sink.finish();
    assert_eq!(summary.total_bytes, 16);
    assert_eq!(summary.cut_bytes, 8);
    assert!(!summary.summary_seen);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("0123{}cdef", cut_marker(8))
    );

    let mut sink = OutputSink::create(
        &path,
        Some(1024),
        Some(regex::Regex::new("^test result: ok$").unwrap()),
    )
    .unwrap();
    sink.write(b"first\ntest res");
    sink.write(b"ult: ok\r\nlast");
    let summary = sink.finish();
    assert!(summary.summary_seen);
    assert_eq!(summary.cut_bytes, 0);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "first\ntest result: ok\r\nlast"
    );
}

#[test]
fn reaching_the_time_budget_is_no_verdict() {
    let fixture = fixture();
    let mut spec = fixture.spec(script("slow", "slow-output"));
    spec.budget = Some(Duration::from_millis(300));
    let output = spec.output.clone();
    let began = Instant::now();
    let outcome = block_on(run_part(spec));
    let took = began.elapsed();
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::TimeBudgetReached)
    );
    // Bound: 300 ms budget + 5 s kill grace (the fixture's default) + 3 s slack.
    assert!(
        took < Duration::from_millis(300) + Duration::from_secs(5 + 3),
        "took {took:?}"
    );
    // Stopped long before the script's own end, however loaded the machine: it never got past
    // its sleep.
    let text = output_text(&output);
    assert!(text.contains("first line"), "{text}");
    assert!(!text.contains("late line"), "{text}");
}

#[cfg(target_os = "linux")]
#[test]
fn cancel_ends_a_script_ignoring_sigterm_and_its_detached_child_within_the_grace() {
    let fixture = fixture();
    let mut spec = fixture.spec(script("stubborn", "ignore-term"));
    spec.kill_grace = Duration::from_secs(2);
    let started: Arc<Mutex<Option<PartStart>>> = Arc::default();
    let seen = started.clone();
    spec.on_start = Some(Box::new(move |start| {
        *seen.lock().unwrap() = Some(start.clone());
    }));
    let stop = spec.stop.clone();
    let running = std::thread::spawn(move || block_on(run_part(spec)));
    let pid_file = fixture.tree.join("detached.pid");
    wait_for("the detached child", || pid_file.exists());
    let detached: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_alive(detached));
    let leader = started.lock().unwrap().clone().unwrap();
    assert!(process_alive(leader.pid));

    let cancelled = Instant::now();
    stop.stop();
    let outcome = running.join().unwrap();
    let took = cancelled.elapsed();
    // SIGTERM is ignored, so only the SIGKILL one second before the end of the grace ends it.
    // The script never ends by itself; the bound leaves room past the two-second grace for
    // finding the processes (grace plus two seconds).
    let bound = Duration::from_secs(2 + 2);
    assert!(took >= Duration::from_secs(1), "took {took:?}");
    assert!(took <= bound, "took {took:?}");
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert_eq!(outcome.start, Some(leader.clone()));
    assert!(!process_alive(leader.pid));
    wait_for("the detached child to be reaped", || {
        !process_alive(detached)
    });
    assert!(
        cancelled.elapsed() <= bound,
        "the detached child outlived the grace"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn orphan_parts_are_stopped_by_their_record() {
    let fixture = fixture();
    let mut spec = fixture.spec(script("orphan", "ignore-term"));
    let started: Arc<Mutex<Option<PartStart>>> = Arc::default();
    let seen = started.clone();
    spec.on_start = Some(Box::new(move |start| {
        *seen.lock().unwrap() = Some(start.clone());
    }));
    let running = std::thread::spawn(move || block_on(run_part(spec)));
    let pid_file = fixture.tree.join("detached.pid");
    wait_for("the detached child", || pid_file.exists());
    let detached: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let start = started.lock().unwrap().clone().unwrap();
    let open = PartRecord {
        n: 1,
        verdict: Verdict::NoVerdict(NoVerdictReason::Interrupted),
        reason: None,
        exit: None,
        started: "s".to_string(),
        ended: None,
        pid: Some(start.pid),
        pid_start_time: Some(start.pid_start_time),
        token: Some(start.token),
        duration_ms: None,
        summary: None,
        failing: Vec::new(),
    };
    let finished = PartRecord {
        n: 2,
        ended: Some("e".to_string()),
        ..open.clone()
    };
    let outcomes = kill_orphan_parts(&[open, finished]);
    assert_eq!(
        outcomes,
        vec![
            bongos_workflow::process::OrphanOutcome::Killed,
            bongos_workflow::process::OrphanOutcome::NotRunning
        ]
    );
    let outcome = running.join().unwrap();
    assert_eq!(
        outcome.verdict,
        Verdict::NoVerdict(NoVerdictReason::Killed { signal: 9 })
    );
    wait_for("the detached child to be reaped", || {
        !process_alive(detached)
    });
}
