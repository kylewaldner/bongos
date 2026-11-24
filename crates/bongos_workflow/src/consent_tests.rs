//! Tests of a person's consent to run without the OS sandbox, through the lifecycle: a start that
//! finds no sandbox asks once per run and holds its key; an allow appends the answer, grants the
//! run in memory and starts the held keys with the grant, for every later program node of the run
//! (fan-out, halt and resume included) until it ends; a stop fails them "refused: no OS sandbox";
//! unattended and heal runs refuse with no ask; an answer with nothing pending is refused; a
//! reload (even right after an answer) starts with no grant, so the keys ask again; and an allow
//! from a peer that cannot attest a person is refused. Runs use the scripted [`FakeRunner`] with
//! nodes whose start finds no sandbox; the answer's clock is fixed; worktrees are plain
//! directories in a temp dir.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{Kind, TaskFile, Todo, TodoState};
use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{EndState, StartedBy};
use bongos_workflow_model::unsandboxed::{self, AllowedBy, Permit, REFUSED_TEXT};
use bongos_workflow_model::{
    Event, FailureReason, LayerDirs, RunState, UnattendedChoice, UnattendedSource,
};
use chrono::{TimeZone as _, Utc};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::WorkflowsSettings;
use crate::consent::{ConsentScope, Grant, InMemoryRunConsents, RunConsentStore};
use crate::lifecycle::{
    BY_PERSON, Lifecycle, NOTHING_TO_ANSWER, Refusal, RefusalKind, RunStatus, RunWorktrees,
    StartChecks, StartContext, StartRequest,
};
use crate::remote::{ALLOW_NEEDS_PERSON, WorkflowPayload, WorkflowVerb, execute};
use crate::runtime::fake::{FakeRunner, NO_SANDBOX_REASON};
use crate::worktrees::{RunWorktree, TaskCheckout, WorktreeError};

/// One program node.
pub(crate) const ONE: &str = r#"
schema = 1
name = "one"
start = "lint"

[nodes.lint]
kind = "program"
script = "lint.py"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// A program node fanning out to two program nodes joined by a third.
const FAN: &str = r#"
schema = 1
name = "fan"
start = "split"

[nodes.split]
kind = "program"
script = "lint.py"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b"] }]

[nodes.a]
kind = "program"
script = "lint.py"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "program"
script = "lint.py"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "program"
script = "lint.py"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// The fixed time every answer is recorded at.
pub(crate) const AT: &str = "2026-01-02T03:04:05Z";

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

/// A project with two leaves `leaf-1` and `leaf-2`, each with a worktree and an open todo, and a
/// profile layer with `one` and `fan`.
pub(crate) struct World {
    _temp: TempDir,
    root: PathBuf,
    dirs: LayerDirs,
    pub(crate) tree: WorkTree,
}

impl World {
    pub(crate) fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("workflows");
        for (name, text) in [("one", ONE), ("fan", FAN)] {
            let dir = profile.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(DEFINITION_FILE), text).unwrap();
            fs::write(dir.join("lint.py"), "print('ok')\n").unwrap();
        }
        let project_dir = root.join("work").join("app");
        let leaves = (1..=2)
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
                    workflow: Some("one".to_string()),
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

    /// A lifecycle whose runner finds no sandbox for the `no_sandbox` nodes, with answers timed
    /// at [`AT`].
    pub(crate) fn lifecycle(&self, no_sandbox: &[&str]) -> Lifecycle<FakeRunner> {
        let settings = WorkflowsSettings {
            enabled: true,
            min_free_disk_mb: 0,
            ..WorkflowsSettings::default()
        };
        let worktrees = DirWorktrees {
            root: self.root.join("runs"),
        };
        let mut runner = FakeRunner::new();
        runner.no_sandbox = no_sandbox.iter().map(|node| node.to_string()).collect();
        let mut lifecycle = Lifecycle::new(runner, Box::new(worktrees), settings);
        lifecycle.set_wall_clock(Arc::new(|| {
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
        }));
        lifecycle
    }

    pub(crate) fn start(
        &self,
        lifecycle: &mut Lifecycle<FakeRunner>,
        request: StartRequest,
    ) -> String {
        let context = StartContext {
            tree: &self.tree,
            active_run: None,
            checks: &Checks,
            definition_commit: None,
            definitions: None,
        };
        lifecycle.start(request, &self.dirs, &context).unwrap()
    }
}

