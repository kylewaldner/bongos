//! Tests of how a run meets its task: what reaches a node (never a human-only artifact), the
//! todo started and completed by `workflow:<name>`, a moved todo, history on failure and
//! cancellation, missing inputs and overrides, the final merge gate and merge = ship. A child of
//! the lifecycle tests, so it shares their world, definitions and scripted runner; task writes go
//! to an in-memory store.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bongos_work::WriteError;
use bongos_work_model::commands::{Change, Context, Write};
use bongos_work_model::store::load;
use bongos_work_model::{TodoRole, session_context};
use bongos_workflow_model::inputs::resolve;
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::state_doc::{self, Paths, RunIdentity};
use bongos_workflow_model::{RunState, parse};
use gpui::TestAppContext;

use super::*;
use crate::builtin_merge::{MergeRefusal, MergeRequest};
use crate::gates;
use crate::task_link::{
    self, Completed, DEPLOY_GATE, MERGE_GATE, TODO_GATE, TODO_MOVED, TaskWrites, WriteCommand,
    gate_key,
};

/// Task writes kept in memory: each command runs on the tree, and its change replaces the files
/// it names in the tree.
pub(super) struct MemoryWrites {
    pub tree: Mutex<WorkTree>,
    pub changes: Mutex<Vec<Change>>,
    /// `merge_is_ship` for every task.
    pub merge_is_ship: AtomicBool,
}

impl MemoryWrites {
    pub fn new(tree: WorkTree) -> Arc<Self> {
        Arc::new(Self {
            tree: Mutex::new(tree),
            changes: Mutex::default(),
            merge_is_ship: AtomicBool::new(true),
        })
    }

    pub fn leaf(&self) -> Task {
        self.tree.lock().unwrap().task_by_id(LEAF).unwrap().clone()
    }

    pub fn todo_states(&self) -> Vec<(String, TodoState)> {
        self.leaf()
            .file
            .unwrap()
            .todos
            .iter()
            .map(|todo| (todo.name.clone(), todo.state))
            .collect()
    }

    /// The leaf's history as `event: detail` by `by`.
    pub fn history(&self) -> Vec<(String, String)> {
        self.leaf()
            .file
            .unwrap()
            .history
            .iter()
            .map(|entry| {
                let text = match &entry.detail {
                    Some(detail) => format!("{}: {detail}", entry.event),
                    None => entry.event.clone(),
                };
                (text, entry.by.clone())
            })
            .collect()
    }

    pub fn edit_leaf(&self, edit: impl FnOnce(&mut TaskFile)) {
        let mut tree = self.tree.lock().unwrap();
        let leaf = task_mut(&mut tree.projects[0].task, LEAF).unwrap();
        edit(leaf.file.as_mut().unwrap());
    }
}

fn task_mut<'a>(task: &'a mut Task, id: &str) -> Option<&'a mut Task> {
    if task.id() == Some(id) {
        return Some(task);
    }
    task.children
        .iter_mut()
        .find_map(|child| task_mut(child, id))
}

fn task_by_dir<'a>(task: &'a mut Task, dir: &Path) -> Option<&'a mut Task> {
    if task.dir == dir {
        return Some(task);
    }
    task.children
        .iter_mut()
        .find_map(|child| task_by_dir(child, dir))
}

impl TaskWrites for MemoryWrites {
    fn tree(&self, _: &gpui::App) -> Option<Arc<WorkTree>> {
        Some(Arc::new(self.tree.lock().unwrap().clone()))
    }

    fn write(
        &self,
        ctx: Context,
        command: WriteCommand,
        _: &mut gpui::App,
    ) -> gpui::Task<Result<Change, WriteError>> {
        let mut tree = self.tree.lock().unwrap();
        let change = match command(&tree, &ctx) {
            Ok(change) => change,
            Err(refusal) => return gpui::Task::ready(Err(WriteError::Refused(refusal))),
        };
        for write in &change.writes {
            match write {
                Write::Task { dir, file } => {
                    let task = task_by_dir(&mut tree.projects[0].task, dir).unwrap();
                    task.file = Some(file.clone());
                }
                Write::Artifacts { dir, file } => {
                    let task = task_by_dir(&mut tree.projects[0].task, dir).unwrap();
                    task.artifacts = Some(file.clone());
                }
                other => panic!("unexpected write {other:?}"),
            }
        }
        self.changes.lock().unwrap().push(change.clone());
        gpui::Task::ready(Ok(change))
    }

