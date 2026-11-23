//! Tests of the run lifecycle: every start refusal in order with its text, pause, halt and resume
//! with a hand edit, a hold on a check without a verdict and its resume, retry (one of three
//! branches, several failed, fresh with its refusal), restart as a new attempt, cancel, revise
//! and approve with a field edit, the resume command, and the `workflows` live-modes source
//! refusing a second run through `can_start_run`. Runs use the scripted [`FakeRunner`]; no agent
//! runs. Worktrees are real git worktrees of a temporary repository where a test needs them, with
//! git isolated from the real configuration.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use bongos_work::{ActiveRun, LiveModesRegistry};
use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{Kind, TaskFile, Todo, TodoState};
use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::{Event, InputValue, LayerDirs, NodeKey};
use gpui::TestAppContext;
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::engine::{EngineRunner, WorkflowEngine};
use crate::lifecycle::{
    GitWorktrees, Lifecycle, Offer, RefusalKind, RunStatus, RunTree, RunWorktrees, StartChecks,
    StartContext, StartRequest, WorkflowEvent, active_runs_on_disk, attempt_dir,
    definition_copy_dir,
};
use crate::runtime::Runner;
use crate::runtime::fake::FakeRunner;
use crate::worktrees::{Git, RunWorktree, TaskCheckout, WorktreeError};
use bongos_workflow_model::record::StartedBy;
use bongos_workflow_model::{
    LaneRef, RunSubject, RunToml, RunTreeRecord, UnattendedChoice, UnattendedSource,
};

const LEAF: &str = "leaf-id";
const GROUP: &str = "group-id";
const TASK_BRANCH: &str = "task/login";

const SINGLE: &str = r#"
schema = 1
name = "single"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const TWO: &str = r#"
schema = 1
name = "two"
start = "plan"

[nodes.plan]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "work" }]

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// `two` without `plan`: a fresh retry must refuse it once `plan` completed.
const TWO_WITHOUT_PLAN: &str = r#"
schema = 1
name = "two"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const FAN: &str = r#"
schema = 1
name = "fan"
start = "split"

[nodes.split]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b", "c"] }]

[nodes.a]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.c]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "agent"
prompt = "work.md"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const REVIEWED: &str = r#"
schema = 1
name = "reviewed"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
gate = { when = "after", question = "Good?" }
output = { summary = { type = "text" }, verdict = { type = "choice", values = ["ship", "hold"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// Needs `task.pr`, Claude with the `github` MCP server, and a program interpreter.
const FULL: &str = r#"
schema = 1
name = "full"
start = "work"

[inputs]
pr = { type = "text", from = "task.pr", required = true }

[nodes.work]
kind = "agent"
prompt = "work.md"
allowed_tools = ["mcp__github__*"]
output = { summary = { type = "text" } }
routes = [{ to = "check" }]

[nodes.check]
kind = "program"
script = "check.py"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// A build checked by the shipped checks program, with no route for `no_verdict`.
const CHECKED: &str = r#"
schema = 1
name = "checked"
start = "build"

[nodes.build]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "check" }]

[nodes.check]
kind = "program"
use = "checks"
routes = [
  { when = { verdict = "pass" }, to = "done" },
  { when = { verdict = "fail" }, to = "broken" },
]

[ends.done]
kind = "success"

[ends.broken]
kind = "failure"
reason = "checks failed"
"#;

/// A checks result with `verdict`, failing `lint`, and `summary`.
fn check_result(verdict: &str, summary: &str) -> serde_json::Value {
    json!({
        "verdict": verdict,
        "failing": ["lint"],
        "scope": "changed files",
        "evidence": [],
        "summary": summary,
    })
}

/// Starts `checked` with its check giving no verdict, and runs until the run is held on it.
fn held_on_the_check(world: &World, lifecycle: &mut Lifecycle<FakeRunner>) -> String {
    write_definition(world.profile(), "checked", CHECKED);
    lifecycle
        .runtime_mut()
        .runner_mut()
        .script("check", check_result("no_verdict", "the runner crashed"));
    let run = world.start(lifecycle, with_workflow("checked")).unwrap();
    finish_all(lifecycle);
    run
}

const BROKEN: &str = r#"
schema = 1
name = "broken"
start = "work"

[nodes.work]
kind = "agent"
routes = [{ to = "nowhere" }]
"#;

/// The start checks, scripted.
#[derive(Default)]
struct Checks {
    agent: Option<String>,
    interpreter: Option<String>,
    mcp: Option<BTreeSet<String>>,
}

impl StartChecks for Checks {
    fn agent(&self, _kind: AgentKind) -> Result<(), String> {
        self.agent.clone().map_or(Ok(()), Err)
    }

    fn interpreter(&self) -> Result<(), String> {
        self.interpreter.clone().map_or(Ok(()), Err)
    }

    fn mcp_servers(&self, _kind: AgentKind) -> Option<BTreeSet<String>> {
        self.mcp.clone()
    }
}

/// Run worktrees as plain directories, with an error to return instead when set.
struct DirWorktrees {
    root: PathBuf,
    error: Arc<std::sync::Mutex<Option<WorktreeError>>>,
}

impl RunWorktrees for DirWorktrees {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        if let Some(error) = self.error.lock().unwrap().clone() {
            return Err(error);
        }
        if task.worktree.is_none() {
            return Err(WorktreeError::NotStarted);
        }
        let run_dir = self.root.join(format!("{}-{run_id}-{attempt}", task.slug));
        let path = run_dir.join("run");
        fs::create_dir_all(&path).unwrap();
        Ok(RunWorktree {
            git_dir: run_dir.join("git"),
            run_dir,
            path,
            branch: format!("{TASK_BRANCH}-run-{attempt}"),
        })
    }

    fn lane(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        path: &Path,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        if let Some(error) = self.error.lock().unwrap().clone() {
            return Err(error);
        }
        let run_dir = self.root.join(format!("{}-{run_id}-{attempt}", task.slug));
        fs::create_dir_all(&run_dir).unwrap();
        Ok(RunWorktree {
            git_dir: run_dir.join("git"),
            run_dir,
            path: path.to_path_buf(),
            branch: format!("{TASK_BRANCH}-run-{attempt}"),
        })
    }
}

/// A temporary work tree (a project whose top group holds one leaf), a profile layer of
/// definitions, and a lifecycle over a [`FakeRunner`].
struct World {
    _temp: TempDir,
    root: PathBuf,
    dirs: LayerDirs,
    tree: WorkTree,
    checks: Checks,
    worktree_error: Arc<std::sync::Mutex<Option<WorktreeError>>>,
}

fn todo(name: &str, workflow: Option<&str>, state: TodoState) -> Todo {
    Todo {
        id: None,
        name: name.to_string(),
        role: None,
        workflow: workflow.map(str::to_string),
        state,
        started: None,
        extra: Default::default(),
    }
}

fn task(dir: PathBuf, file: TaskFile, children: Vec<Task>) -> Task {
    Task {
        slug: dir.file_name().unwrap().to_string_lossy().into_owned(),
        dir,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        definition: None,
        pending: None,
        needs_conversion: false,
        done_hold: None,
        broken_edges: Vec::new(),
        files: BTreeMap::<PathBuf, SystemTime>::new(),
        hashes: BTreeMap::new(),
        children,
    }
}

impl World {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("workflows");
        for (name, text) in [
            ("single", SINGLE),
            ("two", TWO),
            ("fan", FAN),
            ("reviewed", REVIEWED),
            ("full", FULL),
            ("broken", BROKEN),
        ] {
            write_definition(&profile, name, text);
        }
        fs::write(profile.join("full").join("check.py"), "print('ok')\n").unwrap();
        let task_worktree = root.join("task-worktree");
        fs::create_dir_all(&task_worktree).unwrap();