pub(crate) fn request(n: usize, workflow: &str) -> StartRequest {
    StartRequest {
        workflow: Some(workflow.to_string()),
        ..StartRequest::new(leaf_id(n))
    }
}

/// Ticks until nothing changes.
pub(crate) fn settle(lifecycle: &mut Lifecycle<FakeRunner>) {
    for _ in 0..50 {
        if !lifecycle.tick() {
            return;
        }
    }
}

/// Lets every try in flight finish, then settles.
pub(crate) fn finish_all(lifecycle: &mut Lifecycle<FakeRunner>) {
    for _ in 0..50 {
        lifecycle.runtime_mut().runner_mut().release_all();
        if !lifecycle.tick() && lifecycle.runtime().runner().in_flight().is_empty() {
            return;
        }
    }
}

pub(crate) fn events(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> Vec<Event> {
    lifecycle.runtime().events(run).unwrap().to_vec()
}

pub(crate) fn asks(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> usize {
    events(lifecycle, run)
        .iter()
        .filter(|event| matches!(event, Event::ConsentAsked { .. }))
        .count()
}

pub(crate) fn answers(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> usize {
    events(lifecycle, run)
        .iter()
        .filter(|event| matches!(event, Event::ConsentAnswered { .. }))
        .count()
}

pub(crate) fn state(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> RunState {
    lifecycle.state(run).unwrap()
}

pub(crate) fn pending_keys(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> Option<Vec<String>> {
    state(lifecycle, run).consent_pending.map(|pending| {
        pending
            .keys
            .into_iter()
            .map(|key| key.node)
            .collect::<Vec<_>>()
    })
}

/// The nodes the runner started for `run`, in order.
pub(crate) fn started(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> Vec<String> {
    lifecycle
        .runtime()
        .runner()
        .started
        .iter()
        .filter(|(started, _)| started == run)
        .map(|(_, key)| key.node.clone())
        .collect()
}

pub(crate) fn person_grant() -> Permit {
    Permit::Allowed(AllowedBy::RunConsent {
        at: AT.to_string(),
        by: BY_PERSON.to_string(),
    })
}

/// The run's finished failures, as `(node, reason, detail)`.
pub(crate) fn failures(
    lifecycle: &Lifecycle<FakeRunner>,
    run: &str,
) -> Vec<(String, FailureReason, String)> {
    state(lifecycle, run)
        .finished
        .iter()
        .filter_map(|finished| {
            let failure = finished.failure.as_ref()?;
            Some((
                finished.key.node.clone(),
                failure.reason,
                failure.detail.clone(),
            ))
        })
        .collect()
}

#[test]
fn ask_then_allow_runs_the_node_and_a_second_run_asks_again() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let first = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);

    assert_eq!(asks(&lifecycle, &first), 1);
    assert_eq!(
        pending_keys(&lifecycle, &first),
        Some(vec!["lint".to_string()])
    );
    assert_eq!(lifecycle.status(&first), Some(RunStatus::WaitingOnYou));
    assert_eq!(
        lifecycle.consent_question(&first),
        Some(unsandboxed::ask_text("lint", NO_SANDBOX_REASON))
    );
    assert!(started(&lifecycle, &first).is_empty(), "nothing ran");
    assert!(
        state(&lifecycle, &first).finished.is_empty(),
        "an ask is never a try"
    );

    lifecycle
        .answer_unsandboxed(&first, true, BY_PERSON)
        .unwrap();
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &first), vec!["lint"]);
    assert_eq!(
        lifecycle
            .runtime()
            .runner()
            .permits
            .last()
            .map(|(_, permit)| permit),
        Some(&person_grant())
    );
    assert_eq!(
        lifecycle.consent_grant(&first),
        Some(Grant {
            at: AT.to_string(),
            by: BY_PERSON.to_string(),
        })
    );
    let history = state(&lifecycle, &first).consent_history;
    assert_eq!(history.len(), 1);
    assert!(history[0].allowed);
    assert_eq!(history[0].at, AT, "timed by the lifecycle's clock");

    // A second run, while the first's grant is live, asks for itself.
    let second = world.start(&mut lifecycle, request(2, "one"));
    settle(&mut lifecycle);
    assert_eq!(asks(&lifecycle, &second), 1);
    assert!(started(&lifecycle, &second).is_empty());
    assert_eq!(lifecycle.consent_grant(&second), None);

    // The first run ends, and its grant with it.
    lifecycle.runtime_mut().runner_mut().release(&first);
    settle(&mut lifecycle);
    assert_eq!(lifecycle.status(&first), Some(RunStatus::Succeeded));
    assert_eq!(lifecycle.consent_grant(&first), None);
    assert_eq!(asks(&lifecycle, &first), 1, "the allowed run asked once");
}

#[test]
fn stop_fails_the_run_refused_no_os_sandbox() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let run = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);
    assert_eq!(asks(&lifecycle, &run), 1);

    lifecycle
        .answer_unsandboxed(&run, false, BY_PERSON)
        .unwrap();
    settle(&mut lifecycle);

    assert!(started(&lifecycle, &run).is_empty(), "nothing ran");
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    let failed = failures(&lifecycle, &run);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "lint");
    assert_eq!(failed[0].1, FailureReason::SandboxRefused);
    assert_eq!(FailureReason::SandboxRefused.label(), REFUSED_TEXT);
    // The writer appended only the answer: the failed finish is replay's own.
    assert!(
        !events(&lifecycle, &run).iter().any(|event| matches!(
            event,
            Event::NodeFinished { .. } | Event::NodeStarted { .. }
        )),
        "{:?}",
        events(&lifecycle, &run)
    );
    assert_eq!(answers(&lifecycle, &run), 1);
    assert_eq!(lifecycle.consent_grant(&run), None);
    assert_eq!(pending_keys(&lifecycle, &run), None);
    let (end, failure) = state(&lifecycle, &run).ended.unwrap();
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::SandboxRefused);
}

