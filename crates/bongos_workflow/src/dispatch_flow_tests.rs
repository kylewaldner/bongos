//! Tests of the app runner over a real temporary git repository: `merge_branch` joins fan-out
//! branches into the run branch, and the shipped "implement PR" definition runs end to end on
//! the planned path (triage, plan, a fan-out of two items, the join, the program test, the gate,
//! the merge) through the real agent and program kinds, with a fake Claude CLI. Git runs with an
//! isolated home and no global config; every directory is a temp dir; the program clock is fake.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_chat::model::{Agent, Capabilities};
use bongos_chat::{AgentCommand, Health, HealthState};
use bongos_workflow_model::inputs::{Input, InputValue};
use bongos_workflow_model::record::EndState;
use bongos_workflow_model::{FailureReason, Limits, parse};
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::builtin_nodes::BuiltinNodes;
use crate::builtin_run::MergeNode;
use crate::dispatch::{AppRunner, RunnerInputs, RunnerSnapshot};
use crate::dispatch_program::ProgramNodes;
use crate::lifecycle::definition_copy_dir;
use crate::runner_claude::ClaudeCli;
use crate::runner_program::{Clock, Interpreter, SandboxProbe};
use crate::runtime::{RunBinding, RunSpec, Runner, Runtime};
use crate::worktrees::{Git, RunWorktree, TaskCheckout, create_branch, create_run};

const RUN: &str = "01890a5d-ac96-774b-bcce-b30200000042";
const TASK_BRANCH: &str = "task/two-parts";

const FAKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-fixtures/fake-claude-headless"
);

const SHIPPED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/shipped/implement-pr");

/// A Claude CLI for the "implement PR" nodes. It answers `--version`, `--help` and `mcp` through
/// the recorded fake and picks its answer by the output schema it is given. As `plan` it commits
/// the design; as an `implement_item` it commits the file of its item, read from the prompt.
const FAKE_FLOW: &str = r#"#!/bin/sh
case "$1" in
--version | --help | mcp) exec "@FAKE@" "$@" ;;
esac
schema=
want=
for arg in "$@"; do
    if [ "$want" = schema ]; then
        schema=$arg
    fi
    want=
    if [ "$arg" = --json-schema ]; then
        want=schema
    fi
done
prompt=$(cat)
commit() {
    echo "$2" > "$1"
    git add "$1" || exit 5
    git -c user.name=t -c user.email=t@example.com commit -q -m "write $1" || exit 5
}
case "$schema" in
*'"verdict"'*) out='{"verdict":"approve","pr":"https://example.com/pr/1"}' ;;
*'"design"'*)
    commit design.md "two parts"
    out='{"items":["item-alpha","item-beta"],"design":"design.md"}'
    ;;
*'"mode"'*) out='{"mode":"planned","why":"it has two parts"}' ;;
*)
    case "$prompt" in
    *item-alpha*) part=alpha ;;
    *item-beta*) part=beta ;;
    *)
        echo "no item in the prompt" >&2
        exit 4
        ;;
    esac
    commit "$part.txt" "$part"
    out='{"summary":"wrote one part"}'
    ;;
esac
echo '{"type":"system","subtype":"init","cwd":"/work/node","session_id":"00000000-0000-4000-8000-000900000001","tools":["Read","StructuredOutput"],"mcp_servers":[],"model":"claude-haiku-5-5","permissionMode":"acceptEdits","claude_code_version":"2.1.322"}'
printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"total_cost_usd":0.01,"session_id":"00000000-0000-4000-8000-000900000001","permission_denials":[],"result":"done","structured_output":%s}\n' "$out"
"#;

/// A repository whose `main` holds `files`, with the task worktree on [`TASK_BRANCH`] and a run
/// worktree made from it, all under one temp dir.
struct Repo {
    _temp: TempDir,
    root: PathBuf,
    git: Git,
    task: TaskCheckout,
    run: RunWorktree,
}

fn git_run(git: &Git, directory: &Path, args: &[&str]) -> String {
    futures::executor::block_on(git.run(directory, args)).unwrap()
}