        let mut leaf_file = TaskFile::new("Add login", Kind::PrShip);
        leaf_file.id = LEAF.to_string();
        leaf_file.branch = Some(TASK_BRANCH.to_string());
        leaf_file.worktree = Some(task_worktree.to_string_lossy().into_owned());
        leaf_file.todos = vec![
            todo("design", None, TodoState::Done),
            todo("implement", Some("single"), TodoState::Open),
        ];
        let project_dir = root.join("work").join("app");
        let leaf = task(
            project_dir.join("tasks").join("login"),
            leaf_file,
            Vec::new(),
        );
        let mut group_file = TaskFile::new("App", Kind::Other);
        group_file.id = GROUP.to_string();
        let group = task(project_dir, group_file, vec![leaf]);
        let tree = WorkTree {
            root: root.join("work"),
            projects: vec![Project {
                slug: "app".to_string(),
                file: None,
                task: group,
            }],
            ..WorkTree::default()
        };
        Self {
            _temp: temp,
            dirs: LayerDirs {
                profile: Some(profile),
                ..LayerDirs::default()
            },
            root,
            tree,
            checks: Checks::default(),
            worktree_error: Arc::default(),
        }
    }

    fn profile(&self) -> &Path {
        self.dirs.profile.as_deref().unwrap()
    }

    fn leaf_file(&mut self) -> &mut TaskFile {
        self.tree.projects[0].task.children[0]
            .file
            .as_mut()
            .unwrap()
    }

    fn settings(&self) -> WorkflowsSettings {
        let mut settings = WorkflowsSettings {
            enabled: true,
            min_free_disk_mb: 0,
            ..WorkflowsSettings::default()
        };
        settings.limits.retries = 0;
        settings
    }

    /// A lifecycle with plain-directory worktrees.
    fn lifecycle(&self) -> Lifecycle<FakeRunner> {
        let worktrees = DirWorktrees {
            root: self.root.join("runs"),
            error: self.worktree_error.clone(),
        };
        Lifecycle::new(FakeRunner::new(), Box::new(worktrees), self.settings())
    }

    fn start(
        &self,
        lifecycle: &mut Lifecycle<FakeRunner>,
        request: StartRequest,
    ) -> Result<String, crate::lifecycle::Refusal> {
        self.start_with(lifecycle, request, None)
    }

    fn start_with<R: Runner>(
        &self,
        lifecycle: &mut Lifecycle<R>,
        request: StartRequest,
        active_run: Option<ActiveRun>,
    ) -> Result<String, crate::lifecycle::Refusal> {
        let context = StartContext {
            tree: &self.tree,
            active_run,
            checks: &self.checks,
            definition_commit: None,
            definitions: None,
        };
        lifecycle.start(request, &self.dirs, &context)
    }
}

fn write_definition(profile: &Path, name: &str, text: &str) {
    let dir = profile.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(DEFINITION_FILE), text).unwrap();
    fs::write(dir.join("work.md"), "Do the work\n").unwrap();
}

fn with_workflow(name: &str) -> StartRequest {
    StartRequest {
        workflow: Some(name.to_string()),
        ..StartRequest::new(LEAF)
    }
}

/// Ticks until nothing changes.
fn settle<R: Runner>(lifecycle: &mut Lifecycle<R>) {
    for _ in 0..50 {
        if !lifecycle.tick() {
            return;
        }
    }
}

/// Lets every try in flight finish, then settles.
fn finish_all(lifecycle: &mut Lifecycle<FakeRunner>) {
    for _ in 0..50 {
        lifecycle.runtime_mut().runner_mut().release_all();
        if !lifecycle.tick() && lifecycle.runtime().runner().in_flight().is_empty() {
            return;
        }
    }
}

fn started_nodes(lifecycle: &Lifecycle<FakeRunner>) -> Vec<String> {
    lifecycle
        .runtime()
        .runner()
        .started
        .iter()
        .map(|(_, key)| key.node.clone())
        .collect()
}

#[test]
fn start_refusals_come_in_order_with_their_fix() {
    let mut world = World::new();
    let mut lifecycle = world.lifecycle();
    let text = |result: Result<String, crate::lifecycle::Refusal>| {
        let refusal = result.unwrap_err();
        (refusal.kind, refusal.to_string())
    };

    // A group, before anything else.
    assert_eq!(
        text(world.start(&mut lifecycle, StartRequest::new(GROUP))),
        (
            RefusalKind::Group,
            "Groups have no todos. Pick a leaf, or use `task: run`.".to_string()
        )
    );

    // Every later problem at once; each fix uncovers the next refusal, in the documented order.
    world.leaf_file().todos[1].state = TodoState::Done;
    world.leaf_file().todos[1].workflow = Some("missing".to_string());
    world.checks = Checks {
        agent: Some("Install Claude Code, then log in".to_string()),
        interpreter: Some("install Python 3.8+ or set workflows.programs.python".to_string()),
        mcp: Some(BTreeSet::new()),
    };
    *world.worktree_error.lock().unwrap() = Some(WorktreeError::LowDiskSpace {
        location: PathBuf::from("/runs"),
        free_mb: 10,
        min_mb: 2048,
    });
    world.leaf_file().worktree = None;

    assert_eq!(
        text(world.start(&mut lifecycle, StartRequest::new(LEAF))),
        (
            RefusalKind::NoOpenTodo,
            "This task has no open todo for a workflow to complete. Add one (todo: add), or \
             reopen one."
                .to_string()
        )
    );
    world.leaf_file().todos[1].state = TodoState::Open;

    let reported = ActiveRun {
        workflow: "ship".to_string(),
        status: "paused".to_string(),
        step: "review".to_string(),
        past_runs: 0,
    };
    let refusal = world
        .start_with(&mut lifecycle, StartRequest::new(LEAF), Some(reported))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::ActiveRun);
    assert_eq!(
        refusal.to_string(),
        "This task already has an active workflow: ship, paused at node review. A task runs one \
         workflow at a time. Resume it, or cancel it first."
    );

    assert_eq!(
        text(world.start(&mut lifecycle, StartRequest::new(LEAF))),
        (
            RefusalKind::UnknownWorkflow,
            "No workflow is named 'missing'. Check the name, or create it (workflow: new)."
                .to_string()
        )
    );
    world.leaf_file().todos[1].workflow = Some("broken".to_string());

    let (kind, message) = text(world.start(&mut lifecycle, StartRequest::new(LEAF)));
    assert_eq!(kind, RefusalKind::Invalid);
    assert!(message.starts_with("Workflow 'broken' has "), "{message}");
    assert!(
        message.ends_with("Fix the definition (workflow: validate lists them)."),
        "{message}"
    );
    world.leaf_file().todos[1].workflow = Some("full".to_string());

    assert_eq!(
        text(world.start(&mut lifecycle, StartRequest::new(LEAF))),
        (
            RefusalKind::MissingInput,
            "Workflow 'full' needs the input 'pr' (from task.pr), which has no value. Fill it in \
             on the task, or give it when starting."
                .to_string()
        )
    );
    let mut request = StartRequest::new(LEAF);
    request.overrides.insert(
        "pr".to_string(),
        InputValue::Text("https://example.com/pr/7".to_string()),
    );

    assert_eq!(
        text(world.start(&mut lifecycle, request.clone())),
        (
            RefusalKind::AgentUnavailable,
            "Node 'work' needs Claude Code, which cannot run. Install Claude Code, then log in"
                .to_string()
        )
    );
    world.checks.agent = None;

    assert_eq!(
        text(world.start(&mut lifecycle, request.clone())),
        (
            RefusalKind::InterpreterMissing,
            "Node 'check' needs a program interpreter, which is missing. install Python 3.8+ or \
             set workflows.programs.python"
                .to_string()
        )
    );
    world.checks.interpreter = None;

    let (kind, message) = text(world.start(&mut lifecycle, request.clone()));
    assert_eq!(kind, RefusalKind::McpServerMissing);
    assert!(
        message.starts_with(
            "Node 'work' cannot use an MCP server it allows. github: MCP server not installed \
             for claude"
        ),
        "{message}"
    );
    world.checks.mcp = Some(BTreeSet::from(["github".to_string()]));

    let (kind, message) = text(world.start(&mut lifecycle, request.clone()));
    assert_eq!(kind, RefusalKind::LowDiskSpace);
    assert!(
        message.starts_with("Not enough free space: only 10 MB free"),
        "{message}"
    );
    assert!(
        message.ends_with("Free some space, or lower workflows.min_free_disk_mb."),
        "{message}"
    );
    *world.worktree_error.lock().unwrap() = None;

    assert_eq!(
        text(world.start(&mut lifecycle, request.clone())),
        (
            RefusalKind::NoTaskWorktree,
            "This task has no worktree. Start the task first (task: start).".to_string()
        )
    );
    world.leaf_file().worktree = Some(world.root.join("task-worktree").display().to_string());

    let run = world.start(&mut lifecycle, request).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Queued));
    // Nothing was written for the refused starts: only this run has a record.
    let runs_dir = world.tree.projects[0].task.children[0].dir.join("runs");
    assert_eq!(fs::read_dir(&runs_dir).unwrap().count(), 1);
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(record.workflow, "full");
    assert_eq!(record.todo.as_ref().unwrap().index, 1);
    assert_eq!(record.todo.as_ref().unwrap().name, "implement");
    assert_eq!(record.attempts.len(), 1);
    assert!(runs_dir.join(&run).join("run.toml").is_file());
    assert!(
        definition_copy_dir(&attempt_dir(&runs_dir.join(&run), 1), 0)
            .join(DEFINITION_FILE)
            .is_file()
    );
}

#[test]
fn a_task_worktree_that_is_not_there_refuses_with_git_worktrees() {
    let mut world = World::new();
    world.leaf_file().worktree = None;
    let mut lifecycle = Lifecycle::new(
        FakeRunner::new(),
        Box::new(GitWorktrees::system()),
        world.settings(),
    );
    let refusal = world
        .start_with(&mut lifecycle, StartRequest::new(LEAF), None)
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::NoTaskWorktree);
}

