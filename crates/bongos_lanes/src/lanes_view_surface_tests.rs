//! Tests of the Lanes view and its surfaces: the scenario-1 header and an idle reason, a client
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
use bongos_project_repo::{Git, Location, ProjectRepo, StateHandle, StateStore};
use bongos_workflow_model::record::NodeKey;
use gpui::{App, Entity, KeyBinding, Stateful, Task, TestAppContext, VisualTestContext, actions};
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

/// PRD scenario 1: three of five lanes busy, build-4 and build-5 find nothing; four tasks are
/// ready and six wait.
fn scenario_one() -> (LanesStatus, DryRun) {
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
async fn the_view_shows_scenario_ones_header_and_an_idle_reason(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_window(cx).await;
    let (status, dry) = scenario_one();
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
    let (status, _) = scenario_one();
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
