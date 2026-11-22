//! Tests of the Lanes view and its surfaces: a header and an idle reason, a client
//! window that reads the lanes from the store before any event, one notification per gate, and the
//! point moving focus into a registered section.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bongos_lanes_model::{
    DryRun, JobRecord, LaneId, LaneState, LanesState, PickOutcome, RunSubject, TaskId, WaitReason,
    Waiting,
};
use bongos_project_repo::{Git, Location, ProjectRepo, StateEvent, StateHandle, StateStore};
use bongos_workflow_model::record::NodeKey;
use gpui::{
    App, AppContext as _, Entity, KeyBinding, Stateful, Task, TestAppContext, VisualTestContext,
    actions,
};
use project::Project;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::lane_control::{DispatcherPhase, LaneStatus, LanesStatus};
use crate::lanes_view::{
    LanesSource, LanesView, LanesViewSection, LanesViewSections, PointRow, StoreLanes,
    show_lanes_view,
};
use crate::notify::{DesktopSink, GateNotifier};
use crate::store::write_state;
use crate::test_support::{SINCE, lanes_state, writer};
use crate::{LanesSettings, notify_open_gates};

fn lane_status(index: u32, state: LaneState) -> LaneStatus {
    LaneStatus {
        id: LaneId::new("build", index),
        state,
        reason: None,
        task: None,
        pause_requested: false,
        stop_requested: false,
        asked: None,
        job: None,
        jobs_done: 0,
    }
}

/// Three of five lanes busy, build-4 and build-5 find nothing; four tasks are
/// ready and six wait.
fn three_of_five_busy() -> (LanesStatus, DryRun) {
    let mut lanes: Vec<LaneStatus> = (1..=3)
        .map(|index| lane_status(index, LaneState::Running))
        .collect();
    lanes.extend((4..=5).map(|index| lane_status(index, LaneState::Idle)));
    let status = LanesStatus {
        phase: Some(DispatcherPhase::Running),
        table_commit: None,
        table_wait: None,
        lanes,
    };
    let waiting: Vec<Waiting> = (0..6)
        .map(|index| Waiting {
            task: TaskId::new(format!("blocked-{index}")),
            reason: WaitReason::Dependency {
                on: TaskId::new("usage"),
                progress: String::new(),
            },
        })
        .collect();
    let dry = DryRun {
        per_lane: vec![(
            LaneId::new("build", 4),
            PickOutcome::Nothing {
                reasons: waiting.clone(),
                owner_change: Vec::new(),
            },
        )],
        waiting,
        ready: 4,
        critical_path: 3,
        parallel: 3,
    };
    (status, dry)
}

struct FakeSource {
    status: LanesStatus,
    dry: Option<DryRun>,
}

impl LanesSource for FakeSource {
    fn status(&self, _cx: &App) -> Result<LanesStatus> {
        Ok(self.status.clone())
    }

    fn dry_run(&self, _cx: &App) -> Option<Task<Result<DryRun>>> {
        self.dry.clone().map(|dry| Task::ready(Ok(dry)))
    }
}

async fn workspace_window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        LanesSettings::register(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    (workspace, cx)
}

fn open_view(
    source: Rc<dyn LanesSource>,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<LanesView> {
    workspace.update_in(cx, |workspace, window, cx| {
        show_lanes_view(source, workspace, window, cx);
    });
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<LanesView>(cx)
            .next()
            .expect("the view is open")
    })
}

#[gpui::test]
async fn the_view_shows_a_header_and_an_idle_reason(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_window(cx).await;
    let (status, dry) = three_of_five_busy();
    let source = Rc::new(FakeSource {
        status,
        dry: Some(dry),
    });
    let view = open_view(source, &workspace, cx);
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let rows = view.rows().expect("the rows are read");
        assert_eq!(rows.header, "3 of 5 lanes busy, 4 tasks ready, 6 waiting");
        assert_eq!(rows.rows.len(), 5);
        assert_eq!(rows.rows[2].reason, None);
        assert_eq!(
            rows.rows[3].reason.as_deref(),
            Some("nothing eligible: 6 tasks wait for dependencies")
        );
    });
}

