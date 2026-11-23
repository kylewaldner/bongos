//! Tests of a workflow's `instances` limit at start (refused naming the active run, or queued
//! until a run of the workflow ends), of a lane start racing at the limit, and of one active run
//! per leaf whoever started it. Runs use the scripted [`FakeRunner`]; worktrees are plain
//! directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{Kind, TaskFile, Todo, TodoState};
use bongos_workflow_model::LayerDirs;
use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::StartedBy;
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::lifecycle::{
    Lifecycle, Offer, Refusal, RefusalKind, RunStatus, RunWorktrees, StartChecks, StartContext,
    StartRequest,
};
use crate::runtime::fake::FakeRunner;
use crate::worktrees::{RunWorktree, TaskCheckout, WorktreeError, short_id};

const LIMITED: &str = r#"
schema = 1
name = "limited"
instances = 1
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const FREE: &str = r#"
schema = 1
name = "free"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

struct Checks;

impl StartChecks for Checks {
    fn agent(&self, _kind: AgentKind) -> Result<(), String> {
        Ok(())
    }

    fn interpreter(&self) -> Result<(), String> {
        Ok(())
    }

    fn mcp_servers(&self, _kind: AgentKind) -> Option<BTreeSet<String>> {
        None
    }
}

struct DirWorktrees {
    root: PathBuf,
}

impl RunWorktrees for DirWorktrees {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        let run_dir = self.root.join(format!("{}-{run_id}-{attempt}", task.slug));
        let path = run_dir.join("run");
        fs::create_dir_all(&path).unwrap();
        Ok(RunWorktree {
            run_dir,
            path,
            branch: format!("run-{run_id}-{attempt}"),
        })
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

fn leaf_id(n: usize) -> String {
    format!("leaf-{n}")
}

/// A project whose top group holds four leaves `leaf-1` to `leaf-4` (slugs `task-1` to
/// `task-4`), each with a worktree and an open todo, and a profile layer with `limited` and
/// `free`.
struct World {
    _temp: TempDir,
    root: PathBuf,
    dirs: LayerDirs,
    tree: WorkTree,
}

impl World {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("workflows");
        for (name, text) in [("limited", LIMITED), ("free", FREE)] {
            let dir = profile.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(DEFINITION_FILE), text).unwrap();
            fs::write(dir.join("work.md"), "Do the work\n").unwrap();
        }
        let project_dir = root.join("work").join("app");
        let leaves = (1..=4)
            .map(|n| {
                let worktree = root.join(format!("worktree-{n}"));
                fs::create_dir_all(&worktree).unwrap();
                let mut file = TaskFile::new(&format!("Task {n}"), Kind::PrShip);
                file.id = leaf_id(n);
                file.branch = Some(format!("task/{n}"));
                file.worktree = Some(worktree.to_string_lossy().into_owned());
                file.todos = vec![Todo {
                    id: None,
                    name: "implement".to_string(),
                    role: None,
                    workflow: Some("limited".to_string()),
                    state: TodoState::Open,
                    started: None,
                    extra: Default::default(),
                }];
                task(
                    project_dir.join("tasks").join(format!("task-{n}")),
                    file,
                    Vec::new(),
                )
            })
            .collect();
        let mut group_file = TaskFile::new("App", Kind::Other);
        group_file.id = "group-id".to_string();
        let tree = WorkTree {
            root: root.join("work"),
            projects: vec![Project {
                slug: "app".to_string(),
                file: None,
                task: task(project_dir, group_file, leaves),
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
        }
    }

    fn lifecycle(&self) -> Lifecycle<FakeRunner> {
        let settings = WorkflowsSettings {
            enabled: true,
            min_free_disk_mb: 0,
            ..WorkflowsSettings::default()
        };
        let worktrees = DirWorktrees {
            root: self.root.join("runs"),
        };
        Lifecycle::new(FakeRunner::new(), Box::new(worktrees), settings)
    }

    fn start(
        &self,
        lifecycle: &mut Lifecycle<FakeRunner>,
        request: StartRequest,
    ) -> Result<String, Refusal> {
        let context = StartContext {
            tree: &self.tree,
            active_run: None,
            checks: &Checks,
            definition_commit: None,
            definitions: None,
        };
        lifecycle.start(request, &self.dirs, &context)
    }
}

fn request(n: usize, workflow: &str) -> StartRequest {
    StartRequest {
        workflow: Some(workflow.to_string()),
        ..StartRequest::new(leaf_id(n))
    }
}

fn by(n: usize, workflow: &str, started_by: StartedBy) -> StartRequest {
    StartRequest {
        started_by,
        ..request(n, workflow)
    }
}

fn queued(n: usize, workflow: &str) -> StartRequest {
    StartRequest {
        queue: true,
        ..request(n, workflow)
    }
}

fn started_runs(lifecycle: &Lifecycle<FakeRunner>) -> Vec<String> {
    lifecycle
        .runtime()
        .runner()
        .started
        .iter()
        .map(|(run, _)| run.clone())
        .collect()
}

#[test]
fn a_limit_of_one_refuses_a_second_start_naming_the_active_run() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    // With no active run the first start runs at once.
    assert_eq!(started_runs(&lifecycle), vec![first.clone()]);

    let refusal = world
        .start(&mut lifecycle, request(2, "limited"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::InstanceLimit);
    assert_eq!(
        refusal.to_string(),
        format!(
            "Workflow 'limited' already runs 1 of its 1 allowed in this project: run {} on task \
             task-1. Queue this run to start when one ends, or cancel that run first.",
            short_id(&first)
        )
    );
    assert_eq!(refusal.offers, vec![Offer::Queue, Offer::Cancel(first)]);
    assert_eq!(lifecycle.all_runs().len(), 1);
}

#[test]
fn no_limit_lets_several_start() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let runs: Vec<String> = (1..=3)
        .map(|n| world.start(&mut lifecycle, request(n, "free")).unwrap())
        .collect();
    lifecycle.tick();
    assert_eq!(started_runs(&lifecycle), runs);
}