#[test]
fn unattended_and_heal_runs_refuse_with_no_ask() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let unattended = StartRequest {
        unattended: Some(UnattendedChoice {
            value: true,
            source: UnattendedSource::Start,
        }),
        ..request(1, "one")
    };
    let heal = StartRequest {
        started_by: StartedBy::Healer,
        ..request(2, "one")
    };
    for start in [unattended, heal] {
        let run = world.start(&mut lifecycle, start);
        settle(&mut lifecycle);
        assert_eq!(asks(&lifecycle, &run), 0, "no ask");
        assert_eq!(pending_keys(&lifecycle, &run), None);
        assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
        let failed = failures(&lifecycle, &run);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].1, FailureReason::SandboxRefused);
        assert!(failed[0].2.starts_with(REFUSED_TEXT), "{}", failed[0].2);
        assert!(started(&lifecycle, &run).is_empty());
        // Even a grant put in the store by hand does not let it run.
        lifecycle.runtime().consents().grant(
            &run,
            ConsentScope::Sandbox,
            Grant {
                at: AT.to_string(),
                by: BY_PERSON.to_string(),
            },
        );
        assert!(lifecycle.answer_unsandboxed(&run, true, BY_PERSON).is_err());
    }
    assert!(
        lifecycle
            .runtime()
            .runner()
            .permits
            .iter()
            .all(|(_, permit)| *permit == Permit::Refuse)
    );
}

#[test]
fn two_keys_share_one_ask() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["a", "b"]);
    let run = world.start(&mut lifecycle, request(1, "fan"));
    finish_all(&mut lifecycle);

    // `split` ran; both branches found no sandbox in the same step and wait on one ask.
    assert_eq!(started(&lifecycle, &run), vec!["split"]);
    assert_eq!(
        pending_keys(&lifecycle, &run),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(
        lifecycle.consent_question(&run),
        Some(unsandboxed::ask_text("a", NO_SANDBOX_REASON))
    );
    assert_eq!(state(&lifecycle, &run).held.len(), 2);

    // One answer releases both.
    lifecycle.answer_unsandboxed(&run, true, BY_PERSON).unwrap();
    settle(&mut lifecycle);
    assert_eq!(answers(&lifecycle, &run), 1);
    assert_eq!(pending_keys(&lifecycle, &run), None);
    let mut branches = started(&lifecycle, &run)[1..].to_vec();
    branches.sort();
    assert_eq!(branches, vec!["a", "b"]);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
}

#[test]
fn an_answer_with_nothing_pending_is_refused() {
    let world = World::new();
    // A sandbox works: the run never asks and writes no consent event.
    let mut lifecycle = world.lifecycle(&[]);
    let run = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);
    let refusal: Refusal = lifecycle
        .answer_unsandboxed(&run, true, BY_PERSON)
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::WrongState);
    assert_eq!(refusal.to_string(), NOTHING_TO_ANSWER);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(asks(&lifecycle, &run) + answers(&lifecycle, &run), 0);
    assert_eq!(lifecycle.consent_grant(&run), None);

    // Two answers to one ask: the second finds nothing pending and changes nothing.
    let mut lifecycle = world.lifecycle(&["lint"]);
    let run = world.start(&mut lifecycle, request(2, "one"));
    settle(&mut lifecycle);
    lifecycle.answer_unsandboxed(&run, true, BY_PERSON).unwrap();
    let second = lifecycle
        .answer_unsandboxed(&run, false, BY_PERSON)
        .unwrap_err();
    assert_eq!(second.to_string(), NOTHING_TO_ANSWER);
    assert_eq!(answers(&lifecycle, &run), 1);
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &run), vec!["lint"]);
}

