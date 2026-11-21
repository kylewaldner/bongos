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
    // Under the resolved temp folder: declared directories reached through a symlink are refused,
    // and the temp folder itself may sit below one.
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
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
            confinement: None,
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

// Confined parts (`confinement.task_checks`).

/// A check that runs `script` with `sh -c`.
fn shell(name: &str, script: &str) -> CheckDecl {
    CheckDecl {
        name: name.to_string(),
        command: Some(vec!["sh".to_string(), "-c".to_string(), script.to_string()]),
        ..CheckDecl::default()
    }
}

/// Serializes the tests that point the process-wide job roots registry at their own folder.
static REGISTRY: Mutex<()> = Mutex::new(());

/// Makes the job roots registry of this test's own folder, for as long as the returned guard is
/// held (the whole test): the registry is process-wide, so tests running as threads take turns.
#[must_use]
fn own_registry(fixture: &Fixture) -> std::sync::MutexGuard<'static, ()> {
    let guard = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    util::job_roots::init(&fixture.root.path().join("registry").join("roots.json")).unwrap();
    guard
}

/// A repository in the fixture's tree with one commit of `file`.
fn tree_repository(fixture: &Fixture) {
    crate::test_support::init_git_repo(&fixture.tree);
    fixture.write_tree("file", "one\n");
    commit_everything(&fixture.tree, "first");
}

/// Whether the OS sandbox can be created here, else says why the test skips.
fn sandbox_available(fixture: &Fixture) -> bool {
    let policy = sandbox::SandboxPolicy {
        fs: sandbox::SandboxFsPolicy::Restricted {
            writable_paths: vec![fixture.tree.clone()],
        },
        network: sandbox::SandboxNetPolicy::Unrestricted,
        git: sandbox::GitSandboxPolicy::default(),
        read_only_masks: Vec::new(),
    };
    match sandbox::Sandbox::can_create(&policy, Some(&fixture.tree)) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipped: no OS sandbox ({error})");
            false
        }
    }
}

fn confined_spec(fixture: &Fixture, decl: CheckDecl, dirs: &[&Path]) -> PartSpec {
    let mut spec = fixture.spec(decl);
    spec.confinement = Some(crate::process::PartConfinement::default());
    spec.env = EnvBase::Declared {
        env: Vec::new(),
        dirs: dirs.iter().map(|dir| dir.to_path_buf()).collect(),
    };
    spec
}

#[test]
fn a_confined_check_builds_into_declared_directories() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let build = fixture.root.path().join("lane/build");
    let index = fixture.root.path().join("lane/index");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::write(index.join("entries"), "old\n").unwrap();
    let script = format!(
        "mkdir -p {build}/deep && echo built > {build}/deep/out && echo new >> {index}/entries \
         && echo tree > in-tree && echo scratch > \"$TMPDIR/note\"",
        build = build.display(),
        index = index.display()
    );
    let spec = confined_spec(&fixture, shell("build", &script), &[&build, &index]);
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    assert_eq!(
        std::fs::read_to_string(build.join("deep/out")).unwrap(),
        "built\n"
    );
    assert_eq!(
        std::fs::read_to_string(index.join("entries")).unwrap(),
        "old\nnew\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("in-tree")).unwrap(),
        "tree\n"
    );
    let scratch = output.parent().unwrap().join("part-1.scratch");
    assert!(
        !scratch.exists(),
        "the part's scratch is removed at the end"
    );
}

#[test]
fn a_confined_check_cannot_write_an_undeclared_directory_and_the_log_says_so() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let undeclared = fixture.root.path().join("undeclared");
    std::fs::create_dir_all(&undeclared).unwrap();
    let target = undeclared.join("escaped");
    let spec = confined_spec(
        &fixture,
        shell("escape", &format!("touch {}", target.display())),
        &[],
    );
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Fail, "{}", output_text(&output));
    let log = output_text(&output);
    let note = format!("bongos: {}: {}", bongos_confine::RULE, target.display());
    assert!(log.contains(&note), "{log}");
    assert!(!target.exists());
}