fn repo(files: &[(&str, &str)]) -> Repo {
    let temp = TempDir::new().unwrap();
    // Git reports canonical paths; the temporary directory may sit behind a symlink.
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let git = Git::new("git")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    let repository = root.join("repo");
    fs::create_dir_all(&repository).unwrap();
    git_run(&git, &repository, &["init", "-q", "-b", "main"]);
    for (file, text) in files {
        fs::write(repository.join(file), text).unwrap();
        git_run(&git, &repository, &["add", file]);
    }
    git_run(&git, &repository, &["commit", "-q", "-m", "initial"]);
    let task_worktree = root.join("task-worktree");
    git_run(
        &git,
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            TASK_BRANCH,
            &task_worktree.to_string_lossy(),
        ],
    );
    let task = TaskCheckout {
        slug: "two-parts".to_string(),
        worktree: Some(task_worktree),
        branch: Some(TASK_BRANCH.to_string()),
    };
    let settings = WorkflowsSettings {
        worktrees_dir: Some(root.join("runs")),
        min_free_disk_mb: 0,
        ..WorkflowsSettings::default()
    };
    let run =
        futures::executor::block_on(create_run(&git, &task, RUN, 1, &settings, &|_: &Path| {
            Some(u64::MAX)
        }))
        .unwrap();
    Repo {
        _temp: temp,
        root,
        git,
        task,
        run,
    }
}

impl Repo {
    fn binding(&self) -> RunBinding {
        let task_dir = self.root.join("task");
        let run_dir = task_dir.join("runs").join(RUN);
        RunBinding {
            workflow: "implement PR".to_string(),
            task_id: "task".to_string(),
            attempt_dir: run_dir.join("attempts/1"),
            run_dir,
            task_dir,
            worktree: self.run.clone(),
            checkout: self.task.clone(),
            inputs: vec![Input {
                name: "goal".to_string(),
                value: Some(InputValue::Text("Add the two parts".to_string())),
                source: "task.goal".to_string(),
                missing: false,
            }],
            setup: Default::default(),
        }
    }

    /// Commits `file` with `text` in `directory`.
    fn commit(&self, directory: &Path, file: &str, text: &str) {
        fs::write(directory.join(file), text).unwrap();
        git_run(&self.git, directory, &["add", file]);
        git_run(&self.git, directory, &["commit", "-q", "-m", file]);
    }

    fn runner(&self) -> AppRunner {
        let mut runner = AppRunner::default();
        runner.set_git(self.git.clone());
        runner.bind(RUN, self.binding());
        runner
    }
}

fn branch(fan: &str, index: u32) -> Vec<(String, u32)> {
    vec![(fan.to_string(), index)]
}

#[test]
fn merge_branch_joins_each_fan_out_branch_into_the_run_branch() {
    let repo = repo(&[("README.md", "readme\n")]);
    for (index, part) in [(0, "alpha"), (1, "beta")] {
        let worktree =
            futures::executor::block_on(create_branch(&repo.git, &repo.run, "plan", index))
                .unwrap();
        repo.commit(&worktree.path, &format!("{part}.txt"), part);
    }
    let mut runner = repo.runner();

    let first = runner
        .merge_branch(RUN, "test", &branch("plan", 0))
        .unwrap();
    let second = runner
        .merge_branch(RUN, "test", &branch("plan", 1))
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        git_run(&repo.git, &repo.run.path, &["rev-parse", "HEAD"]),
        second
    );
    assert!(repo.run.path.join("alpha.txt").is_file());
    assert!(repo.run.path.join("beta.txt").is_file());
    assert_eq!(
        git_run(
            &repo.git,
            &repo.run.path,
            &["log", "--first-parent", "-2", "--format=%s"]
        ),
        "Join plan branch 1\nJoin plan branch 0"
    );

    // A branch none of whose nodes ran in a worktree has nothing to merge.
    assert_eq!(
        runner
            .merge_branch(RUN, "test", &branch("plan", 7))
            .unwrap(),
        second
    );
    // A run with no binding cannot merge.
    let unbound = runner
        .merge_branch("another-run", "test", &branch("plan", 0))
        .unwrap_err();
    assert_eq!(unbound.reason, FailureReason::WorktreeUnavailable);
}