#[test]
fn a_second_start_names_the_workflow_state_and_node_of_the_active_run() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let run = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started_nodes(&lifecycle), ["plan"]);

    lifecycle.pause(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Pausing));
    // The running node finishes; nothing new starts.
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Paused));
    assert_eq!(started_nodes(&lifecycle), ["plan"]);

    let refusal = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::ActiveRun);
    assert_eq!(
        refusal.to_string(),
        "This task already has an active workflow: two, paused at node plan. A task runs one \
         workflow at a time. Resume it, or cancel it first."
    );
    assert_eq!(
        refusal.offers,
        vec![Offer::Resume(run.clone()), Offer::Cancel(run.clone())]
    );

    // Resume starts what pause held back.
    lifecycle.resume(&run).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started_nodes(&lifecycle), ["plan", "work"]);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
}

#[test]
fn after_cancel_another_workflow_starts() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    assert_eq!(lifecycle.active_run(LEAF).unwrap().workflow, "two");

    lifecycle.cancel(&first).unwrap();
    assert_eq!(lifecycle.status(&first), Some(RunStatus::Cancelled));
    assert_eq!(lifecycle.runtime().runner().stopped.len(), 1);
    assert!(lifecycle.active_run(LEAF).is_none());
    let events = lifecycle.take_events();
    assert!(events.contains(&WorkflowEvent::Changed {
        run: first.clone(),
        task_id: LEAF.to_string(),
        state: RunStatus::Cancelled,
    }));

    let second = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    assert_ne!(first, second);
    let active = lifecycle.active_run(LEAF).unwrap();
    assert_eq!(active.workflow, "single");
    assert_eq!(active.past_runs, 1);
    // A cancelled run cannot be cancelled again.
    assert_eq!(
        lifecycle.cancel(&first).unwrap_err().kind,
        RefusalKind::WrongState
    );
}

#[test]
fn cancel_closes_out_a_failed_run() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    // A failed run still holds the task.
    assert_eq!(lifecycle.active_run(LEAF).unwrap().status, "failed");
    lifecycle.cancel(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Cancelled));
    assert!(lifecycle.active_run(LEAF).is_none());
}

#[test]
fn retry_after_one_of_three_branches_failed_reruns_only_that_node() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("b", 1);
    let run = world.start(&mut lifecycle, with_workflow("fan")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    assert_eq!(started_nodes(&lifecycle), ["split", "a", "b", "c"]);

    lifecycle
        .retry(&run, None, false, &world.dirs, None)
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    // Only b reran; a and c kept their results and the join ran once.
    assert_eq!(
        started_nodes(&lifecycle),
        ["split", "a", "b", "c", "b", "gather"]
    );
}

#[test]
fn retry_with_several_failed_steps_lists_them() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle
        .runtime_mut()
        .runner_mut()
        .fail("b", 1)
        .fail("c", 1);
    let run = world.start(&mut lifecycle, with_workflow("fan")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    let refusal = lifecycle
        .retry(&run, None, false, &world.dirs, None)
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::SeveralFailed);
    assert_eq!(
        refusal.to_string(),
        "Several steps failed: b[split=1]#1, c[split=2]#1. Pick the one to retry."
    );
    let c: NodeKey = "c[split=2]#1".parse().unwrap();
    lifecycle
        .retry(&run, Some(c), false, &world.dirs, None)
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    assert_eq!(started_nodes(&lifecycle).last().unwrap(), "c");
}

#[test]
fn retry_fresh_refuses_a_definition_that_drops_a_completed_node() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    write_definition(world.profile(), "two", TWO_WITHOUT_PLAN);
    let refusal = lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::FreshDefinition);
    assert_eq!(
        refusal.to_string(),
        "the new definition drops node 'plan', which this run completed; restart instead \
         (workflow: restart)"
    );
    let attempt = attempt_dir(lifecycle.run_dir(&run).unwrap(), 1);
    assert!(!definition_copy_dir(&attempt, 1).exists());
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    // A definition that keeps `plan` is copied to `definition/1/` and the run continues on it.
    write_definition(world.profile(), "two", TWO);
    fs::write(
        world.profile().join("two").join("work.md"),
        "Do it better\n",
    )
    .unwrap();
    lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap();
    assert_eq!(
        fs::read_to_string(definition_copy_dir(&attempt, 1).join("work.md")).unwrap(),
        "Do it better\n"
    );
    let events = lifecycle.runtime().events(&run).unwrap();
    assert!(events.contains(&Event::DefinitionReloaded { rev: 1 }));
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(started_nodes(&lifecycle), ["plan", "work", "work"]);
}

#[test]
fn retry_fresh_replaces_a_stray_next_revision_directory() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    // A crash after the copy and before `DefinitionReloaded` left this behind.
    let attempt = attempt_dir(lifecycle.run_dir(&run).unwrap(), 1);
    let stray = definition_copy_dir(&attempt, 1);
    fs::create_dir_all(&stray).unwrap();
    fs::write(stray.join("leftover.md"), "stale\n").unwrap();

    write_definition(world.profile(), "two", TWO);
    lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap();
    assert!(!stray.join("leftover.md").exists());
    assert!(stray.join(DEFINITION_FILE).is_file());
    let events = lifecycle.runtime().events(&run).unwrap();
    assert!(events.contains(&Event::DefinitionReloaded { rev: 1 }));
}

#[test]
fn retry_fresh_never_removes_a_referenced_revision_directory() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 2);
    let run = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    write_definition(world.profile(), "two", TWO);
    fs::write(world.profile().join("two").join("work.md"), "Second\n").unwrap();
    lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    fs::write(world.profile().join("two").join("work.md"), "Third\n").unwrap();
    lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap();
    let attempt = attempt_dir(lifecycle.run_dir(&run).unwrap(), 1);
    assert_eq!(
        fs::read_to_string(definition_copy_dir(&attempt, 1).join("work.md")).unwrap(),
        "Second\n"
    );
    assert_eq!(
        fs::read_to_string(definition_copy_dir(&attempt, 2).join("work.md")).unwrap(),
        "Third\n"
    );
}