    fn merge_is_ship(&self, task: &Task, _: &gpui::App) -> bool {
        task.file
            .as_ref()
            .and_then(|file| file.merge_is_ship)
            .unwrap_or_else(|| self.merge_is_ship.load(Ordering::SeqCst))
    }
}

/// The scripted runner, shared so a test can release its tries once the engine boxed it.
pub(super) struct SharedRunner(pub Arc<Mutex<FakeRunner>>);

impl Runner for SharedRunner {
    fn start(
        &mut self,
        run: &str,
        definition: &bongos_workflow_model::Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<crate::process::ProcessRecord, bongos_workflow_model::Failure> {
        self.0.lock().unwrap().start(run, definition, key, try_number)
    }

    fn poll(&mut self) -> Vec<crate::runtime::Finish> {
        self.0.lock().unwrap().poll()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        self.0.lock().unwrap().stop(tries)
    }

    fn merge_branch(
        &mut self,
        run: &str,
        join: &str,
        branch: &bongos_workflow_model::scheduler::Branch,
    ) -> Result<String, bongos_workflow_model::Failure> {
        self.0.lock().unwrap().merge_branch(run, join, branch)
    }
}

/// An engine over `world` with a scripted runner, task writes in memory and a merger that
/// records its requests.
pub(super) struct Linked {
    pub engine: WorkflowEngine,
    pub runner: Arc<Mutex<FakeRunner>>,
    pub writes: Arc<MemoryWrites>,
    pub merges: Arc<Mutex<Vec<MergeRequest>>>,
}

pub(super) fn linked(world: &World, runner: FakeRunner) -> Linked {
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let runner = Arc::new(Mutex::new(runner));
    let lifecycle = Lifecycle::new(
        Box::new(SharedRunner(runner.clone())) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    let writes = MemoryWrites::new(world.tree.clone());
    let merges: Arc<Mutex<Vec<MergeRequest>>> = Arc::default();
    let recorded = merges.clone();
    let engine = WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle)
        .with_task_writes(writes.clone())
        .with_merger(Arc::new(move |request: &MergeRequest| {
            recorded.lock().unwrap().push(request.clone());
            Ok::<String, MergeRefusal>("abc123".to_string())
        }));
    Linked {
        engine,
        runner,
        writes,
        merges,
    }
}

pub(super) fn auto_runner() -> FakeRunner {
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner
}

impl Linked {
    pub fn start(&self, request: StartRequest, cx: &mut TestAppContext) -> String {
        let tree = self.writes.tree.lock().unwrap().clone();
        let run = cx
            .update(|cx| self.engine.start_in(request, &tree, cx))
            .unwrap();
        cx.run_until_parked();
        run
    }

    /// Ticks and publishes until two passes change nothing, letting settles finish.
    pub fn drive(&self, cx: &mut TestAppContext) {
        let mut quiet = 0;
        for _ in 0..100 {
            let changed = self.engine.lifecycle().tick();
            cx.update(|cx| self.engine.publish(cx));
            cx.run_until_parked();
            quiet = if changed { 0 } else { quiet + 1 };
            if quiet >= 2 {
                return;
            }
        }
    }

    /// Lets the tries in flight finish, then drives.
    pub fn release(&self, cx: &mut TestAppContext) {
        self.runner.lock().unwrap().release_all();
        self.drive(cx);
    }

    /// The nodes of `run` that finished, in order.
    pub fn finished(&self, run: &str) -> Vec<String> {
        let lifecycle = self.engine.lifecycle();
        lifecycle
            .state(run)
            .unwrap()
            .finished
            .iter()
            .map(|finished| finished.key.node.clone())
            .collect()
    }

    pub fn status(&self, run: &str) -> RunStatus {
        self.engine.lifecycle().status(run).unwrap()
    }

    pub fn open_gates(&self, run: &str) -> Vec<(String, String)> {
        let state = self.engine.lifecycle().state(run).unwrap();
        state
            .open_gates
            .iter()
            .map(|(key, question)| (key.node.clone(), question.clone()))
            .collect()
    }