#[test]
fn merge_branch_of_a_snapshot_run_has_nothing_to_merge() {
    let temp = TempDir::new().unwrap();
    let mut binding = crate::dispatch_tests::binding(temp.path(), "plain");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir_all(&snapshot).unwrap();
    binding.worktree.path = snapshot.clone();
    binding.worktree.branch = String::new();
    binding.setup.snapshot = true;
    let mut runner = AppRunner::default();
    runner.bind(RUN, binding);

    // Every key ran in the snapshot, which is no repository: the join merges nothing.
    assert_eq!(
        runner.merge_branch(RUN, "test", &branch("plan", 0)),
        Ok(String::new())
    );
    assert_eq!(fs::read_dir(&snapshot).unwrap().count(), 0);
}

#[test]
fn merge_branch_fails_a_conflict_naming_the_file_and_leaves_the_run_branch() {
    let repo = repo(&[("shared.txt", "one\ntwo\nthree\n")]);
    for (index, text) in [(0, "one\nFIRST\nthree\n"), (1, "one\nSECOND\nthree\n")] {
        let worktree =
            futures::executor::block_on(create_branch(&repo.git, &repo.run, "plan", index))
                .unwrap();
        repo.commit(&worktree.path, "shared.txt", text);
    }
    let mut runner = repo.runner();

    let merged = runner
        .merge_branch(RUN, "test", &branch("plan", 0))
        .unwrap();
    let conflict = runner
        .merge_branch(RUN, "test", &branch("plan", 1))
        .unwrap_err();
    assert_eq!(conflict.reason, FailureReason::MergeConflict);
    assert!(
        conflict.detail.contains("shared.txt"),
        "{}",
        conflict.detail
    );
    assert_eq!(
        git_run(&repo.git, &repo.run.path, &["rev-parse", "HEAD"]),
        merged
    );
    assert_eq!(
        git_run(&repo.git, &repo.run.path, &["status", "--porcelain"]),
        ""
    );
}

struct FrozenClock(Mutex<Instant>);

impl Clock for FrozenClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

// A test reads the fake's `--version` and `--help` once; blocking the thread is fine here.
#[allow(clippy::disallowed_methods)]
fn fake_cli(command: &Path, env: &BTreeMap<String, String>) -> ClaudeCli {
    let run = |arg: &str| {
        String::from_utf8(Command::new(FAKE).arg(arg).output().unwrap().stdout).unwrap()
    };
    let caps = Capabilities::from_help(&run("--version"), &run("--help"), &[]);
    let health = Health {
        agent: Agent::Claude,
        state: HealthState::Ready {
            version: caps.version().to_string(),
            login_checked: true,
        },
        fix: String::new(),
        caps: Some(caps),
        chat_caps: None,
    };
    ClaudeCli::check(
        &AgentCommand {
            command: command.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
        },
        &health,
        env,
    )
    .unwrap()
}

/// Copies the shipped "implement PR" files to the attempt's `definition/0/`, as a run start does.
fn copy_shipped(attempt_dir: &Path) {
    let copy = definition_copy_dir(attempt_dir, 0);
    fs::create_dir_all(&copy).unwrap();
    for entry in fs::read_dir(SHIPPED).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, copy.join(path.file_name().unwrap())).unwrap();
    }
}