#[test]
fn revise_reruns_the_node_with_the_comment_and_approve_routes_on() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().script(
        "work",
        json!({ "summary": "first draft", "verdict": "ship" }),
    );
    let run = world
        .start(&mut lifecycle, with_workflow("reviewed"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    let key = NodeKey::new("work", 1);
    assert_eq!(lifecycle.open_gates(&run), std::slice::from_ref(&key));

    let empty = lifecycle.revise(&run, &key, "  ".to_string()).unwrap_err();
    assert_eq!(empty.kind, RefusalKind::WrongState);
    lifecycle
        .revise(&run, &key, "Use smaller steps".to_string())
        .unwrap();
    let prompt = lifecycle.prompt_for(&run, &key, "Do the work\n");
    assert!(
        prompt.starts_with("Do the work\n\nRevision requested:\nUse smaller steps\n"),
        "{prompt}"
    );
    assert!(prompt.contains("\"summary\": \"first draft\""), "{prompt}");
    settle(&mut lifecycle);
    assert_eq!(started_nodes(&lifecycle), ["work", "work"]);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    // Not a revision any more once the gate is approved.
    lifecycle.approve(&run, &key, None).unwrap();
    assert_eq!(
        lifecycle.prompt_for(&run, &key, "Do the work"),
        "Do the work"
    );
    settle(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    // A gate that is not open refuses.
    assert_eq!(
        lifecycle.approve(&run, &key, None).unwrap_err().kind,
        RefusalKind::NoGate
    );
}

#[test]
fn approve_may_set_one_output_field_checked_against_the_schema() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle
        .runtime_mut()
        .runner_mut()
        .script("work", json!({ "summary": "done", "verdict": "hold" }));
    let run = world
        .start(&mut lifecycle, with_workflow("reviewed"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    let key = NodeKey::new("work", 1);

    let invalid = lifecycle
        .approve(&run, &key, Some(("verdict".to_string(), json!("maybe"))))
        .unwrap_err();
    assert_eq!(invalid.kind, RefusalKind::InvalidEdit);
    assert!(
        invalid
            .to_string()
            .starts_with("The new 'verdict' does not fit the output of 'work': "),
        "{invalid}"
    );
    assert!(
        invalid
            .to_string()
            .ends_with("Give a value of the field's type.")
    );
    let unknown = lifecycle
        .approve(&run, &key, Some(("color".to_string(), json!("red"))))
        .unwrap_err();
    assert_eq!(
        unknown.to_string(),
        "Node 'work' has no output field 'color'. Edit one of: summary, verdict."
    );
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));

    lifecycle
        .approve(&run, &key, Some(("verdict".to_string(), json!("ship"))))
        .unwrap();
    let state = lifecycle.state(&run).unwrap();
    assert_eq!(
        state.closed_gates.last().unwrap().edit,
        Some(json!({ "verdict": "ship" }))
    );
    settle(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
}

#[test]
fn copy_resume_command_resumes_the_last_session_in_its_worktree() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    assert_eq!(
        lifecycle.resume_command(&run, None).unwrap_err().kind,
        RefusalKind::NoSession
    );
    let worktree = lifecycle.worktree(&run).unwrap().path.clone();
    let key = NodeKey::new("work", 1);
    lifecycle
        .runtime_mut()
        .runner_mut()
        .session("work", "0b5c2f8e-session");
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(
        lifecycle.resume_command(&run, Some(&key)).unwrap(),
        format!(
            "cd '{}' && claude --resume '0b5c2f8e-session'",
            worktree.display()
        )
    );
}

/// A real repository with a task worktree on [`TASK_BRANCH`], git isolated from the real
/// configuration; the world's leaf points at it.
fn real_git(world: &mut World) -> GitWorktrees {
    let home = world.root.join("home");
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
    let repository = world.root.join("repo");
    fs::create_dir_all(&repository).unwrap();
    let run = |directory: &Path, args: &[&str]| {
        futures::executor::block_on(git.run(directory, args)).unwrap();
    };
    run(&repository, &["init", "-q", "-b", "main"]);
    fs::write(repository.join("README.md"), "readme\n").unwrap();
    run(&repository, &["add", "README.md"]);
    run(&repository, &["commit", "-q", "-m", "initial"]);
    let task_worktree = world.root.join("task-git");
    run(
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
    world.leaf_file().worktree = Some(task_worktree.to_string_lossy().into_owned());
    GitWorktrees {
        git,
        free_space: Arc::new(|_| None),
    }
}

#[test]
fn restart_makes_a_new_attempt_with_a_new_worktree_and_no_reused_result() {
    let mut world = World::new();
    let worktrees = real_git(&mut world);
    let mut lifecycle = Lifecycle::new(FakeRunner::new(), Box::new(worktrees), world.settings());
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world.start(&mut lifecycle, with_workflow("two")).unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    let first = lifecycle.worktree(&run).unwrap().clone();
    assert!(first.branch.ends_with("-1"), "{}", first.branch);

    write_definition(
        world.profile(),
        "two",
        SINGLE.replace("single", "two").as_str(),
    );
    let restarted = lifecycle.restart(&run, &world.dirs, None).unwrap();
    assert_eq!(restarted, run, "a restart keeps the one run record");
    let second = lifecycle.worktree(&run).unwrap().clone();
    assert_ne!(second.path, first.path);
    assert_ne!(second.branch, first.branch);
    assert!(second.branch.ends_with("-2"), "{}", second.branch);
    assert!(second.path.join("README.md").is_file());
    assert!(first.path.is_dir(), "the earlier attempt's worktree stays");
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(
        record
            .attempts
            .iter()
            .map(|attempt| attempt.n)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(record.todo.as_ref().unwrap().name, "implement");

    // No earlier result: the new attempt starts from nothing, on the latest definition.
    let state = lifecycle.state(&run).unwrap();
    assert!(state.finished.is_empty());
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Queued));
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    let refusal = lifecycle.restart(&run, &world.dirs, None).unwrap_err();
    assert_eq!(
        refusal.kind,
        RefusalKind::WrongState,
        "a succeeded run stays"
    );
    let first_log = attempt_dir(lifecycle.run_dir(&run).unwrap(), 1).join("events.jsonl");
    assert!(first_log.is_file());
}

#[test]
fn restart_of_a_cancelled_run_is_refused_while_another_run_holds_the_task() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    lifecycle.cancel(&first).unwrap();
    let second = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    let refusal = lifecycle.restart(&first, &world.dirs, None).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::ActiveRun);
    assert_eq!(
        refusal.offers,
        vec![Offer::Resume(second.clone()), Offer::Cancel(second)]
    );
    assert_eq!(lifecycle.status(&first), Some(RunStatus::Cancelled));
}

#[test]
fn halt_then_resume_reruns_the_node_in_the_same_worktree_with_the_hand_edit() {
    let mut world = World::new();
    let worktrees = real_git(&mut world);
    let mut lifecycle = Lifecycle::new(FakeRunner::new(), Box::new(worktrees), world.settings());
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    let key = NodeKey::new("work", 1);
    assert_eq!(
        lifecycle.runtime().in_flight(&run),
        std::slice::from_ref(&key)
    );

    assert_eq!(lifecycle.progressing_runs(), std::slice::from_ref(&run));
    lifecycle.halt(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Halted));
    assert_eq!(lifecycle.active_runs(), std::slice::from_ref(&run));
    assert!(
        lifecycle.progressing_runs().is_empty(),
        "a halted run waits for a person"
    );
    assert_eq!(lifecycle.runtime().runner().stopped, [(run.clone(), key)]);
    let halted = lifecycle.halt_worktrees(&run).unwrap();
    let worktree = lifecycle.worktree(&run).unwrap().path.clone();
    assert_eq!(halted.run, worktree);
    assert!(halted.branches.is_empty());
    assert_eq!(lifecycle.active_run(LEAF).unwrap().step, "work");

    // Kyle fixes a file by hand while the run is halted.
    fs::write(worktree.join("fix.txt"), "by hand\n").unwrap();
    lifecycle.resume(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Running));
    assert_eq!(lifecycle.worktree(&run).unwrap().path, worktree);
    assert_eq!(
        fs::read_to_string(worktree.join("fix.txt")).unwrap(),
        "by hand\n"
    );
    let tries: Vec<u32> = lifecycle
        .runtime()
        .events(&run)
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Event::NodeStarted { try_number, .. } => Some(*try_number),
            _ => None,
        })
        .collect();
    assert_eq!(tries, [1, 2]);
    // The resumed try is not a retry: with `retries = 0` it still runs and succeeds.
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(started_nodes(&lifecycle), ["work", "work"]);
}

#[test]
fn a_check_without_a_verdict_holds_the_run_until_resumed() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let run = held_on_the_check(&world, &mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    let state = lifecycle.state(&run).unwrap();
    assert_eq!(
        state.held.keys().collect::<Vec<_>>(),
        [&NodeKey::new("check", 1)]
    );
    assert_eq!(started_nodes(&lifecycle), ["build", "check"]);

    lifecycle
        .runtime_mut()
        .runner_mut()
        .script("check", check_result("pass", "all green"));
    lifecycle.resume(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Running));
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert!(lifecycle.state(&run).unwrap().held.is_empty());
    // Only the held check reran; the build kept its result.
    assert_eq!(started_nodes(&lifecycle), ["build", "check", "check"]);
}

#[gpui::test]
fn a_second_start_is_refused_through_can_start_run(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        WorkflowsSettings::register(cx);
    });
    let world = World::new();
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let lifecycle = Lifecycle::new(
        Box::new(FakeRunner::new()) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    let engine = WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle);
    let seen: Rc<RefCell<Vec<WorkflowEvent>>> = Rc::default();
    cx.update(|cx| {
        crate::sources::register(&engine, cx);
        let seen = seen.clone();
        WorkflowEngine::observe(cx, move |event, _| seen.borrow_mut().push(event.clone()));
    });

    let run = cx
        .update(|cx| engine.start_in(StartRequest::new(LEAF), &world.tree, cx))
        .unwrap();
    // The start ticks once, so its first node is already running.
    assert_eq!(
        seen.borrow().as_slice(),
        [
            WorkflowEvent::Changed {
                run: run.clone(),
                task_id: LEAF.to_string(),
                state: RunStatus::Queued,
            },
            WorkflowEvent::Changed {
                run: run.clone(),
                task_id: LEAF.to_string(),
                state: RunStatus::Running,
            }
        ]
    );
    engine.lifecycle().tick();

    // The gate `task: mode` and every other caller ask: the source reports the run.
    let start = cx.update(|cx| LiveModesRegistry::can_start_run(LEAF, cx));
    assert!(!start.allowed);
    assert_eq!(
        start.chip.as_deref(),
        Some("Run in progress: single, running, step work")
    );
    assert!(cx.update(|cx| LiveModesRegistry::can_start_run(GROUP, cx).allowed));

    let refusal = cx
        .update(|cx| engine.start_in(with_workflow("two"), &world.tree, cx))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::ActiveRun);
    assert_eq!(
        refusal.to_string(),
        "This task already has an active workflow: single, running at node work. A task runs one \
         workflow at a time. Resume it, or cancel it first."
    );

    cx.update(|cx| engine.cancel(&run, cx)).unwrap();
    assert!(cx.update(|cx| LiveModesRegistry::can_start_run(LEAF, cx).allowed));
}

#[gpui::test]
fn the_engine_reports_the_worktree_of_the_active_run(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        WorkflowsSettings::register(cx);
    });
    let world = World::new();
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let lifecycle = Lifecycle::new(
        Box::new(FakeRunner::new()) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    let engine = WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle);
    assert_eq!(engine.active_run_worktree(LEAF), None);

    let run = cx
        .update(|cx| engine.start_in(StartRequest::new(LEAF), &world.tree, cx))
        .unwrap();
    engine.lifecycle().tick();

    let path = engine.lifecycle().worktree(&run).unwrap().path.clone();
    assert!(path.exists());
    assert_eq!(engine.active_run_worktree(LEAF), Some((run.clone(), path)));
    assert_eq!(engine.active_run_worktree(GROUP), None);

    cx.update(|cx| engine.cancel(&run, cx)).unwrap();
    assert_eq!(engine.active_run_worktree(LEAF), None);
}