#[test]
fn a_reload_from_disk_asks_again() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let run = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);
    lifecycle.answer_unsandboxed(&run, true, BY_PERSON).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &run), vec!["lint"]);

    // The app goes away with `lint` running; a new process loads the run from disk.
    drop(lifecycle);
    let mut fresh = world.lifecycle(&["lint"]);
    let report = fresh.load_runs(&world.tree).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(fresh.consent_grant(&run), None, "replay never grants");
    assert_eq!(state(&fresh, &run).consent_history.len(), 1);

    fresh.resume(&run).unwrap();
    settle(&mut fresh);
    assert!(started(&fresh, &run).is_empty(), "nothing ran unsandboxed");
    assert_eq!(asks(&fresh, &run), 2);
    assert_eq!(pending_keys(&fresh, &run), Some(vec!["lint".to_string()]));
    assert_eq!(fresh.status(&run), Some(RunStatus::WaitingOnYou));
}

#[gpui::test]
fn an_allow_from_a_peer_that_cannot_attest_is_refused(cx: &mut TestAppContext) {
    let payload = WorkflowPayload {
        run: Some("run-1".to_string()),
        ..WorkflowPayload::default()
    };
    // Refused before anything is read: no engine, no settings and no run are needed for it.
    let refused = cx.update(|cx| {
        execute(
            WorkflowVerb::AllowUnsandboxedOnce,
            payload.clone(),
            false,
            cx,
        )
    });
    assert_eq!(refused, Err(ALLOW_NEEDS_PERSON.to_string()));

    // The verbs travel by name.
    assert_eq!(
        serde_json::to_value(WorkflowVerb::AllowUnsandboxedOnce).unwrap(),
        "allow-unsandboxed-once"
    );
    assert_eq!(
        serde_json::to_value(WorkflowVerb::RefuseUnsandboxed).unwrap(),
        "refuse-unsandboxed"
    );
}

#[test]
fn a_fan_out_and_a_later_node_reuse_the_grant() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["split", "a", "b", "gather"]);
    let run = world.start(&mut lifecycle, request(1, "fan"));
    settle(&mut lifecycle);
    assert_eq!(
        pending_keys(&lifecycle, &run),
        Some(vec!["split".to_string()])
    );
    lifecycle.answer_unsandboxed(&run, true, BY_PERSON).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &run), vec!["split"]);

    // Halted and resumed in the same process, the grant still holds.
    lifecycle.halt(&run).unwrap();
    lifecycle.resume(&run).unwrap();
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &run), vec!["split", "split"]);
    lifecycle.pause(&run).unwrap();
    lifecycle.resume(&run).unwrap();

    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    let mut nodes = started(&lifecycle, &run);
    nodes.sort();
    assert_eq!(nodes, vec!["a", "b", "gather", "split", "split"]);
    assert_eq!(asks(&lifecycle, &run), 1, "one ask for the whole run");
    let permits = &lifecycle.runtime().runner().permits;
    assert_eq!(permits.len(), 6, "{permits:?}");
    assert_eq!(permits[0].1, Permit::Ask);
    assert!(
        permits[1..]
            .iter()
            .all(|(_, permit)| *permit == person_grant()),
        "{permits:?}"
    );
    assert_eq!(lifecycle.consent_grant(&run), None, "forgotten at the end");
}

