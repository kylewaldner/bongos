use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState,
    RunSubject, TaskId, WaitReason,
};
use bongos_project_repo::{Location, StateApplyHooks, StateStore, StateWriter};
use bongos_project_repo_model::{DefinitionTree, RelPath};
use futures::channel::oneshot;
use gpui::{App, Task};
use tempfile::TempDir;

use crate::hooks::{BeforeJob, JobEnd, JobHook, JobHookCx};

/// A writer over a state directory inside `root`, with its own hook registry, so tests never
/// share the global one.
pub(crate) fn writer(root: &TempDir) -> Arc<StateWriter> {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    };
    Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap())
}

/// A pinned time; nothing here reads the clock.
pub(crate) const SINCE: &str = "2026-03-01T10:00:00Z";

/// The lanes state where lane `build-1` owns the top-level task `feature` and runs `run-lane` on
/// its leaf `leaf`, and lane `docs-1` is idle.
pub(crate) fn lanes_state() -> LanesState {
    let build = LaneId::new("build", 1);
    let mut running = LaneRecord::new(&build, LaneState::Running);
    running.job = Some(JobRecord {
        run_id: "run-lane".to_string(),
        subject: RunSubject::Task(TaskId::new("leaf")),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: Some("c1".to_string()),
        extension: None,
    });
    running.jobs_done = 2;
    let docs = LaneId::new("docs", 1);
    let idle = LaneRecord::new(&docs, LaneState::Idle);
    let owner = OwnerRecord {
        lane: Some(build.clone()),
        state: OwnerState::Active,
        since: SINCE.to_string(),
        by: Author::Lane(build.clone()),
        reason: None,
        prepared_commit: None,
        refreshed: false,
    };
    let done = OwnerRecord {
        lane: None,
        state: OwnerState::Done,
        since: SINCE.to_string(),
        by: Author::Person("kim".to_string()),
        reason: Some("finished".to_string()),
        prepared_commit: Some("c0".to_string()),
        refreshed: true,
    };
    LanesState {
        owners: [
            (TaskId::new("feature"), owner),
            (TaskId::new("shipped"), done),
        ]
        .into(),
        lanes: [(build, running), (docs, idle)].into(),
    }
}

/// One call a [`GatedHook`] heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookCall {
    /// `before_job` of the job with this run id.
    Before(String),
    /// `after_job` of the job with this run id.
    After(String, JobEnd),
}

/// A job hook whose `before_job` and `after_job` answer when the test says so. Ungated, each
/// answers at once (`before_job` with its `answer`).
#[derive(Default)]
pub(crate) struct GatedHook {
    /// `before_job` waits for [`GatedHook::decide`].
    pub(crate) gate_before: AtomicBool,
    /// `after_job` waits for [`GatedHook::release_after`].
    pub(crate) gate_after: AtomicBool,
    /// What an ungated `before_job` answers; `Go` when unset.
    pub(crate) answer: Mutex<Option<BeforeJob>>,
    /// Shown with `set_wait` when `before_job` is called.
    pub(crate) wait: Mutex<Option<WaitReason>>,
    calls: Mutex<Vec<HookCall>>,
    before_gates: Mutex<Vec<oneshot::Sender<BeforeJob>>>,
    after_gates: Mutex<Vec<oneshot::Sender<()>>>,
    /// How many `before_job` futures were dropped before they answered.
    dropped: Arc<AtomicUsize>,
}

impl GatedHook {
    pub(crate) fn calls(&self) -> Vec<HookCall> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    /// Answers every waiting `before_job` with `decision`.
    pub(crate) fn decide(&self, decision: BeforeJob) {
        for gate in self.before_gates.lock().unwrap().drain(..) {
            gate.send(decision.clone()).ok();
        }
    }

    /// Lets every waiting `after_job` resolve.
    pub(crate) fn release_after(&self) {
        for gate in self.after_gates.lock().unwrap().drain(..) {
            gate.send(()).ok();
        }
    }
}

/// Counts a `before_job` future dropped while still armed.
struct DropCounter {
    dropped: Arc<AtomicUsize>,
    armed: bool,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        if self.armed {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl JobHook for GatedHook {
    fn before_job(
        &self,
        _lane: &LaneId,
        job: &JobRecord,
        hcx: JobHookCx,
        cx: &mut App,
    ) -> Task<BeforeJob> {
        self.calls
            .lock()
            .unwrap()
            .push(HookCall::Before(job.run_id.clone()));
        if let Some(reason) = self.wait.lock().unwrap().clone() {
            hcx.set_wait(reason);
        }
        if !self.gate_before.load(Ordering::SeqCst) {
            let answer = self.answer.lock().unwrap().clone();
            return Task::ready(answer.unwrap_or(BeforeJob::Go));
        }
        let (sender, receiver) = oneshot::channel();
        self.before_gates.lock().unwrap().push(sender);
        let counter = DropCounter {
            dropped: self.dropped.clone(),
            armed: true,
        };
        cx.foreground_executor().spawn(async move {
            // The whole counter moves into the future, not only its flag.
            let mut counter = counter;
            let decision = receiver.await;
            counter.armed = false;
            decision.unwrap_or(BeforeJob::Go)
        })
    }

    fn after_job(&self, _lane: &LaneId, job: &JobRecord, end: &JobEnd, cx: &mut App) -> Task<()> {
        self.calls
            .lock()
            .unwrap()
            .push(HookCall::After(job.run_id.clone(), end.clone()));
        if !self.gate_after.load(Ordering::SeqCst) {
            return Task::ready(());
        }
        let (sender, receiver) = oneshot::channel();
        self.after_gates.lock().unwrap().push(sender);
        cx.foreground_executor().spawn(async move {
            receiver.await.ok();
        })
    }
}

/// The `task.toml` of a task with id and goal `id`, followed by `extra` lines.
pub(crate) fn task_toml(id: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{id}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// A tree of the top group `top` and the tasks at `folders`, each with the id of its last part.
pub(crate) fn tree(folders: &[&str]) -> DefinitionTree {
    let tasks: Vec<(&str, &str)> = folders.iter().map(|folder| (*folder, "")).collect();
    tree_with(&tasks)
}

/// [`tree`] whose tasks are `(folder, extra lines)`.
pub(crate) fn tree_with(tasks: &[(&str, &str)]) -> DefinitionTree {
    let mut files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            b"schema = 1\nid = \"p\"\ntitle = \"P\"\n".to_vec(),
        ),
        (
            RelPath::new("task.toml").unwrap(),
            task_toml("top", "").into_bytes(),
        ),
    ];
    for (folder, extra) in tasks {
        let id = folder.rsplit('/').next().unwrap();
        files.push((
            RelPath::new(format!("{folder}/task.toml")).unwrap(),
            task_toml(id, extra).into_bytes(),
        ));
    }
    DefinitionTree::parse_files(files).expect("the fixture parses")
}