#[test]
fn the_shipped_implement_pr_runs_its_planned_path_to_the_merge() {
    if !on_path("make") {
        eprintln!("skipped: no make on PATH for the shipped test script");
        return;
    }
    let makefile = "test:\n\ttest -f alpha.txt\n\ttest -f beta.txt\n";
    let repo = repo(&[("README.md", "readme\n"), ("Makefile", makefile)]);
    let home = repo.root.join("home");
    // The shim refuses a command looked up through a relative or empty PATH entry.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::env::split_paths(&path).filter(|directory| directory.is_absolute()),
    )
    .unwrap();
    let env = BTreeMap::from([
        ("PATH".to_string(), path.to_string_lossy().into_owned()),
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ]);
    let interpreter = match Interpreter::find(None, &env) {
        Ok(interpreter) => interpreter,
        Err(failure) => {
            eprintln!("skipped: {failure}");
            return;
        }
    };
    let claude = repo.root.join("fake-claude-flow");
    fs::write(&claude, FAKE_FLOW.replace("@FAKE@", FAKE)).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let binding = repo.binding();
    fs::create_dir_all(&binding.task_dir).unwrap();
    fs::write(
        binding.task_dir.join("task.toml"),
        "schema = 1\nid = \"task\"\ngoal = \"Add the two parts\"\nkind = \"other\"\n\
         status = \"open\"\ncreated = 2026-01-01T00:00:00Z\n",
    )
    .unwrap();
    copy_shipped(&binding.attempt_dir);
    let text = fs::read_to_string(Path::new(SHIPPED).join("workflow.toml")).unwrap();
    let definition = parse(&text).unwrap();

    let inputs = RunnerInputs::new(RunnerSnapshot {
        claude: Some(Ok(fake_cli(&claude, &env))),
        interpreter: Some(Ok(interpreter)),
        base_env: env,
        ..RunnerSnapshot::default()
    });
    let mut runner = AppRunner::new(inputs);
    runner.set_git(repo.git.clone());
    let probe: SandboxProbe = Arc::new(|_, _| Err("no sandbox (test)".to_string()));
    let clock = Arc::new(FrozenClock(Mutex::new(Instant::now())));
    runner
        .kinds_mut()
        .set_program(Box::new(ProgramNodes::with_clock_and_probe(clock, probe)));
    let mut runtime = Runtime::new(Limits::default(), runner);
    let builtins = BuiltinNodes::default();
    builtins.insert("merge", Arc::new(MergeNode::new(repo.git.clone())));
    runtime.set_builtins(builtins);
    runtime
        .add(RunSpec {
            id: RUN.to_string(),
            definition,
            attempt_dir: binding.attempt_dir.clone(),
            loop_bound: 5,
        })
        .unwrap();
    runtime.bind(RUN, binding);

    // Ticks until the run ends, approving the review gate when it opens.
    let deadline = Instant::now() + Duration::from_secs(120);
    let state = loop {
        runtime.tick();
        let state = runtime.state(RUN).unwrap();
        if state.ended.is_some() {
            break state;
        }
        if let Some(key) = state.open_gates.keys().next().cloned() {
            runtime.close_gate(RUN, key, "kyle", None, None).unwrap();
            continue;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not end: {:#?}",
            runtime.events(RUN)
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(
        state.ended,
        Some((EndState::Succeeded, None)),
        "{:#?}",
        runtime.events(RUN)
    );
    let mut finished: Vec<String> = state
        .finished
        .iter()
        .map(|finished| finished.key.to_string())
        .collect();
    finished.sort();
    assert_eq!(
        finished,
        [
            "implement_item[plan=0]#1",
            "implement_item[plan=1]#1",
            "merge#1",
            "plan#1",
            "review#1",
            "test#1",
            "triage#1",
        ]
    );
    assert_eq!(
        state
            .merged
            .iter()
            .filter(|merged| merged.join == "test")
            .count(),
        2
    );
    assert_eq!(state.closed_gates.len(), 1);
    // Both items reached the task branch through the join and the merge.
    let task_worktree = repo.task.worktree.as_deref().unwrap();
    assert_eq!(
        fs::read_to_string(task_worktree.join("alpha.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(task_worktree.join("beta.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(
        git_run(&repo.git, task_worktree, &["rev-parse", "HEAD"]),
        git_run(&repo.git, &repo.run.path, &["rev-parse", "HEAD"])
    );
}