#[gpui::test]
async fn a_client_window_shows_the_lanes_from_the_store_before_any_event(cx: &mut TestAppContext) {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| write_state(tx, &lanes_state()))
        .unwrap();
    let folder = root.path().join("project");
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state: folder.join("state"),
        folder,
        remote: None,
        converted_from: None,
    };
    let reader = StateStore::open_reader(&location).unwrap();
    let repo = ProjectRepo::new(location, Git::locate().unwrap())
        .with_state(StateHandle::Reader(Arc::new(reader)));

    let (workspace, cx) = workspace_window(cx).await;
    let view = open_view(Rc::new(StoreLanes { repo }), &workspace, cx);
    // No event and no executor turn: the lanes are there as the view opens.
    view.read_with(cx, |view, _| {
        let rows = view.rows().expect("the lanes are read from the store");
        assert!(
            rows.header.starts_with("1 of 2 lanes busy"),
            "{}",
            rows.header
        );
        let ids: Vec<String> = rows.rows.iter().map(|row| row.id.to_string()).collect();
        assert_eq!(ids, ["build-1", "docs-1"]);
    });
}

#[derive(Default)]
struct FakeSink(Mutex<Vec<(String, String)>>);

impl DesktopSink for FakeSink {
    fn notify(&self, title: &str, body: &str) {
        self.0
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string()));
    }
}

#[test]
fn a_gate_notifies_once_and_only_when_enabled_and_no_client_is_attached() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let mut state: LanesState = lanes_state();
    let lane = state.lanes.get_mut(&LaneId::new("build", 1)).unwrap();
    lane.job = Some(JobRecord {
        run_id: "run-lane".to_string(),
        subject: RunSubject::Task(TaskId::new("leaf")),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
        extension: None,
    });
    writer.transact(|tx| write_state(tx, &state)).unwrap();
    let sink = Arc::new(FakeSink::default());
    let notifier = GateNotifier::new(sink.clone());
    let mut gates = BTreeMap::new();
    gates.insert(NodeKey::new("merge", 1), "Merge Usage events?".to_string());
    let notify = |run: &str, enabled: bool, attached: bool| {
        notify_open_gates(&notifier, &writer, run, &gates, enabled, attached, SINCE).unwrap()
    };

    assert_eq!(notify("run-lane", false, false), 0, "notifications are off");
    assert_eq!(
        notify("run-lane", true, true),
        0,
        "a window client is attached"
    );
    assert_eq!(notify("other-run", true, false), 0, "not a lane's run");
    assert_eq!(notify("run-lane", true, false), 1);
    assert_eq!(
        notify("run-lane", true, false),
        0,
        "the gate notified already"
    );
    assert_eq!(sink.0.lock().unwrap().len(), 1);
    assert_eq!(sink.0.lock().unwrap()[0].0, "build-1 needs you");
}

actions!(lanes_view_test, [Poke]);

struct PokeSection {
    pokes: Arc<AtomicUsize>,
}

impl LanesViewSection for PokeSection {
    fn key_context(&self) -> &'static str {
        "PokeSection"
    }

    fn title(&self) -> String {
        "Pools".to_string()
    }

    fn rows(&self, _cx: &App) -> Vec<String> {
        vec!["gpu: 1 of 2 in use".to_string()]
    }

    fn decorate(&self, element: Stateful<gpui::Div>) -> Stateful<gpui::Div> {
        use gpui::InteractiveElement as _;
        let pokes = self.pokes.clone();
        element.on_action(move |_: &Poke, _, _| {
            pokes.fetch_add(1, Ordering::SeqCst);
        })
    }
}