    pub fn approve(&self, run: &str, gate: &str, cx: &mut TestAppContext) {
        cx.update(|cx| {
            self.engine
                .approve(&run.to_string(), &gate_key(gate), None, cx)
        })
        .unwrap();
        self.drive(cx);
    }
}

/// The leaf's todos: design (done), implement (open, `single`), ship (open, ships).
fn three_todos(world: &mut World) {
    let mut ship = todo("ship", Some("single"), TodoState::Open);
    ship.role = Some(TodoRole::Ships);
    world.leaf_file().todos.push(ship);
}

#[gpui::test]
fn task_link_success_checks_off_exactly_one_todo(cx: &mut TestAppContext) {
    let mut world = World::new();
    three_todos(&mut world);
    let linked = linked(&world, auto_runner());
    let run = linked.start(StartRequest::new(LEAF), cx);

    // The start marks the todo started and says so, by the workflow.
    let leaf = linked.writes.leaf();
    assert!(leaf.file.as_ref().unwrap().todos[1].started.is_some());
    assert_eq!(
        linked.writes.history(),
        [
            ("todo started: implement".to_string(), "workflow:single".to_string()),
            ("run started: single".to_string(), "workflow:single".to_string()),
        ]
    );
    assert_eq!(linked.writes.changes.lock().unwrap().len(), 1, "one write");

    // `single` has no merge node: its end offers the merge as the final gate.
    linked.drive(cx);
    assert_eq!(linked.status(&run), RunStatus::WaitingOnYou);
    assert_eq!(
        linked.open_gates(&run),
        [(MERGE_GATE.to_string(), task_link::MERGE_QUESTION.to_string())]
    );
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Open);
    assert!(linked.merges.lock().unwrap().is_empty());

    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    let merges = linked.merges.lock().unwrap().clone();
    assert_eq!(merges.len(), 1);
    assert_eq!(merges[0].task_branch, TASK_BRANCH);
    assert_eq!(merges[0].run_branch, format!("{TASK_BRANCH}-run-1"));
    assert_eq!(merges[0].todo, "implement");

    // Exactly the run's todo changed, in one more write by the workflow.
    assert_eq!(
        linked.writes.todo_states(),
        [
            ("design".to_string(), TodoState::Done),
            ("implement".to_string(), TodoState::Done),
            ("ship".to_string(), TodoState::Open),
        ]
    );
    let changes = linked.writes.changes.lock().unwrap().clone();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1].entry.event, "todo done");
    assert_eq!(changes[1].entry.detail.as_deref(), Some("implement"));
    assert_eq!(changes[1].entry.by, "workflow:single");
}

#[gpui::test]
fn task_link_a_moved_todo_is_found_by_name(cx: &mut TestAppContext) {
    let mut world = World::new();
    three_todos(&mut world);
    let linked = linked(&world, auto_runner());
    let run = linked.start(StartRequest::new(LEAF), cx);
    // Kyle adds a todo in front while the run works: the run's todo is now at index 2.
    linked
        .writes
        .edit_leaf(|file| file.todos.insert(0, todo("research", None, TodoState::Open)));
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(
        linked.writes.todo_states(),
        [
            ("research".to_string(), TodoState::Open),
            ("design".to_string(), TodoState::Done),
            ("implement".to_string(), TodoState::Done),
            ("ship".to_string(), TodoState::Open),
        ]
    );
}

#[gpui::test]
fn task_link_a_todo_done_by_hand_waits_on_you(cx: &mut TestAppContext) {
    let mut world = World::new();
    three_todos(&mut world);
    let linked = linked(&world, auto_runner());
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.writes.edit_leaf(|file| file.todos[1].state = TodoState::Done);
    let before = linked.writes.leaf().file.unwrap().todos;

    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::WaitingOnYou);
    assert_eq!(
        linked.open_gates(&run),
        [(TODO_GATE.to_string(), TODO_MOVED.to_string())]
    );
    assert_eq!(
        gates::needs_you(&linked.engine.lifecycle())
            .into_iter()
            .map(|needs| needs.reason)
            .collect::<Vec<_>>(),
        [format!("gate: {TODO_MOVED}")]
    );
    assert_eq!(linked.writes.leaf().file.unwrap().todos, before);

    // Approving it ends the run without touching any todo.
    linked.approve(&run, TODO_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.leaf().file.unwrap().todos, before);
}

#[test]
fn task_link_a_refused_completion_waits_with_the_reason() {
    let state = RunState::default();
    let reason = "login cannot ship before api is Done (it is In Progress); use task: advance \
                  override";
    assert_eq!(
        task_link::completed(&state, &Err(WriteError::Refused(reason.to_string()))),
        Completed::Wait {
            key: gate_key(task_link::REFUSED_GATE),
            question: reason.to_string(),
        }
    );
    assert_eq!(
        task_link::completed(&state, &Err(WriteError::Refused(TODO_MOVED.to_string()))),
        Completed::Wait {
            key: gate_key(TODO_GATE),
            question: TODO_MOVED.to_string(),
        }
    );
}