#[test]
fn a_queued_start_runs_when_the_active_one_ends() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    let second = world.start(&mut lifecycle, queued(2, "limited")).unwrap();
    lifecycle.tick();
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Queued));
    assert!(lifecycle.runtime().is_held_back(&second));
    assert_eq!(started_runs(&lifecycle), vec![first.clone()]);

    // A third queued start waits behind the second.
    let third = world.start(&mut lifecycle, queued(3, "limited")).unwrap();
    lifecycle.tick();
    assert_eq!(started_runs(&lifecycle), vec![first.clone()]);

    lifecycle.runtime_mut().runner_mut().release(&first);
    lifecycle.tick();
    assert_eq!(lifecycle.status(&first), Some(RunStatus::Succeeded));
    assert_eq!(started_runs(&lifecycle), vec![first, second.clone()]);
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Running));
    assert_eq!(lifecycle.status(&third), Some(RunStatus::Queued));

    lifecycle.runtime_mut().runner_mut().release(&second);
    lifecycle.tick();
    assert_eq!(lifecycle.status(&third), Some(RunStatus::Running));
}

#[test]
fn halting_and_resuming_a_queued_start_keeps_it_waiting_for_the_limit() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    let second = world.start(&mut lifecycle, queued(2, "limited")).unwrap();
    lifecycle.tick();

    lifecycle.halt(&second).unwrap();
    lifecycle.tick();
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Halted));
    lifecycle.resume(&second).unwrap();
    lifecycle.tick();
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Queued));
    assert!(lifecycle.runtime().is_held_back(&second));
    assert_eq!(started_runs(&lifecycle), vec![first.clone()]);

    lifecycle.runtime_mut().runner_mut().release(&first);
    lifecycle.tick();
    assert_eq!(started_runs(&lifecycle), vec![first, second.clone()]);
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Running));
}

#[test]
fn a_halted_start_that_never_began_does_not_count_toward_the_limit() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    let second = world.start(&mut lifecycle, queued(2, "limited")).unwrap();
    let third = world.start(&mut lifecycle, queued(3, "limited")).unwrap();
    lifecycle.tick();
    lifecycle.halt(&second).unwrap();
    lifecycle.tick();

    lifecycle.runtime_mut().runner_mut().release(&first);
    lifecycle.tick();
    assert_eq!(started_runs(&lifecycle), vec![first, third]);
    assert_eq!(lifecycle.status(&second), Some(RunStatus::Halted));
}

#[test]
fn a_relaunch_holds_back_a_queued_start_again() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    let second = world.start(&mut lifecycle, queued(2, "limited")).unwrap();
    lifecycle.tick();

    let mut fresh = world.lifecycle();
    let report = fresh.load_runs(&world.tree).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    fresh.tick();
    assert_eq!(fresh.status(&second), Some(RunStatus::Queued));
    assert!(fresh.runtime().is_held_back(&second));
    assert!(!fresh.runtime().is_held_back(&first));

    fresh.resume(&first).unwrap();
    fresh.tick();
    assert_eq!(started_runs(&fresh), vec![first.clone()]);
    fresh.runtime_mut().runner_mut().release(&first);
    fresh.tick();
    assert_eq!(started_runs(&fresh), vec![first, second.clone()]);
    assert_eq!(fresh.status(&second), Some(RunStatus::Running));
}

#[test]
fn a_relaunch_keeps_a_halted_start_that_never_began_waiting_for_the_limit() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    lifecycle.tick();
    let second = world.start(&mut lifecycle, queued(2, "limited")).unwrap();
    lifecycle.tick();
    lifecycle.halt(&second).unwrap();
    lifecycle.tick();

    let mut fresh = world.lifecycle();
    fresh.load_runs(&world.tree).unwrap();
    fresh.resume(&first).unwrap();
    fresh.tick();
    fresh.resume(&second).unwrap();
    fresh.tick();
    assert_eq!(fresh.status(&second), Some(RunStatus::Queued));
    assert_eq!(started_runs(&fresh), vec![first.clone()]);

    fresh.runtime_mut().runner_mut().release(&first);
    fresh.tick();
    assert_eq!(started_runs(&fresh), vec![first, second.clone()]);
}

#[test]
fn a_lane_start_racing_at_the_limit_is_refused() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let first = world.start(&mut lifecycle, request(1, "limited")).unwrap();
    for started_by in [StartedBy::Lane, StartedBy::Landing] {
        let request = StartRequest {
            queue: true,
            ..by(2, "limited", started_by)
        };
        let refusal = world.start(&mut lifecycle, request).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::InstanceLimit);
        assert!(
            refusal.message.contains(&short_id(&first)),
            "{}",
            refusal.message
        );
        assert!(refusal.offers.is_empty());
    }
    assert_eq!(lifecycle.all_runs(), vec![first]);
}

#[test]
fn a_hand_start_on_a_leaf_with_a_lane_run_gets_the_active_run_message() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let lane = world
        .start(&mut lifecycle, by(1, "free", StartedBy::Lane))
        .unwrap();
    lifecycle.tick();

    let refusal = world.start(&mut lifecycle, request(1, "free")).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::ActiveRun);
    assert_eq!(
        refusal.to_string(),
        "This task already has an active workflow: free, running at node work. A task runs one \
         workflow at a time. Resume it, or cancel it first."
    );
    assert_eq!(
        refusal.offers,
        vec![Offer::Resume(lane.clone()), Offer::Cancel(lane)]
    );
}