#[gpui::test]
async fn the_point_moves_focus_into_a_section_and_back(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_window(cx).await;
    let pokes = Arc::new(AtomicUsize::new(0));
    cx.update(|_, cx| {
        LanesViewSections::register(
            Arc::new(PokeSection {
                pokes: pokes.clone(),
            }),
            cx,
        );
        cx.bind_keys([
            KeyBinding::new("x", Poke, Some("PokeSection")),
            KeyBinding::new("n", menu::SelectNext, Some("LanesView")),
            KeyBinding::new("p", menu::SelectPrevious, Some("LanesView")),
        ]);
    });
    let (status, _) = three_of_five_busy();
    let view = open_view(Rc::new(FakeSource { status, dry: None }), &workspace, cx);
    cx.run_until_parked();
    let own = view.read_with(cx, |view, cx| gpui::Focusable::focus_handle(view, cx));
    let section = view.read_with(cx, |view, _| view.section_handle(0).unwrap());
    cx.update(|window, cx| window.focus(&own, cx));
    view.read_with(cx, |view, _| {
        assert_eq!(view.point(), Some(PointRow::Lane(0)));
    });

    // On a lane row the section's binding does not fire.
    cx.simulate_keystrokes("x");
    assert_eq!(pokes.load(Ordering::SeqCst), 0);

    // Five lane rows, then the section's row.
    cx.simulate_keystrokes("n n n n n");
    view.read_with(cx, |view, _| {
        assert_eq!(view.point(), Some(PointRow::Section(0, 0)));
    });
    cx.update(|window, _| assert!(section.is_focused(window)));
    cx.simulate_keystrokes("x");
    assert_eq!(pokes.load(Ordering::SeqCst), 1);

    // Back on a lane row the view's own bindings fire and focus is the view's again.
    cx.simulate_keystrokes("p");
    view.read_with(cx, |view, _| {
        assert_eq!(view.point(), Some(PointRow::Lane(4)));
    });
    cx.update(|window, _| {
        assert!(own.is_focused(window));
        assert!(!section.is_focused(window));
    });
    cx.simulate_keystrokes("x");
    assert_eq!(pokes.load(Ordering::SeqCst), 1);
}

/// A source with a goal for each task, a lifecycle for each run, and a channel of changes the test
/// sends on. Its status is replaced by the test.
struct LiveSource {
    status: std::cell::RefCell<LanesStatus>,
    changes: std::cell::RefCell<Option<futures::channel::mpsc::UnboundedReceiver<StateEvent>>>,
}

impl LanesSource for LiveSource {
    fn status(&self, _cx: &App) -> Result<LanesStatus> {
        Ok(self.status.borrow().clone())
    }

    fn goal(&self, task: &TaskId, _cx: &App) -> Option<String> {
        Some(format!("goal of {}", task.0))
    }

    fn run_lifecycle(&self, run: &str, _cx: &App) -> Option<String> {
        Some(format!("{run} is running"))
    }

    fn changes(&self) -> Option<futures::channel::mpsc::UnboundedReceiver<StateEvent>> {
        self.changes.borrow_mut().take()
    }
}

fn working_status(task: &str) -> LanesStatus {
    let mut lane = lane_status(1, LaneState::Running);
    lane.task = Some(TaskId::new(task));
    lane.job = Some(JobRecord {
        run_id: "run-1".to_string(),
        subject: RunSubject::Task(TaskId::new(task)),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
        extension: None,
    });
    LanesStatus {
        phase: Some(DispatcherPhase::Running),
        table_commit: None,
        table_wait: None,
        lanes: vec![lane, lane_status(2, LaneState::Idle)],
    }
}

fn changed() -> StateEvent {
    StateEvent::Changed {
        generation: 1,
        task_ids: Vec::new(),
        namespaces: Vec::new(),
    }
}