#[path = "completion_tests.rs"]
mod completion_tests;
#[path = "enable_tests.rs"]
mod enable_tests;
#[path = "engine_tests.rs"]
mod engine_tests;
#[path = "gates_tests.rs"]
mod gates_tests;
#[path = "outcomes_tests.rs"]
mod outcomes_tests;
#[path = "picker_tests.rs"]
mod picker_tests;
#[path = "run_view_tests.rs"]
mod run_view_tests;
#[path = "runs_panel_tests.rs"]
mod runs_panel_tests;
#[path = "task_link_tests.rs"]
mod task_link_tests;

#[test]
fn a_lifecycle_binds_each_attempt_to_its_worktree_and_unbinds_at_a_restart() {
    use crate::dispatch_tests::Recorder;
    let world = World::new();
    let recorder = Recorder::default();
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let mut lifecycle = Lifecycle::new(recorder.clone(), Box::new(worktrees), world.settings());
    let run = world
        .start_with(&mut lifecycle, StartRequest::new(LEAF), None)
        .unwrap();
    let first = lifecycle.worktree(&run).unwrap().clone();
    assert_eq!(
        recorder.lines(),
        vec![format!("bind {run} {}", first.path.display())]
    );

    lifecycle.restart(&run, &world.dirs, None).unwrap();
    let second = lifecycle.worktree(&run).unwrap().clone();
    assert_ne!(first.path, second.path);
    assert_eq!(
        recorder.lines()[1..],
        [
            format!("unbind {run}"),
            format!("bind {run} {}", second.path.display())
        ]
    );
}

/// Starts `workflow` in a lifecycle of its own, lets `drive` move it, and drops the lifecycle (a
/// quit without the `Interrupted` record). The run's files stay on disk in the world.
fn seed(
    world: &World,
    workflow: &str,
    drive: impl FnOnce(&mut Lifecycle<FakeRunner>, &str),
) -> String {
    let mut lifecycle = world.lifecycle();
    let run = world
        .start(&mut lifecycle, with_workflow(workflow))
        .unwrap();
    drive(&mut lifecycle, &run);
    run
}

fn mid_node(lifecycle: &mut Lifecycle<FakeRunner>, _run: &str) {
    settle(lifecycle);
}

fn run_dir_of(world: &World, run: &str) -> PathBuf {
    world.root.join("work/app/tasks/login/runs").join(run)
}

fn events_of(world: &World, run: &str) -> PathBuf {
    attempt_dir(&run_dir_of(world, run), 1).join("events.jsonl")
}

fn sleeper() -> crate::process::NodeProcess {
    let mut command = std::process::Command::new("sleep");
    command.arg("60");
    crate::process::NodeProcess::spawn(
        command,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        std::process::Stdio::null(),
    )
    .unwrap()
}

#[test]
fn load_runs_loads_a_gate_a_mid_node_run_and_an_ended_run() {
    let world = World::new();
    let gate = seed(&world, "reviewed", |lifecycle, _| {
        lifecycle
            .runtime_mut()
            .runner_mut()
            .script("work", json!({ "summary": "draft", "verdict": "ship" }));
        settle(lifecycle);
        finish_all(lifecycle);
    });
    let mid = seed(&world, "single", mid_node);
    let ended = seed(&world, "single", |lifecycle, _| {
        settle(lifecycle);
        finish_all(lifecycle);
    });

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded.len(), 3, "{report:?}");
    assert!(report.skipped.is_empty(), "{report:?}");

    let gate_state = lifecycle.state(&gate).unwrap();
    assert_eq!(gate_state.open_gates.len(), 1);
    assert!(gate_state.interrupted.is_empty());
    let mid_state = lifecycle.state(&mid).unwrap();
    assert_eq!(mid_state.interrupted.len(), 1);
    assert!(mid_state.running.is_empty());
    assert_eq!(lifecycle.status(&ended), Some(RunStatus::Succeeded));
    // Each run is bound to its attempt's worktree, and nothing resumed by itself.
    assert!(lifecycle.worktree(&mid).unwrap().path.ends_with("run"));
    assert_eq!(lifecycle.task_of(&mid), Some(LEAF));
    lifecycle.tick();
    assert!(lifecycle.runtime().runner().started.is_empty());
}

#[test]
fn load_runs_kills_a_leftover_process_with_a_matching_start_time() {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    let mut process = sleeper();
    let record = process.record();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::process::start_time(record.pid).is_none() {
        assert!(std::time::Instant::now() < deadline, "no start time");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let record = process.record();
    let mut log = Event::Created.to_line();
    log.push_str(
        &Event::NodeStarted {
            key: NodeKey::new("work", 1),
            try_number: 1,
            pid: record.pid,
            pid_start_time: record.start_time,
        }
        .to_line(),
    );
    fs::write(events_of(&world, &run), log).unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(
        report.orphans,
        vec![(
            run.clone(),
            vec![(
                NodeKey::new("work", 1),
                crate::process::OrphanOutcome::Killed
            )]
        )]
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !process.has_exited() {
        assert!(std::time::Instant::now() < deadline, "not killed");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(lifecycle.state(&run).unwrap().interrupted.len(), 1);
}

/// A client window must not load the runs of a project another process writes: loading marks a
/// node in flight interrupted in that process's log (and stops its process).
#[test]
fn load_runs_except_leaves_the_runs_under_a_skipped_folder_alone() {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    let before = fs::read(events_of(&world, &run)).unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle
        .load_runs_except(&world.tree, &[world.root.join("work/app")])
        .unwrap();
    assert!(report.loaded.is_empty(), "{report:?}");
    assert!(report.orphans.is_empty(), "{report:?}");
    assert!(lifecycle.state(&run).is_none());
    assert_eq!(fs::read(events_of(&world, &run)).unwrap(), before);

    let report = lifecycle
        .load_runs_except(&world.tree, &[world.root.join("elsewhere")])
        .unwrap();
    assert_eq!(report.loaded, vec![run.clone()], "{report:?}");
    assert_eq!(lifecycle.state(&run).unwrap().interrupted.len(), 1);
}

/// A client window shows the runs of a project another process writes, read from their records:
/// the Runs panel rows, the status item and the needs-you list see them, nothing drives them, no
/// command reaches them and none of their files changes; what the host writes later is seen at
/// the next read, and an unchanged run is not read again as a change.
#[test]
fn observe_runs_shows_the_runs_another_process_writes_without_touching_them() {
    let world = World::new();
    let files = world.root.join("work/app");
    let mut host = world.lifecycle();
    host.runtime_mut()
        .runner_mut()
        .script("work", json!({ "summary": "draft", "verdict": "ship" }));
    let run = world.start(&mut host, with_workflow("reviewed")).unwrap();
    settle(&mut host);
    assert_eq!(host.status(&run), Some(RunStatus::Running));
    let record = fs::read(run_dir_of(&world, &run).join("run.toml")).unwrap();
    let log = fs::read(events_of(&world, &run)).unwrap();

    let mut client = world.lifecycle();
    let report = client
        .load_runs_except(&world.tree, std::slice::from_ref(&files))
        .unwrap();
    assert!(report.loaded.is_empty(), "{report:?}");
    assert!(client.shown_runs().is_empty(), "nothing read yet");
    assert!(client.observe_runs(&world.tree, std::slice::from_ref(&files)));
    assert!(client.all_runs().is_empty(), "the client drives no run");
    assert!(client.state(&run).is_none());
    assert!(client.is_observed(&run));
    assert_eq!(client.shown_runs(), vec![run.clone()]);
    assert_eq!(client.shown_active_runs(), vec![run.clone()]);
    assert_eq!(client.shown_task_of(&run), Some(LEAF));
    assert_eq!(client.shown_record(&run).unwrap().workflow, "reviewed");
    assert!(client.shown_definition(&run).is_some());
    assert_eq!(
        client.shown_attempt_dir(&run),
        Some(attempt_dir(&run_dir_of(&world, &run), 1))
    );
    let rows = crate::runs_panel::RunRow::all(&client);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run, run);
    assert_eq!(rows[0].status, RunStatus::Running);
    assert_eq!(rows[0].nodes, vec!["work".to_string()]);
    assert_eq!(
        crate::status_item::Summary::of(&client).running,
        vec!["work".to_string()]
    );
    assert!(crate::gates::needs_you(&client).is_empty());

    // Nothing drives it here, no command reaches it, and nothing is written for it.
    client.tick();
    assert!(client.runtime().runner().started.is_empty());
    assert!(client.take_events().is_empty());
    assert!(client.cancel(&run).is_err());
    assert!(client.pause(&run).is_err());
    assert_eq!(fs::read(events_of(&world, &run)).unwrap(), log);
    assert_eq!(
        fs::read(run_dir_of(&world, &run).join("run.toml")).unwrap(),
        record
    );
    assert!(
        !client.observe_runs(&world.tree, std::slice::from_ref(&files)),
        "no file changed"
    );

    // The host finishes the step and the run waits at its gate: the next read shows it.
    finish_all(&mut host);
    assert!(client.observe_runs(&world.tree, std::slice::from_ref(&files)));
    assert_eq!(client.shown_state(&run).unwrap().open_gates.len(), 1);
    let needs = crate::gates::needs_you(&client);
    assert_eq!(needs.len(), 1, "{needs:?}");
    assert_eq!(needs[0].task_id, LEAF);
    assert_eq!(crate::status_item::Summary::of(&client).needing, 1);
    assert!(client.take_events().is_empty());

    // A cleared run is no longer shown, and neither is any once the project is not another's.
    fs::write(run_dir_of(&world, &run).join("cleared"), "").unwrap();
    assert!(client.observe_runs(&world.tree, std::slice::from_ref(&files)));
    assert!(client.shown_runs().is_empty());
    fs::remove_file(run_dir_of(&world, &run).join("cleared")).unwrap();
    assert!(client.observe_runs(&world.tree, std::slice::from_ref(&files)));
    assert!(client.observe_runs(&world.tree, &[]));
    assert!(client.shown_runs().is_empty());
    assert!(!client.is_observed(&run));
}

/// A run the client takes over (its project's writer came back) is shown once, as its own.
#[test]
fn a_run_loaded_here_is_no_longer_shown_from_its_records() {
    let world = World::new();
    let files = world.root.join("work/app");
    let run = seed(&world, "single", mid_node);
    let mut client = world.lifecycle();
    assert!(client.observe_runs(&world.tree, std::slice::from_ref(&files)));
    assert!(client.is_observed(&run));
    let report = client.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded, vec![run.clone()], "{report:?}");
    assert!(!client.is_observed(&run));
    assert_eq!(client.shown_runs(), vec![run.clone()]);
    // A later read leaves a run this process drives alone.
    client.observe_runs(&world.tree, std::slice::from_ref(&files));
    assert_eq!(client.shown_runs(), vec![run]);
}

#[test]
fn load_runs_skips_a_cleared_run_and_a_corrupt_record() {
    let world = World::new();
    let cleared = seed(&world, "single", mid_node);
    let corrupt = seed(&world, "single", mid_node);
    let kept = seed(&world, "single", mid_node);
    fs::write(run_dir_of(&world, &cleared).join("cleared"), "").unwrap();
    fs::write(run_dir_of(&world, &corrupt).join("run.toml"), "not = [toml").unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded, vec![kept]);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].0.ends_with(&corrupt));
    assert!(lifecycle.state(&cleared).is_none());
    assert!(lifecycle.state(&corrupt).is_none());
}