#[test]
fn an_answer_then_a_crash_before_resuming_releases_the_held_keys_which_ask_again() {
    for allowed in [true, false] {
        let world = World::new();
        let mut lifecycle = world.lifecycle(&["lint"]);
        let run = world.start(&mut lifecycle, request(1, "one"));
        settle(&mut lifecycle);
        // The answer is written and granted, and the process dies before the next tick.
        lifecycle
            .answer_unsandboxed(&run, allowed, BY_PERSON)
            .unwrap();
        drop(lifecycle);

        let mut fresh = world.lifecycle(&["lint"]);
        fresh.load_runs(&world.tree).unwrap();
        let loaded = state(&fresh, &run);
        assert!(loaded.held.is_empty(), "{allowed}: no key stays held");
        assert_eq!(loaded.consent_pending, None, "{allowed}");
        assert_eq!(loaded.consent_history.len(), 1);
        assert_eq!(fresh.consent_grant(&run), None);

        settle(&mut fresh);
        if allowed {
            // Ready to start, with an empty store: it asks again.
            assert_eq!(asks(&fresh, &run), 2);
            assert_eq!(pending_keys(&fresh, &run), Some(vec!["lint".to_string()]));
            assert_eq!(fresh.status(&run), Some(RunStatus::WaitingOnYou));
        } else {
            assert_eq!(asks(&fresh, &run), 1);
            assert_eq!(fresh.status(&run), Some(RunStatus::Failed));
            assert_eq!(failures(&fresh, &run)[0].1, FailureReason::SandboxRefused);
        }
        assert!(started(&fresh, &run).is_empty());
    }
}

fn grant_at(by: &str) -> Grant {
    Grant {
        at: AT.to_string(),
        by: by.to_string(),
    }
}

#[test]
fn two_scopes_of_one_run_are_independent() {
    let store = InMemoryRunConsents::default();
    let rule = ConsentScope::Rule("x".to_string());

    store.grant("run-1", ConsentScope::Sandbox, grant_at("sandbox"));
    assert_eq!(
        store.granted("run-1", &ConsentScope::Sandbox),
        Some(grant_at("sandbox"))
    );
    assert_eq!(store.granted("run-1", &rule), None);

    let store = InMemoryRunConsents::default();
    store.grant("run-1", rule.clone(), grant_at("rule"));
    assert_eq!(store.granted("run-1", &rule), Some(grant_at("rule")));
    assert_eq!(store.granted("run-1", &ConsentScope::Sandbox), None);
    assert_eq!(
        store.granted("run-1", &ConsentScope::Rule("y".to_string())),
        None
    );
}

#[test]
fn forgetting_a_run_forgets_all_its_scopes_and_keeps_other_runs() {
    let store = InMemoryRunConsents::default();
    let rule = ConsentScope::Rule("x".to_string());
    for run in ["run-1", "run-2"] {
        store.grant(run, ConsentScope::Sandbox, grant_at(run));
        store.grant(run, rule.clone(), grant_at(run));
    }

    store.forget("run-1");
    assert_eq!(store.granted("run-1", &ConsentScope::Sandbox), None);
    assert_eq!(store.granted("run-1", &rule), None);
    assert_eq!(
        store.granted("run-2", &ConsentScope::Sandbox),
        Some(grant_at("run-2"))
    );
    assert_eq!(store.granted("run-2", &rule), Some(grant_at("run-2")));
}

#[test]
fn a_heal_run_gets_no_grant() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let heal = world.start(
        &mut lifecycle,
        StartRequest {
            started_by: StartedBy::Healer,
            ..request(1, "one")
        },
    );
    let person = world.start(&mut lifecycle, request(2, "one"));
    settle(&mut lifecycle);

    // A grant put in the store by hand is never read for the heal run, only for the other.
    for run in [&heal, &person] {
        lifecycle
            .runtime()
            .consents()
            .grant(run, ConsentScope::Sandbox, grant_at(BY_PERSON));
    }
    assert_eq!(lifecycle.consent_grant(&heal), None);
    assert_eq!(lifecycle.runtime().sandbox_grant(&heal), None);
    assert_eq!(lifecycle.consent_grant(&person), Some(grant_at(BY_PERSON)));
    assert!(
        lifecycle
            .answer_unsandboxed(&heal, true, BY_PERSON)
            .is_err()
    );
    assert_eq!(answers(&lifecycle, &heal), 0);

    // A run that asked and then turns out to be a heal run: its allow is recorded, never granted.
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let run = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);
    assert_eq!(asks(&lifecycle, &run), 1);
    lifecycle
        .runtime_mut()
        .set_started_by(&run, StartedBy::Healer);
    lifecycle.answer_unsandboxed(&run, true, BY_PERSON).unwrap();
    assert_eq!(answers(&lifecycle, &run), 1);
    assert_eq!(
        lifecycle
            .runtime()
            .consents()
            .granted(&run, &ConsentScope::Sandbox),
        None,
        "nothing stored"
    );
    settle(&mut lifecycle);
    assert!(started(&lifecycle, &run).is_empty(), "nothing ran");
}