#[test]
fn a_confined_scope_script_answers_from_its_scratch_and_cannot_write_its_staging_folder() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let staging = fixture.root.path().join("output/scope");
    std::fs::create_dir_all(&staging).unwrap();
    let changed = staging.join("changed-files.txt");
    std::fs::write(&changed, "src/lib.rs\n").unwrap();
    let planted = staging.join("planted");
    let script = format!(
        "echo '{{\"scope\": \"all\", \"reason\": \"r\"}}' > \"$BONGOS_SCOPE_RESULT\" && \
         if echo x > {planted}; then exit 3; fi",
        planted = planted.display()
    );
    let mut spec = confined_spec(&fixture, shell("scope", &script), &[]);
    spec.output = staging.join("scope.log");
    spec.role = crate::process::ProcessRole::Scope {
        changed_files: changed,
        result: staging.join("scope-answer.json"),
    };
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(
        outcome.exit,
        Some(Exit::Code(0)),
        "{}",
        output_text(&output)
    );
    let answer = String::from_utf8(outcome.scope_answer.unwrap()).unwrap();
    assert!(answer.contains("\"all\""), "{answer}");
    assert!(!planted.exists(), "the staging folder was writable");
    assert!(!staging.join("scope-answer.json").exists());
    assert!(!staging.join("scope.scratch").exists());
}

#[test]
fn a_stale_scratch_symlink_is_replaced_not_followed() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "kept\n").unwrap();
    let spec = confined_spec(
        &fixture,
        shell("stale", "echo note > \"$TMPDIR/note\""),
        &[],
    );
    let output = spec.output.clone();
    let scratch = output.parent().unwrap().join("part-1.scratch");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &scratch).unwrap();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    assert!(
        !outside.join("note").exists(),
        "the job wrote through the link"
    );
    assert!(!outside.join("part-1.params.json").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep")).unwrap(),
        "kept\n"
    );
    assert!(std::fs::symlink_metadata(&scratch).is_err());
}

#[test]
fn a_declared_directory_holding_the_output_folder_is_refused() {
    let fixture = fixture();
    let _registry = own_registry(&fixture);
    let records = fixture.root.path().join("output");
    let marker = fixture.tree.join("ran");
    let mut spec = confined_spec(
        &fixture,
        shell("wide", &format!("touch {}", marker.display())),
        &[fixture.root.path()],
    );
    spec.confinement = Some(crate::process::PartConfinement {
        records_dir: Some(records),
        probe: Some(Arc::new(|_, _| Ok(()))),
        ..crate::process::PartConfinement::default()
    });
    let outcome = block_on(run_part(spec));
    match outcome.verdict {
        Verdict::NoVerdict(NoVerdictReason::StartFailed(reason)) => {
            assert!(
                reason.contains("overlaps the checks' output folder"),
                "{reason}"
            );
        }
        other => panic!("expected a start failure, got {other:?}"),
    }
    assert!(!marker.exists(), "the check ran");
    let scratch = fixture.root.path().join("output/wide/part-1.scratch");
    assert!(
        !scratch.exists(),
        "the scratch is removed after a failed start"
    );
}

#[test]
fn a_confined_check_cannot_commit_in_its_tree() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    tree_repository(&fixture);
    let _registry = own_registry(&fixture);
    fixture.write_tree("file", "two\n");
    let spec = confined_spec(
        &fixture,
        shell(
            "commit",
            "if git -c user.name=t -c user.email=t@example.com commit -q -a -m second; \
             then exit 1; fi",
        ),
        &[],
    );
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    // The script passes only when the commit failed.
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    let count = crate::test_support::git_in(&fixture.tree, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count, "1");
}

#[test]
fn a_confined_check_cannot_commit_in_a_linked_worktree() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let main = fixture.root.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    crate::test_support::init_git_repo(&main);
    std::fs::write(main.join("file"), "one\n").unwrap();
    commit_everything(&main, "first");
    let linked = fixture.root.path().join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    crate::test_support::git_in(&main, &["worktree", "add", "-q", "-b", "task", &linked_arg]);
    std::fs::write(linked.join("file"), "two\n").unwrap();
    let mut spec = confined_spec(
        &fixture,
        shell(
            "commit",
            "if git -c user.name=t -c user.email=t@example.com commit -q -a -m second; \
             then exit 1; fi",
        ),
        &[],
    );
    spec.tree = linked.clone();
    let git_dir = main.join(".git/worktrees/linked");
    spec.confinement = Some(crate::process::PartConfinement {
        git_dirs: Ok(crate::process::host_git_dirs(&git_dir)),
        ..crate::process::PartConfinement::default()
    });
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    // The script passes only when the commit failed.
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    let count = crate::test_support::git_in(&linked, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count, "1");
}