#[test]
fn load_runs_skips_a_record_without_an_event_log() {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    fs::remove_file(events_of(&world, &run)).unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert!(report.loaded.is_empty());
    assert!(report.skipped[0].1.contains("events.jsonl"), "{report:?}");
    // Nothing was created for it either.
    assert!(!events_of(&world, &run).exists());
}

#[test]
fn load_runs_skips_a_damaged_log_and_loads_the_rest() {
    let world = World::new();
    let damaged = seed(&world, "single", mid_node);
    let good = seed(&world, "single", mid_node);
    fs::write(events_of(&world, &damaged), "not json\n").unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded, vec![good]);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].0.ends_with(&damaged));
    assert!(lifecycle.state(&damaged).is_none());
}

#[test]
#[cfg(unix)]
fn load_runs_skips_a_log_that_cannot_be_opened_and_loads_the_rest() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    let locked = seed(&world, "single", mid_node);
    let good = seed(&world, "single", mid_node);
    let path = events_of(&world, &locked);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    // Root ignores the mode and opens it anyway; the check below only holds for others.
    let writable = fs::OpenOptions::new().append(true).open(&path).is_ok();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    if writable {
        return;
    }
    assert_eq!(report.loaded, vec![good]);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(lifecycle.state(&locked).is_none());
}

#[test]
fn load_runs_skips_a_run_id_repeated_in_another_task_directory() {
    let mut world = World::new();
    let run = seed(&world, "single", mid_node);
    let other = world.root.join("work/app/tasks/other");
    fs::create_dir_all(other.join("runs")).unwrap();
    let copy = other.join("runs").join(&run);
    fs::create_dir_all(&copy).unwrap();
    let source = run_dir_of(&world, &run);
    let mut stack = vec![source.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let target = copy.join(path.strip_prefix(&source).unwrap());
            if path.is_dir() {
                fs::create_dir_all(&target).unwrap();
                stack.push(path);
            } else {
                fs::copy(&path, &target).unwrap();
            }
        }
    }
    let mut file = TaskFile::new("Other", Kind::PrShip);
    file.id = "other".to_string();
    let other_task = task(other, file, Vec::new());
    world.tree.projects[0].task.children.push(other_task);

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded, vec![run.clone()]);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert_eq!(lifecycle.all_runs(), vec![run]);
}

#[test]
fn load_runs_takes_the_revision_from_the_log_not_from_a_stray_directory() {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    let attempt = attempt_dir(&run_dir_of(&world, &run), 1);
    let copy = attempt.join("definition/1");
    fs::create_dir_all(&copy).unwrap();
    fs::write(copy.join(DEFINITION_FILE), "broken").unwrap();

    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded, vec![run.clone()], "{report:?}");
    assert_eq!(lifecycle.state(&run).unwrap().definition_rev, None);
}

#[test]
fn load_runs_with_no_runs_directory_loads_and_writes_nothing() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert!(report.loaded.is_empty() && report.skipped.is_empty());
    assert!(!world.root.join("work").exists());
    assert!(!world.root.join("runs").exists());
}

#[test]
fn load_runs_twice_is_refused_as_a_duplicate() {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    match lifecycle.load_runs(&world.tree) {
        Err(crate::runtime::RuntimeError::Duplicate(id)) => assert_eq!(id, run),
        other => panic!("expected Duplicate, got {other:?}"),
    }
    assert_eq!(lifecycle.all_runs(), vec![run]);
}

#[test]
fn resume_after_load_reruns_only_the_interrupted_key() {
    let world = World::new();
    let run = seed(&world, "two", |lifecycle, _| {
        settle(lifecycle);
        lifecycle.runtime_mut().runner_mut().release_all();
        settle(lifecycle);
        assert_eq!(started_nodes(lifecycle), ["plan", "work"]);
    });

    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(
        lifecycle.state(&run).unwrap().interrupted.keys().next(),
        Some(&NodeKey::new("work", 1))
    );
    settle(&mut lifecycle);
    assert!(started_nodes(&lifecycle).is_empty());
    lifecycle.resume(&run).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started_nodes(&lifecycle), ["work"]);

    // The resumed run finishes, with `plan`'s result kept from before the relaunch.
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(started_nodes(&lifecycle), ["work"]);
    let state = lifecycle.state(&run).unwrap();
    assert!(state.interrupted.is_empty());
    assert!(
        state
            .finished
            .iter()
            .any(|finished| finished.key == NodeKey::new("plan", 1) && finished.succeeded())
    );
}

const NETTED: &str = r#"
schema = 1
name = "netted"
start = "fetch"

[nodes.fetch]
kind = "program"
script = "fetch.py"
permissions = { network = ["example.org"] }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn restart_and_fresh_retry_refuse_a_network_entry_when_allow_network_is_off() {
    let world = World::new();
    write_definition(world.profile(), "netted", NETTED);
    fs::write(
        world.profile().join("netted").join("fetch.py"),
        "print('ok')\n",
    )
    .unwrap();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("fetch", 1);
    let run = world
        .start(&mut lifecycle, with_workflow("netted"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    // `programs.allow_network = false` now: neither a restart nor a fresh retry may run it.
    let mut settings = world.settings();
    settings.programs.allow_network = false;
    lifecycle.set_settings(settings);
    let refusal = lifecycle.restart(&run, &world.dirs, None).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Invalid, "{refusal}");
    assert!(refusal.to_string().contains("allow_network"), "{refusal}");
    let refusal = lifecycle
        .retry(&run, None, true, &world.dirs, None)
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Invalid, "{refusal}");
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    assert_eq!(lifecycle.record(&run).unwrap().attempts.len(), 1);
}

#[test]
fn quit_stops_in_flight_work_and_records_interrupted() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    assert_eq!(started_nodes(&lifecycle), ["work"]);

    lifecycle.quit();
    let events = lifecycle.runtime().events(&run).unwrap();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Interrupted { keys } if keys == &[NodeKey::new("work", 1)]
        )),
        "{events:?}"
    );
    assert!(!lifecycle.tick(), "nothing starts after a quit");
}

fn seed_succeeded(world: &World) -> String {
    seed(world, "single", |lifecycle, _| {
        settle(lifecycle);
        finish_all(lifecycle);
    })
}

fn seed_cancelled(world: &World) -> String {
    seed(world, "single", |lifecycle, run| {
        settle(lifecycle);
        lifecycle.cancel(run).unwrap();
    })
}

fn seed_failed(world: &World) -> String {
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    run
}