#[gpui::test]
fn task_link_history_on_fail_and_cancel(cx: &mut TestAppContext) {
    let world = World::new();
    let mut runner = auto_runner();
    runner.fail("work", 1);
    let linked = linked(&world, runner);
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);
    assert_eq!(linked.status(&run), RunStatus::Failed);
    let history = linked.writes.history();
    assert_eq!(
        history.last().unwrap(),
        &(
            "run failed: agent exited nonzero at work".to_string(),
            "workflow:single".to_string()
        )
    );
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Open);

    cx.update(|cx| linked.engine.cancel(&run, cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(linked.status(&run), RunStatus::Cancelled);
    assert_eq!(
        linked.writes.history().last().unwrap(),
        &("run cancelled".to_string(), "workflow:single".to_string())
    );
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Open);
}

/// Needs `task.pr`.
const NEEDS_PR: &str = r#"
schema = 1
name = "needspr"
start = "work"

[inputs]
pr = { type = "text", from = "task.pr", required = true }

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[gpui::test]
fn task_link_missing_inputs_refuse_and_overrides_win(cx: &mut TestAppContext) {
    let world = World::new();
    write_definition(world.profile(), "needspr", NEEDS_PR);
    let linked = linked(&world, FakeRunner::new());
    let tree = world.tree.clone();
    let refusal = cx
        .update(|cx| linked.engine.start_in(with_workflow("needspr"), &tree, cx))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::MissingInput);
    assert!(refusal.message.contains("'pr' (from task.pr)"), "{refusal}");
    cx.run_until_parked();
    assert!(
        linked.writes.changes.lock().unwrap().is_empty(),
        "a refused start writes nothing on the task"
    );

    let mut request = with_workflow("needspr");
    request.overrides.insert(
        "pr".to_string(),
        InputValue::Text("https://example.com/pr/9".to_string()),
    );
    let run = linked.start(request, cx);
    let lifecycle = linked.engine.lifecycle();
    let input = &lifecycle.record(&run).unwrap().inputs[0];
    assert_eq!(input.source, "override");
    assert_eq!(
        input.value,
        Some(InputValue::Text("https://example.com/pr/9".to_string()))
    );
}

/// A ship workflow with a merge node and a deploy wait.
const SHIP: &str = r#"
schema = 1
name = "ship"
start = "merge"

[nodes.merge]
kind = "program"
use = "merge"
output = { result = { type = "choice", values = ["merged", "conflict"] } }
routes = [{ to = "deploy" }]