fn open_view_with_clock(
    source: Rc<dyn LanesSource>,
    now: chrono::DateTime<chrono::Utc>,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<LanesView> {
    workspace.update_in(cx, |workspace, window, cx| {
        let view = cx.new(|cx| LanesView::with_clock(source, Rc::new(move || now), cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    })
}

fn clock_at(text: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[gpui::test]
async fn a_lane_row_shows_its_goal_run_lifecycle_and_time_in_job(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_window(cx).await;
    let source = Rc::new(LiveSource {
        status: std::cell::RefCell::new(working_status("leaf")),
        changes: Default::default(),
    });
    // The job started at 10:00:00; the injected clock says 10:12:30.
    let view = open_view_with_clock(source, clock_at("2026-03-01T10:12:30Z"), &workspace, cx);
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let row = &view.rows().unwrap().rows[0];
        assert_eq!(row.goal.as_deref(), Some("goal of leaf"));
        assert_eq!(row.run_lifecycle.as_deref(), Some("run-1 is running"));
        assert_eq!(row.in_job.as_deref(), Some("12m"));
        // A lane with no job has none of these.
        let idle = &view.rows().unwrap().rows[1];
        assert_eq!((idle.goal.as_ref(), idle.in_job.as_ref()), (None, None));
    });
}

#[gpui::test]
async fn the_view_reads_again_when_the_lanes_change(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_window(cx).await;
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    let source = Rc::new(LiveSource {
        status: std::cell::RefCell::new(working_status("first")),
        changes: std::cell::RefCell::new(Some(receiver)),
    });
    let view = open_view_with_clock(
        source.clone(),
        clock_at("2026-03-01T10:00:00Z"),
        &workspace,
        cx,
    );
    cx.run_until_parked();
    let goal = |cx: &mut VisualTestContext| {
        view.read_with(cx, |view, _| view.rows().unwrap().rows[0].goal.clone())
    };
    assert_eq!(goal(cx).as_deref(), Some("goal of first"));

    // The state changes; nothing asks the view to refresh but the change event.
    *source.status.borrow_mut() = working_status("second");
    assert_eq!(goal(cx).as_deref(), Some("goal of first"));
    sender.unbounded_send(changed()).unwrap();
    cx.run_until_parked();
    assert_eq!(goal(cx).as_deref(), Some("goal of second"));
}

#[gpui::test]
async fn the_keys_act_on_the_row_at_point(cx: &mut TestAppContext) {
    use crate::lanes_view::LanesViewCommand;
    let (workspace, cx) = workspace_window(cx).await;
    let source = Rc::new(LiveSource {
        status: std::cell::RefCell::new(working_status("leaf")),
        changes: Default::default(),
    });
    let view = open_view_with_clock(source, clock_at("2026-03-01T10:00:00Z"), &workspace, cx);
    cx.run_until_parked();
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = seen.clone();
    cx.update(|_, cx| {
        cx.subscribe(&view, move |_, command: &LanesViewCommand, _| {
            sink.borrow_mut().push(command.clone())
        })
        .detach();
    });
    let build_1 = LaneId::new("build", 1);
    let build_2 = LaneId::new("build", 2);

    // The point starts on build-1, which works on `leaf` in `run-1`.
    view.update(cx, |view, cx| {
        view.open_task(cx);
        view.open_run(cx);
        view.pause_lane(cx);
        view.continue_lane(cx);
        view.stop_lane(cx);
        view.dry_run(cx);
    });
    assert_eq!(
        *seen.borrow(),
        [
            LanesViewCommand::OpenTask(TaskId::new("leaf")),
            LanesViewCommand::OpenRun("run-1".to_string()),
            LanesViewCommand::Pause(build_1.clone()),
            LanesViewCommand::Continue(build_1.clone()),
            LanesViewCommand::Stop(build_1),
            LanesViewCommand::DryRun,
        ]
    );

    // On the idle lane there is no task and no run to open, but its lane still takes commands.
    seen.borrow_mut().clear();
    view.update_in(cx, |view, window, cx| {
        view.set_point(1, window, cx);
        view.open_task(cx);
        view.open_run(cx);
        view.pause_lane(cx);
    });
    assert_eq!(*seen.borrow(), [LanesViewCommand::Pause(build_2)]);
}