fn sorted(mut runs: Vec<String>) -> Vec<String> {
    runs.sort();
    runs
}

#[test]
fn clear_finished_hides_succeeded_and_cancelled_runs_and_survives_a_reload() {
    let world = World::new();
    let succeeded = seed_succeeded(&world);
    let cancelled = seed_cancelled(&world);
    let failed = seed_failed(&world);
    let active = seed(&world, "single", mid_node);
    let mut lifecycle = world.lifecycle();
    let report = lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(report.loaded.len(), 4, "{report:?}");
    assert_eq!(lifecycle.status(&failed), Some(RunStatus::Failed));

    let outcome = lifecycle.clear_finished();
    assert_eq!(
        sorted(outcome.cleared.clone()),
        sorted(vec![succeeded.clone(), cancelled.clone()])
    );
    assert!(outcome.failed.is_empty());
    assert_eq!(
        sorted(lifecycle.all_runs()),
        sorted(vec![failed.clone(), active.clone()])
    );
    assert!(lifecycle.state(&succeeded).is_none());
    assert!(lifecycle.state(&cancelled).is_none());
    for run in [&succeeded, &cancelled] {
        assert!(run_dir_of(&world, run).join("cleared").is_file());
    }
    for run in [&failed, &active] {
        assert!(!run_dir_of(&world, run).join("cleared").exists());
    }

    // Twice is a no-op.
    assert_eq!(lifecycle.clear_finished(), Default::default());

    // A relaunch does not bring them back.
    let mut fresh = world.lifecycle();
    let report = fresh.load_runs(&world.tree).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(sorted(report.loaded), sorted(vec![failed, active]));
}

#[test]
fn clear_finished_with_nothing_to_clear_writes_nothing() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    assert_eq!(lifecycle.clear_finished(), Default::default());
    assert!(!world.root.join("work/app/tasks/login/runs").exists());

    let active = seed(&world, "single", mid_node);
    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    assert_eq!(lifecycle.clear_finished(), Default::default());
    assert!(!run_dir_of(&world, &active).join("cleared").exists());
    assert_eq!(lifecycle.all_runs(), vec![active]);
}

#[test]
fn a_failed_marker_write_leaves_the_run_listed() {
    let world = World::new();
    let blocked = seed_succeeded(&world);
    let fine = seed_cancelled(&world);
    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    // A directory where the marker goes: the write fails.
    fs::create_dir(run_dir_of(&world, &blocked).join("cleared")).unwrap();

    let outcome = lifecycle.clear_finished();
    assert_eq!(outcome.cleared, vec![fine.clone()]);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, blocked);
    assert_eq!(lifecycle.status(&blocked), Some(RunStatus::Succeeded));
    assert!(lifecycle.state(&fine).is_none());
}

#[test]
fn find_cleared_lists_the_finished_cleared_runs_with_their_worktrees() {
    let world = World::new();
    let succeeded = seed_succeeded(&world);
    let active = seed(&world, "single", mid_node);
    // Nothing is cleared yet.
    assert!(crate::lifecycle::find_cleared(&world.tree).is_empty());

    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    let worktree = lifecycle.worktree(&succeeded).unwrap().clone();
    assert_eq!(lifecycle.clear_finished().cleared, vec![succeeded.clone()]);
    // A marker on a run that is not finished never offers its worktree.
    fs::write(run_dir_of(&world, &active).join("cleared"), "").unwrap();

    let found = crate::lifecycle::find_cleared(&world.tree);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].id, succeeded);
    assert_eq!(found[0].status, RunStatus::Succeeded);
    assert_eq!(found[0].worktree, worktree);
    assert_eq!(found[0].dir, run_dir_of(&world, &succeeded));
    assert_eq!(
        found[0].finished,
        fs::metadata(events_of(&world, &succeeded))
            .unwrap()
            .modified()
            .unwrap()
    );
}

#[test]
fn the_engine_dump_names_each_run_with_its_state_and_work_in_flight() {
    let world = World::new();
    let ended = seed_succeeded(&world);
    let mut lifecycle = world.lifecycle();
    lifecycle.load_runs(&world.tree).unwrap();
    let mut settings = world.settings();
    settings.limits.run_max_cost_usd = Some(5.0);
    lifecycle.set_settings(settings);
    let running = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();

    // Started, not yet ticked: queued first.
    let text = lifecycle.dump_text();
    assert!(text.contains("runs: 2"), "{text}");
    assert!(
        text.contains(&format!(
            "run {running} task {LEAF} workflow single: queued;"
        )),
        "{text}"
    );
    assert!(text.contains("queue position 1"), "{text}");
    assert!(
        text.contains(&format!(
            "run {ended} task {LEAF} workflow single: succeeded;"
        )),
        "{text}"
    );
    assert!(text.contains("tries in flight: none"), "{text}");

    settle(&mut lifecycle);
    let text = lifecycle.dump_text();
    assert!(
        text.contains(&format!("run {running} task {LEAF} workflow single: running; in flight: work#1; not queued; cost $0.00, cap $5.00")),
        "{text}"
    );
    assert!(text.contains("agent (claude): 1"), "{text}");
}

#[test]
fn an_edited_gate_field_is_what_the_next_node_state_holds() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle
        .runtime_mut()
        .runner_mut()
        .script("work", json!({ "summary": "done", "verdict": "hold" }));
    let run = world
        .start(&mut lifecycle, with_workflow("reviewed"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    let key = NodeKey::new("work", 1);

    let fields = lifecycle.gate_fields(&run, &key);
    let named: Vec<(&str, &serde_json::Value)> = fields
        .iter()
        .map(|field| (field.name.as_str(), &field.value))
        .collect();
    assert_eq!(
        named,
        vec![("summary", &json!("done")), ("verdict", &json!("hold"))]
    );

    lifecycle
        .approve(&run, &key, Some(("verdict".to_string(), json!("ship"))))
        .unwrap();
    let state = lifecycle.state(&run).unwrap();
    let doc = bongos_workflow_model::state_doc::build(
        &[],
        &state,
        bongos_workflow_model::state_doc::RunIdentity {
            id: run.clone(),
            workflow: "reviewed".to_string(),
            todo: Some(bongos_workflow_model::record::TodoRef {
                index: 1,
                name: "implement".to_string(),
            }),
            task_id: LEAF.to_string(),
            task_dir: world.root.clone(),
            branch: TASK_BRANCH.to_string(),
            run_branch: "run".to_string(),
        },
        &toml::Table::new(),
        bongos_workflow_model::state_doc::Paths {
            worktree: world.root.clone(),
            scratch: world.root.clone(),
            trees: Default::default(),
        },
    );
    let value: serde_json::Value = serde_json::from_str(&doc.to_json()).unwrap();
    assert_eq!(
        value["results"]["work"],
        json!({ "summary": "done", "verdict": "ship" })
    );
}

#[path = "landing_tests.rs"]
mod landing_tests;

#[path = "project_run_tests.rs"]
mod project_run_tests;

/// `SINGLE` with the inputs `inputs` (lines of the `[inputs]` table), named `name`.
fn with_inputs(name: &str, inputs: &str) -> String {
    SINGLE
        .replace("\"single\"", &format!("\"{name}\""))
        .replace(
            "start = \"work\"\n",
            &format!("start = \"work\"\n\n[inputs]\n{inputs}\n"),
        )
}

fn project_request(workflow: &str) -> StartRequest {
    StartRequest {
        workflow: Some(workflow.to_string()),
        ..StartRequest::project(GROUP)
    }
}

#[test]
fn a_project_subject_start_of_a_workflow_that_reads_the_task_is_refused_naming_the_input() {
    let world = World::new();
    write_definition(
        world.profile(),
        "goaled",
        &with_inputs("goaled", r#"goal = { type = "text", from = "task.goal" }"#),
    );
    let mut lifecycle = world.lifecycle();
    let mut request = project_request("goaled");
    // An override does not make a task source usable: the workflow reads the task.
    request
        .overrides
        .insert("goal".to_string(), InputValue::Text("Tidy".to_string()));
    let refusal = world.start(&mut lifecycle, request).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::NeedsTask);
    assert_eq!(
        refusal.message,
        "input goal reads task.goal; a run with no task cannot use it"
    );
    let text = fs::read_to_string(world.profile().join("goaled").join(DEFINITION_FILE)).unwrap();
    let problems = bongos_workflow_model::project_subject_problems(
        &bongos_workflow_model::parse(&text).unwrap(),
    );
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].message, refusal.message);
    assert!(lifecycle.all_runs().is_empty(), "nothing started");
}