[nodes.deploy]
kind = "program"
use = "wait"
with = { condition = "deploy" }
output = { result = { type = "choice", values = ["satisfied", "waiting"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn ship_world() -> World {
    let mut world = World::new();
    write_definition(world.profile(), "ship", SHIP);
    let mut ship = todo("ship", Some("ship"), TodoState::Open);
    ship.role = Some(TodoRole::Ships);
    world.leaf_file().todos[1].state = TodoState::Done;
    world.leaf_file().todos.push(ship);
    world.leaf_file().merge_is_ship = Some(false);
    world
}

#[gpui::test]
fn task_link_merge_is_ship_off_keeps_the_ship_todo_open_until_the_deploy_wait(
    cx: &mut TestAppContext,
) {
    let world = ship_world();
    let mut runner = FakeRunner::new();
    runner.script("merge", json!({ "result": "merged" }));
    runner.script("deploy", json!({ "result": "satisfied" }));
    let linked = linked(&world, runner);
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);

    // The merge node finishes; the deploy wait is still waiting: the ship todo stays open.
    assert!(linked.finished(&run).is_empty());
    linked.release(cx);
    assert_eq!(linked.finished(&run), ["merge"]);
    assert_eq!(linked.status(&run), RunStatus::Running);
    assert_eq!(linked.writes.todo_states()[2].1, TodoState::Open);

    // The deploy is confirmed: the run succeeds with the ship todo done, and no engine gate,
    // since the definition merges and waits on deploy itself.
    linked.release(cx);
    assert_eq!(linked.finished(&run), ["merge", "deploy"]);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.todo_states()[2].1, TodoState::Done);
    assert!(linked.merges.lock().unwrap().is_empty());
}

#[gpui::test]
fn task_link_merge_is_ship_off_without_a_deploy_wait_asks_to_confirm_the_deploy(
    cx: &mut TestAppContext,
) {
    let mut world = ship_world();
    world.leaf_file().todos[2].workflow = Some("single".to_string());
    let linked = linked(&world, auto_runner());
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(linked.merges.lock().unwrap().len(), 1);
    assert_eq!(linked.status(&run), RunStatus::WaitingOnYou);
    assert_eq!(linked.open_gates(&run)[0].0, DEPLOY_GATE);
    assert_eq!(linked.writes.todo_states()[2].1, TodoState::Open);

    linked.approve(&run, DEPLOY_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.todo_states()[2].1, TodoState::Done);
    assert_eq!(linked.merges.lock().unwrap().len(), 1, "merged once");
}

/// Reads the shared artifacts and the plan; a prompt names them.
const CONTEXT: &str = r#"
schema = 1
name = "context"
start = "work"

[inputs]
goal = { type = "text", from = "task.goal" }
artifacts = { type = "path", from = "task.artifacts" }

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn task_link_a_human_only_artifact_never_reaches_a_node() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let project = bongos_work_model::layout::project_dir(&root, "app");
    let leaf_dir = bongos_work_model::layout::child_dir(&project, "login");
    let write = |path: PathBuf, text: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write(
        bongos_work_model::layout::project_file(&project),
        "schema = 1\ntitle = \"App\"\n",
    );
    write(
        project.join("task.toml"),
        "schema = 1\nid = \"group-id\"\ngoal = \"App\"\nkind = \"other\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    write(
        leaf_dir.join("task.toml"),
        "schema = 1\nid = \"leaf-id\"\ngoal = \"Add login\"\nkind = \"pr-ship\"\n\
         status = \"open\"\ncreated = 2020-01-01T10:00:00Z\n\n[[todos]]\nname = \"implement\"\n\
         state = \"open\"\n",
    );
    write(leaf_dir.join("design.md"), "the design\n");
    write(leaf_dir.join("secret.md"), "private notes\n");
    write(
        leaf_dir.join("artifacts.toml"),
        "schema = 1\n\n[[artifact]]\npath = \"design.md\"\ndescription = \"the design\"\n\
         visibility = \"shared\"\n\n[[artifact]]\npath = \"secret.md\"\n\
         description = \"hush hush\"\nvisibility = \"human-only\"\n",
    );
    let tree = load(&root);
    let task = tree.task_by_id(LEAF).expect("the leaf loads");
    let definition = parse(CONTEXT).unwrap();
    let inputs = resolve(&definition, task, &tree, Some(0), &BTreeMap::new());
    let hidden = |text: &str| !text.contains("secret.md") && !text.contains("hush hush");

    // Inputs list the shared artifact only.
    let listed: Vec<String> = inputs
        .iter()
        .filter_map(|input| input.value.as_ref().map(InputValue::to_text))
        .collect();
    assert!(listed.iter().any(|value| value.contains("design.md")));
    assert!(listed.iter().all(|value| hidden(value)));

    // A prompt rendered from them.
    let prompt =
        task_link::render_prompt("Goal {{inputs.goal}}\nRead {{inputs.artifacts}}\n", &inputs)
            .unwrap();
    assert!(prompt.contains("design.md"));
    assert!(hidden(&prompt), "{prompt}");

    // The state document of a program node.
    let todo = TodoRef {
        index: 0,
        name: "implement".to_string(),
    };
    let document = state_doc::build(
        &inputs,
        &RunState::default(),
        RunIdentity {
            id: "run".to_string(),
            workflow: "context".to_string(),
            todo: todo.clone(),
            task_id: LEAF.to_string(),
            task_dir: leaf_dir.clone(),
            branch: TASK_BRANCH.to_string(),
            run_branch: "run".to_string(),
        },
        &toml::Table::new(),
        Paths {
            worktree: root.join("wt"),
            scratch: root.join("scratch"),
        },
    )
    .to_json();
    assert!(document.contains("design.md"));
    assert!(hidden(&document), "{document}");

    // The system file an agent node gets.
    let context = session_context(&leaf_dir).unwrap();
    let system = task_link::system_text(&context, "context", &todo);
    assert!(system.contains("design.md"));
    assert!(system.contains("Goal: Add login"));
    assert!(hidden(&system), "{system}");
}