#[test]
fn a_confined_check_can_diff_its_tree() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    tree_repository(&fixture);
    let _registry = own_registry(&fixture);
    fixture.write_tree("file", "two\n");
    // `git diff --exit-code` exits 1 when the tree differs from the index.
    let spec = confined_spec(
        &fixture,
        shell("diff", "git diff --exit-code > diff.txt; test $? -eq 1"),
        &[],
    );
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    let diff = std::fs::read_to_string(fixture.tree.join("diff.txt")).unwrap();
    assert!(diff.contains("+two"), "{diff}");
}

#[test]
fn with_the_switch_off_a_check_runs_as_before() {
    let fixture = fixture();
    let other = fixture.root.path().join("undeclared");
    std::fs::create_dir_all(&other).unwrap();
    let script = format!(
        "echo free > {other}/file && echo \"${{TMPDIR:-unset}}\" > tmpdir",
        other = other.display()
    );
    let spec = fixture.spec(shell("plain", &script));
    assert!(spec.confinement.is_none());
    let output = spec.output.clone();
    let outcome = block_on(run_part(spec));
    assert_eq!(outcome.verdict, Verdict::Pass, "{}", output_text(&output));
    assert_eq!(
        std::fs::read_to_string(other.join("file")).unwrap(),
        "free\n"
    );
    let tmpdir = std::fs::read_to_string(fixture.tree.join("tmpdir")).unwrap();
    assert!(!tmpdir.contains(".scratch"), "{tmpdir}");
    assert!(!output.parent().unwrap().join("part-1.scratch").exists());
}

#[test]
fn a_sandbox_that_cannot_be_created_is_a_start_failure_and_nothing_runs() {
    let fixture = fixture();
    let _registry = own_registry(&fixture);
    let marker = fixture.tree.join("ran");
    let mut spec = confined_spec(
        &fixture,
        shell("none", &format!("touch {}", marker.display())),
        &[],
    );
    spec.confinement = Some(crate::process::PartConfinement {
        probe: Some(Arc::new(|_, _| Err("no sandbox in this test".to_string()))),
        ..crate::process::PartConfinement::default()
    });
    let outcome = block_on(run_part(spec));
    match outcome.verdict {
        Verdict::NoVerdict(NoVerdictReason::StartFailed(reason)) => {
            assert!(reason.contains("no sandbox in this test"), "{reason}");
        }
        other => panic!("expected a start failure, got {other:?}"),
    }
    assert!(outcome.start.is_none());
    assert!(!marker.exists(), "the check ran unconfined");
}

/// Runs `spec` on its own thread; a part that never ends fails the test instead of hanging it.
fn run_guarded(spec: PartSpec) -> PartOutcome {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        sender.send(block_on(run_part(spec))).ok();
    });
    receiver
        .recv_timeout(Duration::from_secs(120))
        .expect("the part never ended (bongos blocked reading a file the part made)")
}

#[test]
fn a_confined_result_file_that_is_a_fifo_is_refused_not_read() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let decl = CheckDecl {
        result_file: Some(true),
        ..shell("fifo", "mkfifo \"$BONGOS_CHECK_RESULT\" || exit 9")
    };
    let spec = confined_spec(&fixture, decl, &[]);
    let output = spec.output.clone();
    let outcome = run_guarded(spec);
    assert_eq!(
        outcome.exit,
        Some(Exit::Code(0)),
        "{}",
        output_text(&output)
    );
    match outcome.verdict {
        Verdict::NoVerdict(NoVerdictReason::InvalidResult(reason)) => {
            assert!(reason.contains("not a regular file"), "{reason}");
        }
        other => panic!("expected an invalid result, got {other:?}"),
    }
    assert!(outcome.result.is_none());
}