#[test]
fn a_project_subject_start_resolves_its_inputs_from_overrides_alone() {
    let world = World::new();
    write_definition(
        world.profile(),
        "briefed",
        &with_inputs(
            "briefed",
            r#"brief = { type = "text", from = "literal:draft" }
mode = { type = "text", from = "literal:fast" }"#,
        ),
    );
    let mut lifecycle = world.lifecycle();
    // Every input resolves from the override or its literal, with no task.
    let mut request = project_request("briefed");
    request
        .overrides
        .insert("brief".to_string(), InputValue::Text("Tidy".to_string()));
    let run = world.start(&mut lifecycle, request).unwrap();
    let inputs: Vec<(String, Option<InputValue>)> = lifecycle
        .record(&run)
        .unwrap()
        .inputs
        .iter()
        .map(|input| (input.name.clone(), input.value.clone()))
        .collect();
    assert_eq!(
        inputs,
        vec![
            (
                "brief".to_string(),
                Some(InputValue::Text("Tidy".to_string()))
            ),
            (
                "mode".to_string(),
                Some(InputValue::Text("fast".to_string()))
            ),
        ]
    );
    lifecycle.cancel(&run).unwrap();

    // An unknown project and a start with no workflow named are refused first.
    let refusal = world
        .start(&mut lifecycle, StartRequest::project("nowhere"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::UnknownProject);
    let refusal = world
        .start(&mut lifecycle, StartRequest::project(GROUP))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::UnknownWorkflow);
}

#[test]
fn a_dispatcher_is_never_started_as_a_run() {
    let world = World::new();
    let dir = world.profile().join("lanes");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(DEFINITION_FILE),
        "schema = 1\nname = \"lanes\"\ndispatcher = true\n",
    )
    .unwrap();
    let mut lifecycle = world.lifecycle();
    let refusal = world
        .start(&mut lifecycle, with_workflow("lanes"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Unsupported, "{refusal}");
    assert!(lifecycle.all_runs().is_empty());
}

/// The lanes' dispatcher starts as a project run, the one way a dispatcher runs.
#[test]
fn a_dispatcher_starts_as_a_project_run_but_not_as_a_task_run() {
    let world = World::new();
    let dir = world.profile().join("lanes");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(DEFINITION_FILE),
        "schema = 1\nname = \"lanes\"\ndispatcher = true\n",
    )
    .unwrap();
    let mut lifecycle = world.lifecycle();
    let refusal = world
        .start(&mut lifecycle, with_workflow("lanes"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Unsupported, "{refusal}");
    assert!(lifecycle.all_runs().is_empty());

    let run = world
        .start(&mut lifecycle, project_request("lanes"))
        .unwrap();
    assert_eq!(lifecycle.all_runs(), vec![run.clone()]);
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(record.workflow, "lanes");
    assert_eq!(
        record.subject,
        RunSubject::Project {
            id: GROUP.to_string()
        }
    );
}

#[test]
fn the_active_runs_on_disk_name_each_task_with_a_run_that_holds_it() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    assert!(active_runs_on_disk(&world.tree).is_empty());
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    assert_eq!(
        active_runs_on_disk(&world.tree),
        BTreeMap::from([(LEAF.to_string(), run.clone())])
    );
    lifecycle.cancel(&run).unwrap();
    assert!(active_runs_on_disk(&world.tree).is_empty());
}

fn build_lane() -> Option<LaneRef> {
    Some(LaneRef {
        id: "build-1".to_string(),
        kind: "build".to_string(),
    })
}

#[test]
fn a_task_run_in_a_lane_tree_works_there_and_keeps_its_run_directory_across_a_reload() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let tree = world.root.join("lane-build-1");
    fs::create_dir_all(&tree).unwrap();
    let request = StartRequest {
        lane: build_lane(),
        started_by: StartedBy::Lane,
        tree: RunTree::Lane {
            path: tree.clone(),
            base: None,
            start: None,
            merge_into: None,
        },
        ..with_workflow("single")
    };
    let run = world.start(&mut lifecycle, request).unwrap();
    let binding = lifecycle.binding(&run).unwrap();
    assert_eq!(binding.worktree.path, tree);
    assert_ne!(binding.worktree.run_dir, tree);
    assert!(!binding.worktree.run_dir.starts_with(&tree));
    let text = fs::read_to_string(lifecycle.run_dir(&run).unwrap().join("run.toml")).unwrap();
    let record = RunToml::parse(&text).unwrap();
    assert_eq!(
        record.tree,
        Some(RunTreeRecord::Lane {
            path: tree,
            run_dir: binding.worktree.run_dir.clone(),
        })
    );

    let mut reloaded = world.lifecycle();
    reloaded.load_runs(&world.tree).unwrap();
    let again = reloaded.binding(&run).unwrap();
    assert_eq!(again.worktree, binding.worktree);
}

#[test]
fn a_lane_tree_with_no_lane_is_a_bad_request() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let request = StartRequest {
        tree: RunTree::Lane {
            path: world.root.join("lane"),
            base: None,
            start: None,
            merge_into: None,
        },
        ..with_workflow("single")
    };
    let refusal = world.start(&mut lifecycle, request).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
    assert!(lifecycle.all_runs().is_empty());
}

#[test]
fn a_lane_tree_with_a_start_or_a_task_run_on_a_snapshot_or_named_repository_is_refused() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    for tree in [
        RunTree::Lane {
            path: world.root.join("lane"),
            base: None,
            start: Some("main".to_string()),
            merge_into: None,
        },
        RunTree::Snapshot {
            path: world.root.join("snapshot"),
        },
        RunTree::Fresh {
            repo: Some(world.root.join("repo")),
            base: None,
            merge_into: None,
        },
    ] {
        let request = StartRequest {
            tree,
            lane: build_lane(),
            ..with_workflow("single")
        };
        let refusal = world.start(&mut lifecycle, request).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::Unsupported, "{refusal}");
    }
    assert!(lifecycle.all_runs().is_empty());
}

#[test]
fn a_task_start_records_its_reserved_id_parent_lane_and_unattended_choice() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let id = bongos_work_model::new_id();
    let request = StartRequest {
        run_id: Some(id.clone()),
        parent: Some("parent-run".to_string()),
        lane: Some(LaneRef {
            id: "lane-1".to_string(),
            kind: "build".to_string(),
        }),
        unattended: Some(UnattendedChoice {
            value: true,
            source: UnattendedSource::Lane,
        }),
        started_by: StartedBy::Lane,
        ..with_workflow("single")
    };
    let run = world.start(&mut lifecycle, request.clone()).unwrap();
    assert_eq!(run, id);
    let text = fs::read_to_string(lifecycle.run_dir(&run).unwrap().join("run.toml")).unwrap();
    let record = RunToml::parse(&text).unwrap();
    assert_eq!(
        record.subject,
        RunSubject::Task {
            id: LEAF.to_string()
        }
    );
    assert_eq!(record.task_id, LEAF);
    assert_eq!(record.parent.as_deref(), Some("parent-run"));
    assert_eq!(record.lane, request.lane);
    assert_eq!(record.unattended, request.unattended);
    assert_eq!(record.started_by, StartedBy::Lane);
    assert_eq!(record.todo.as_ref().unwrap().name, "implement");

    // The reserved id is taken now, and an id that is no UUID v7 is refused.
    for bad in [id, "not-a-uuid".to_string()] {
        let request = StartRequest {
            run_id: Some(bad),
            ..with_workflow("single")
        };
        let refusal = world.start(&mut lifecycle, request).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
    }
}

#[test]
fn a_definition_commit_the_layers_were_not_read_from_is_refused() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let request = StartRequest {
        definition_commit: Some(bongos_project_repo::CommitId("a".repeat(40))),
        ..with_workflow("single")
    };
    let refusal = world.start(&mut lifecycle, request).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
    assert!(lifecycle.all_runs().is_empty());
}

// A take-back loads again only the runs its handover moved: a run another handover moved to
// another process stays there, and a run loaded already is not loaded twice.
#[test]
fn load_new_runs_loads_only_the_named_runs_not_loaded_yet() {
    let world = World::new();
    let ours = seed(&world, "single", mid_node);
    let moved_elsewhere = seed(&world, "single", mid_node);
    let mut lifecycle = world.lifecycle();
    let report = lifecycle
        .load_new_runs(&world.tree, std::slice::from_ref(&ours))
        .unwrap();
    assert_eq!(report.loaded, vec![ours.clone()]);
    assert_eq!(lifecycle.all_runs(), vec![ours.clone()]);

    let report = lifecycle
        .load_new_runs(&world.tree, std::slice::from_ref(&ours))
        .unwrap();
    assert!(report.loaded.is_empty(), "{report:?}");
    assert!(lifecycle.state(&moved_elsewhere).is_none());
}

// While a project's writer moves, no run starts or restarts under its files; elsewhere, and once
// the move ends, they do.
#[test]
fn no_run_starts_or_restarts_under_a_project_whose_writer_moves() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    finish_all(&mut lifecycle);
    let files = world.root.join("work");

    lifecycle.refuse_starts_under(&world.root.join("elsewhere"), true);
    lifecycle.refuse_starts_under(&files, true);
    let refusal = lifecycle.restart(&run, &world.dirs, None).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Held);
    let refusal = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Held);
    assert_eq!(lifecycle.all_runs(), vec![run.clone()]);

    lifecycle.refuse_starts_under(&files, false);
    let started = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    assert_ne!(started, run);
}
