//! Tests of the run lifecycle: every start refusal in order with its text, pause, halt and resume
//! with a hand edit, retry (one of three branches, several failed, fresh with its refusal),
//! restart as a new attempt, cancel, revise and approve with a field edit, the resume command,
//! and the `workflows` live-modes source refusing a second run through `can_start_run`. Runs use
//! the scripted [`FakeRunner`]; no agent runs. Worktrees are real git worktrees of a temporary
//! repository where a test needs them, with git isolated from the real configuration.

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
    GitWorktrees, Lifecycle, Offer, RefusalKind, RunStatus, RunWorktrees, StartChecks,
    StartContext, StartRequest, WorkflowEvent, attempt_dir, definition_copy_dir,
};
use crate::runtime::Runner;
use crate::runtime::fake::FakeRunner;
use crate::worktrees::{Git, RunWorktree, TaskCheckout, WorktreeError};

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
            run_dir,
            path,
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
    assert_eq!(record.todo.index, 1);
    assert_eq!(record.todo.name, "implement");
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

    lifecycle.retry(&run, None, false, &world.dirs).unwrap();
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

    let refusal = lifecycle.retry(&run, None, false, &world.dirs).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::SeveralFailed);
    assert_eq!(
        refusal.to_string(),
        "Several steps failed: b[split=1]#1, c[split=2]#1. Pick the one to retry."
    );
    let c: NodeKey = "c[split=2]#1".parse().unwrap();
    lifecycle.retry(&run, Some(c), false, &world.dirs).unwrap();
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
    let refusal = lifecycle.retry(&run, None, true, &world.dirs).unwrap_err();
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
    lifecycle.retry(&run, None, true, &world.dirs).unwrap();
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
    let restarted = lifecycle.restart(&run, &world.dirs).unwrap();
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
    assert_eq!(record.todo.name, "implement");

    // No earlier result: the new attempt starts from nothing, on the latest definition.
    let state = lifecycle.state(&run).unwrap();
    assert!(state.finished.is_empty());
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Queued));
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    let refusal = lifecycle.restart(&run, &world.dirs).unwrap_err();
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
    let refusal = lifecycle.restart(&first, &world.dirs).unwrap_err();
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

    lifecycle.halt(&run).unwrap();
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Halted));
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
    assert_eq!(
        seen.borrow().as_slice(),
        [WorkflowEvent::Changed {
            run: run.clone(),
            task_id: LEAF.to_string(),
            state: RunStatus::Queued,
        }]
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

#[path = "gates_tests.rs"]
mod gates_tests;
#[path = "outcomes_tests.rs"]
mod outcomes_tests;
#[path = "picker_tests.rs"]
mod picker_tests;
#[cfg(test)]
mod run_view_tests;
#[path = "run_view_tests.rs"]
mod run_view_tests;
#[path = "task_link_tests.rs"]
mod task_link_tests;