#[test]
fn a_confined_scope_answer_that_is_a_fifo_is_no_answer() {
    let fixture = fixture();
    if !sandbox_available(&fixture) {
        return;
    }
    let _registry = own_registry(&fixture);
    let staging = fixture.root.path().join("output/scope");
    std::fs::create_dir_all(&staging).unwrap();
    let changed = staging.join("changed-files.txt");
    std::fs::write(&changed, "src/lib.rs\n").unwrap();
    let mut spec = confined_spec(
        &fixture,
        shell("scope", "mkfifo \"$BONGOS_SCOPE_RESULT\" || exit 9"),
        &[],
    );
    spec.output = staging.join("scope.log");
    spec.role = crate::process::ProcessRole::Scope {
        changed_files: changed,
        result: staging.join("scope-answer.json"),
    };
    let output = spec.output.clone();
    let outcome = run_guarded(spec);
    assert_eq!(
        outcome.exit,
        Some(Exit::Code(0)),
        "{}",
        output_text(&output)
    );
    assert!(outcome.scope_answer.is_none());
}

/// The start failure reason of a confined part declaring `dirs`; the sandbox is never asked.
fn declared_dirs_refusal(fixture: &Fixture, dirs: &[&Path]) -> String {
    let marker = fixture.tree.join("ran");
    let mut spec = confined_spec(
        fixture,
        shell("dirs", &format!("touch {}", marker.display())),
        dirs,
    );
    spec.confinement = Some(crate::process::PartConfinement {
        probe: Some(Arc::new(|_, _| Ok(()))),
        ..crate::process::PartConfinement::default()
    });
    let outcome = block_on(run_part(spec));
    assert!(!marker.exists(), "the check ran");
    match outcome.verdict {
        Verdict::NoVerdict(NoVerdictReason::StartFailed(reason)) => reason,
        other => panic!("expected a start failure, got {other:?}"),
    }
}

#[test]
fn a_declared_directory_reached_through_a_symlink_is_refused() {
    let fixture = fixture();
    let _registry = own_registry(&fixture);
    let sensitive = fixture.root.path().join("sensitive");
    std::fs::create_dir_all(&sensitive).unwrap();
    let lane = fixture.root.path().join("lane");
    std::fs::create_dir_all(&lane).unwrap();
    let link = lane.join("build");
    std::os::unix::fs::symlink(&sensitive, &link).unwrap();
    let reason = declared_dirs_refusal(&fixture, &[&link]);
    assert!(reason.contains("reached through a symlink"), "{reason}");
    // Through a symlinked parent too, even where nothing exists yet.
    let below = link.join("deep");
    let reason = declared_dirs_refusal(&fixture, &[&below]);
    assert!(reason.contains("reached through a symlink"), "{reason}");
    assert!(!sensitive.join("deep").exists());
}

#[test]
fn a_declared_directory_nested_in_another_writable_root_is_refused() {
    let fixture = fixture();
    let _registry = own_registry(&fixture);
    let in_tree = fixture.tree.join("target");
    let reason = declared_dirs_refusal(&fixture, &[&in_tree]);
    assert!(reason.contains("overlaps the tree"), "{reason}");
    assert!(!in_tree.exists(), "made before it was checked");

    let build = fixture.root.path().join("lane/build");
    let nested = build.join("index");
    let reason = declared_dirs_refusal(&fixture, &[&build, &nested]);
    assert!(
        reason.contains("overlaps the declared directory"),
        "{reason}"
    );
    assert!(!build.exists(), "made before it was checked");

    let scratch = fixture.root.path().join("output/dirs/part-1.scratch/cache");
    let reason = declared_dirs_refusal(&fixture, &[&scratch]);
    assert!(reason.contains("overlaps the part's scratch"), "{reason}");
}

#[test]
fn a_declared_directory_named_twice_is_fine() {
    let fixture = fixture();
    let _registry = own_registry(&fixture);
    let build = fixture.root.path().join("lane/build");
    let mut spec = confined_spec(&fixture, shell("twice", "true"), &[&build, &build]);
    spec.confinement = Some(crate::process::PartConfinement {
        probe: Some(Arc::new(|_, _| Err("stop after the checks".to_string()))),
        ..crate::process::PartConfinement::default()
    });
    let outcome = block_on(run_part(spec));
    match outcome.verdict {
        Verdict::NoVerdict(NoVerdictReason::StartFailed(reason)) => {
            assert!(reason.contains("stop after the checks"), "{reason}");
        }
        other => panic!("expected the probe's start failure, got {other:?}"),
    }
    assert!(build.is_dir());
}
