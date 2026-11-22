//! The handover through real sockets and a real state store in a temporary folder: the window is
//! one test app holding the writer, the engine host's side runs on a thread of its own (asking
//! for the writer and waiting on the lock as the host process does) and then serves the project
//! from a second test app. The window's runs, task checks and writer participants are recording
//! fakes.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use bongos_project_repo::{
    ClientKind, Location, OpenError, ProjectRepo, StateApplyHooks, StateHandle, StateOp,
    StateStore, StateWriter,
};
use bongos_project_repo_model::Author;
use bongos_workflow_model::NodeKey;
use futures::channel::oneshot;
use gpui::{App, AppContext as _, Task, TestAppContext};

use crate::engine_host_tests::{Fixture, VERSION, branch_of, fixture, set_branch, wait_for};
use crate::{
    ClientOptions, DrainChoice, EngineAccess, EngineAccesses, HandoverRequest, HandoverRuns,
    HandoverTimes, HostClient, HostCommand, HostInfo, HostMode, HostSpawner, MOVING_REFUSAL,
    MOVING_RETRY, Moving, OpenedState, SpawnedHost, WriterHost, WriterHosts, WriterOptions,
    WriterParticipant, WriterParticipants, did_not_start, expect_handover, install_handovers,
    is_moving, live_host, open_state_with_hooks, read_host_file, start_handover,
    take_writer_checked, take_writer_from_window, write_host_file,
};

const LONG: Duration = Duration::from_secs(60);
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Default)]
struct FakeState {
    log: Vec<String>,
    busy: usize,
    drain: Option<oneshot::Sender<()>>,
    /// What `host.json` said and whether the writer was still held when the runs were handed
    /// over.
    at_hand_over: Option<(Option<Moving>, bool)>,
    /// The live runs.
    runs: Vec<String>,
    /// Whether starts under the project's files are refused now.
    refusing: bool,
    /// The checks keys running now: each counts in `busy` until `ChecksDrained` parks it.
    checking: Vec<(String, NodeKey)>,
    /// What task checks' drain resolves with: the nodes whose checks it stopped.
    stopped_checks: Vec<(String, NodeKey)>,
    /// Whether task checks' drain waits for [`release_checks`] (its runs still stopping).
    hold_checks: bool,
    checks: Option<oneshot::Sender<Vec<(String, NodeKey)>>>,
}

/// The window's runs: `runs` are live, `busy` of them run a node until [`finish_nodes`].
struct FakeRuns {
    state: Rc<RefCell<FakeState>>,
    location: Location,
    writer: Arc<StateWriter>,
}

impl HandoverRuns for FakeRuns {
    fn live_runs(&self, _repo: &ProjectRepo, _cx: &mut App) -> Result<Vec<String>> {
        let mut state = self.state.borrow_mut();
        state.log.push("live".to_string());
        Ok(state.runs.clone())
    }

    fn in_flight(&self, _runs: &[String], _cx: &App) -> usize {
        self.state.borrow().busy
    }

    fn drain(&self, _cx: &mut App) -> oneshot::Receiver<()> {
        let mut state = self.state.borrow_mut();
        state.log.push("drain".to_string());
        let (sender, receiver) = oneshot::channel();
        if state.busy == 0 {
            sender.send(()).ok();
        } else {
            state.drain = Some(sender);
        }
        receiver
    }

    fn undrain(&self, _cx: &mut App) {
        let mut state = self.state.borrow_mut();
        state.log.push("undrain".to_string());
        state.drain = None;
    }

    fn drain_checks(&self, cx: &mut App) -> Task<Vec<(String, NodeKey)>> {
        let mut state = self.state.borrow_mut();
        state.log.push("drain checks".to_string());
        if !state.hold_checks {
            return Task::ready(state.stopped_checks.clone());
        }
        let (sender, receiver) = oneshot::channel();
        state.checks = Some(sender);
        cx.background_spawn(async move { receiver.await.unwrap_or_default() })
    }

    fn undrain_checks(&self, _cx: &mut App) {
        self.state
            .borrow_mut()
            .log
            .push("undrain checks".to_string());
    }

    fn checks_drained(&self, run: &str, key: &NodeKey, _cx: &mut App) {
        let mut state = self.state.borrow_mut();
        state.log.push(format!("checks drained {run} {key}"));
        let parked = (run.to_string(), key.clone());
        if let Some(index) = state.checking.iter().position(|check| *check == parked) {
            state.checking.remove(index);
            state.busy -= 1;
            if state.busy == 0
                && let Some(drain) = state.drain.take()
            {
                drain.send(()).ok();
            }
        }
    }

    fn interrupt(&self, runs: &[String], _cx: &mut App) {
        let mut state = self.state.borrow_mut();
        state.log.push(format!("interrupt {runs:?}"));
        state.busy = 0;
        if let Some(drain) = state.drain.take() {
            drain.send(()).ok();
        }
    }

    fn hand_over(&self, to_pid: u32, runs: &[String], _cx: &mut App) {
        let moving = read_host_file(&self.location).and_then(|info| info.moving);
        let mut state = self.state.borrow_mut();
        state.log.push(format!("hand over {runs:?} to {to_pid}"));
        state.at_hand_over = Some((moving, !self.writer.is_released()));
    }

    fn reload(&self, runs: &[String], _cx: &mut App) {
        self.state.borrow_mut().log.push(format!("reload {runs:?}"));
    }

    fn refuse_starts(&self, files: &std::path::Path, on: bool, _cx: &mut App) {
        assert_eq!(files, self.location.files);
        self.state.borrow_mut().refusing = on;
    }
}

fn finish_nodes(state: &Rc<RefCell<FakeState>>) {
    let mut state = state.borrow_mut();
    state.busy = 0;
    if let Some(drain) = state.drain.take() {
        drain.send(()).ok();
    }
}

/// Task checks' drain resolves: the check runs it stopped have ended.
fn release_checks(state: &Rc<RefCell<FakeState>>) {
    let mut state = state.borrow_mut();
    let stopped = state.stopped_checks.clone();
    if let Some(checks) = state.checks.take() {
        checks.send(stopped).ok();
    }
}

fn log(state: &Rc<RefCell<FakeState>>) -> Vec<String> {
    state.borrow().log.clone()
}

fn writer_of(fixture: &Fixture) -> Arc<StateWriter> {
    fixture
        .repo
        .state()
        .and_then(StateHandle::opened_writer)
        .cloned()
        .unwrap()
}

/// The window: it holds the writer, serves the socket and takes handovers.
fn window(
    fixture: &Fixture,
    runs: &[&str],
    busy: usize,
    start_timeout: Duration,
    cx: &mut TestAppContext,
) -> Rc<RefCell<FakeState>> {
    let state = Rc::new(RefCell::new(FakeState {
        busy,
        runs: runs.iter().map(ToString::to_string).collect(),
        ..FakeState::default()
    }));
    window_from(fixture, state, start_timeout, cx)
}

/// [`window`] with runs already set up in `state`.
fn window_from(
    fixture: &Fixture,
    state: Rc<RefCell<FakeState>>,
    start_timeout: Duration,
    cx: &mut TestAppContext,
) -> Rc<RefCell<FakeState>> {
    let fake = FakeRuns {
        state: state.clone(),
        location: fixture.location().clone(),
        writer: writer_of(fixture),
    };
    let times = HandoverTimes {
        start_timeout,
        handover_timeout: HANDOVER_TIMEOUT,
    };
    let mut client = fixture.client_options(ClientKind::Window);
    client.moving_timeout = LONG;
    cx.update(|cx| {
        install_handovers(Rc::new(fake), Rc::new(move |_| times), client, cx);
        let host =
            WriterHost::start(fixture.repo.clone(), fixture.writer_options(true), cx).unwrap();
        cx.default_global::<WriterHosts>().0.push(host);
        EngineAccesses::set(
            fixture.location().id.clone(),
            EngineAccess::Local(fixture.repo.clone()),
            cx,
        );
    });
    state
}

/// The engine host's side on a thread: asks the window for the writer and waits on the lock.
fn ask_for_writer(
    fixture: &Fixture,
    cx: &TestAppContext,
) -> mpsc::Receiver<Result<StateWriter, String>> {
    let location = fixture.location().clone();
    let window = read_host_file(&location).expect("the window's host.json");
    let options = fixture.client_options(ClientKind::Cli);
    let executor = cx.executor();
    let hooks: &'static StateApplyHooks = Box::leak(Box::new(StateApplyHooks::new()));
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        sender
            .send(take_writer_from_window(
                &location, hooks, &window, options, LONG, executor,
            ))
            .ok();
    });
    receiver
}

fn wait_for_writer(
    taken: &mpsc::Receiver<Result<StateWriter, String>>,
    cx: &mut TestAppContext,
) -> Result<StateWriter, String> {
    let mut got = None;
    wait_for(cx, |_| {
        if let Ok(result) = taken.try_recv() {
            got = Some(result);
        }
        got.is_some()
    });
    got.unwrap()
}

/// The engine host serving the project with the writer it took, in its own app, at the same
/// socket path the window served.
fn serve_host(
    fixture: &Fixture,
    writer: StateWriter,
    host_cx: &mut TestAppContext,
) -> (WriterHost, ProjectRepo) {
    let repo = ProjectRepo::new(fixture.location().clone(), fixture.repo.git().clone())
        .with_state(StateHandle::Writer(Arc::new(writer)));
    let options = WriterOptions {
        mode: HostMode::Host,
        bongos_version: VERSION.to_string(),
        hosts_dir: fixture.hosts_dir(),
        serve_socket: true,
    };
    let host = host_cx
        .update(|cx| WriterHost::start(repo.clone(), options, cx))
        .unwrap();
    (host, repo)
}

fn is_remote(fixture: &Fixture, cx: &mut TestAppContext) -> bool {
    let id = fixture.location().id.clone();
    cx.update(|cx| EngineAccesses::get(&id, cx))
        .is_some_and(|access| !access.is_local())
}

/// Moves the branch of task `l1` from `from` to `to` (a compare-and-set, so a write that does
/// not land in order is a conflict that changes nothing).
fn write(
    repo: &ProjectRepo,
    from: Option<&str>,
    to: &str,
    cx: &TestAppContext,
) -> Task<Result<()>> {
    let repo = repo.clone();
    let ops = vec![StateOp::Set {
        task: "l1".to_string(),
        field: "branch".to_string(),
        expected: from.map(|from| toml::Value::String(from.to_string())),
        value: Some(toml::Value::String(to.to_string())),
    }];
    cx.background_spawn(async move { repo.apply_state(ops, Author::agent("chat 1")).await })
}

fn generation(handle: &StateHandle) -> u64 {
    handle.read(|read| read.generation()).unwrap()
}

// `lanes: start` with a run in a long node: the node finishes in the window, `moving` is on disk
// and the writer still held when the runs are handed over, then the lock goes to the host; the
// window's own writes land in order (during the drain on its own writer, after the release
// through the queue and then the host), the generation only goes up, and the window ends as a
// client of the host.
#[gpui::test]
async fn a_handover_drains_then_moves_the_writer_and_the_window_attaches_to_the_host(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    write(&fixture.repo, None, "before", cx).await.unwrap();
    let before = generation(fixture.repo.state().unwrap());

    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain".to_string()));
    // The window still holds the writer during the drain: its own write lands at once.
    write(&fixture.repo, Some("before"), "during", cx)
        .await
        .unwrap();
    let during = generation(fixture.repo.state().unwrap());
    assert!(during > before, "{during} > {before}");
    assert!(taken.try_recv().is_err(), "the host waits on the lock");

    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).expect("the host took the writer");
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            "live".to_string(),
            format!("hand over [\"run-1\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (moving, held) = state.borrow().at_hand_over.clone().unwrap();
    assert_eq!(moving.map(|moving| moving.to_pid), Some(pid));
    assert!(held, "the writer was released after the hand-over");
    assert!(writer_of(&fixture).is_released());

    // Released, not yet switched: the window's write waits in its queue.
    let queued = write(&fixture.repo, Some("during"), "queued", cx);
    let (host, host_repo) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
    queued.await.unwrap();
    let host_state = host_repo.state().unwrap();
    assert_eq!(branch_of(host_state, "l1").as_deref(), Some("queued"));
    let after = generation(host_state);
    assert!(after > during, "{after} > {during}");

    // From now on the window writes through the host and reads what the host wrote.
    write(&fixture.repo, Some("queued"), "later", cx)
        .await
        .unwrap();
    assert_eq!(branch_of(host_state, "l1").as_deref(), Some("later"));
    assert_eq!(
        branch_of(fixture.repo.state().unwrap(), "l1").as_deref(),
        Some("later")
    );
    assert!(generation(host_state) > after);
    let info = live_host(fixture.location()).unwrap();
    assert_eq!(info.mode, HostMode::Host);
    assert_eq!(info.moving, None);
    assert!(cx.update(|cx| cx.global::<WriterHosts>().hosts().is_empty()));
    // The writer moved for good: the window's backup timer ends instead of waiting.
    assert!(fixture.repo.backups_stopped() && !fixture.repo.backups_released());
    drop(host);
}

// A client's request during the move is refused with the moving text; a client that tries again
// reaches the host once it serves.
#[gpui::test]
async fn a_client_request_during_the_move_is_refused_and_retried_until_the_host_serves(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let socket = live_host(fixture.location()).unwrap().socket.unwrap();
    let connect = |timeout: Duration| {
        let mut options = fixture.client_options(ClientKind::Cli);
        options.moving_timeout = timeout;
        HostClient::connect(&socket, options, cx.executor()).unwrap()
    };
    let once = connect(Duration::ZERO);
    let retrying = connect(LONG);

    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain".to_string()));
    let error = once
        .request(HostCommand::WorkWrite {
            state_ops: vec![set_branch("l1", "refused")],
            origin: Author::agent("cli"),
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), MOVING_REFUSAL);

    let done = Arc::new(AtomicBool::new(false));
    let answered = {
        let done = done.clone();
        let request = retrying.request(HostCommand::WorkWrite {
            state_ops: vec![set_branch("l1", "retried")],
            origin: Author::agent("cli"),
        });
        cx.background_spawn(async move {
            let answer = request.await;
            done.store(true, Ordering::SeqCst);
            answer
        })
    };
    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let (_host, host_repo) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| {
        cx.executor().advance_clock(MOVING_RETRY);
        done.load(Ordering::SeqCst)
    });
    answered.await.unwrap();
    assert_eq!(
        branch_of(host_repo.state().unwrap(), "l1").as_deref(),
        Some("retried")
    );
}

// With no live run, nothing waits on a drain: the writer moves at once.
#[gpui::test]
async fn a_window_with_no_live_run_hands_over_without_waiting(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &[], 0, LONG, cx);
    let taken = ask_for_writer(&fixture, cx);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            "live".to_string(),
            format!("hand over [] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
}

// A drain past the handover timeout asks again; "Interrupt them now" then interrupts the runs
// handed over and the move goes on. `lanes: start` hears the window is a client of the host.
#[gpui::test]
async fn a_drain_past_its_timeout_asks_again_and_interrupt_them_now_moves_on(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let asked = Rc::new(RefCell::new(Vec::new()));
    let prompt_asked = asked.clone();
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Wait,
                    ask_again: Some(Rc::new(move |busy, _cx| {
                        prompt_asked.borrow_mut().push(busy);
                        Task::ready(DrainChoice::InterruptNow)
                    })),
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain".to_string()));
    cx.executor()
        .advance_clock(HANDOVER_TIMEOUT - Duration::from_secs(1));
    cx.run_until_parked();
    assert!(asked.borrow().is_empty(), "not asked before the timeout");
    cx.executor().advance_clock(Duration::from_secs(1));
    let writer = wait_for_writer(&taken, cx).unwrap();
    assert_eq!(*asked.borrow(), vec![1]);
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            "interrupt [\"run-1\"]".to_string(),
            "live".to_string(),
            format!("hand over [\"run-1\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    let Ok(client) = watch.done.await.unwrap() else {
        panic!("the handover did not end with the host");
    };
    assert_eq!(client.welcomed().mode, HostMode::Host);
}

// The person cancelling keeps the writer in the window: the host is refused and nothing moved.
#[gpui::test]
async fn a_cancelled_handover_keeps_the_writer_in_the_window(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Cancel,
                    ask_again: None,
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    let Err(refused) = wait_for_writer(&taken, cx) else {
        panic!("the host took the writer of a cancelled handover");
    };
    assert!(refused.contains(crate::CANCELLED), "{refused}");
    assert!(watch.done.await.unwrap().is_err());
    assert_eq!(
        log(&state),
        vec!["live", "drain", "drain checks", "undrain checks", "undrain"]
    );
    assert!(!state.borrow().refusing, "runs start here again");
    assert!(!writer_of(&fixture).is_released());
    assert_eq!(read_host_file(fixture.location()).unwrap().moving, None);
    write(&fixture.repo, None, "still-here", cx).await.unwrap();
    assert!(!cx.update(|cx| crate::is_moving(&id, cx)));
}

// The host never takes the lock: past the start timeout the window takes the writer back, its
// queued write lands here, the runs load again and `lanes: start` hears the host did not start.
#[gpui::test]
async fn a_host_that_never_takes_the_lock_gives_the_writer_back(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 0, Duration::from_millis(100), cx);
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Wait,
                    ask_again: None,
                },
                cx,
            )
        })
        .unwrap();
    // A host that asks for the writer and never waits on the lock.
    let socket = live_host(fixture.location()).unwrap().socket.unwrap();
    let asking = HostClient::connect(
        &socket,
        fixture.client_options(ClientKind::Cli),
        cx.executor(),
    )
    .unwrap();
    let released =
        cx.background_spawn(asking.request_unbounded(HostCommand::Host(crate::HostCtl::Handover)));
    wait_for(cx, |_| writer_of(&fixture).is_released());
    let queued = write(&fixture.repo, None, "queued", cx);
    released.await.unwrap();

    let Err(error) = watch.done.await.unwrap() else {
        panic!("the window attached to a host that never took the writer");
    };
    assert_eq!(error, did_not_start(&fixture.log()));
    queued.await.unwrap();
    assert!(!writer_of(&fixture).is_released());
    assert_eq!(
        branch_of(fixture.repo.state().unwrap(), "l1").as_deref(),
        Some("queued")
    );
    // Task checks then the runs undrain (the reverse of the drain), then the runs load again.
    let logged = log(&state);
    assert_eq!(
        logged[logged.len() - 3..],
        ["undrain checks", "undrain", "reload [\"run-1\"]"]
    );
    assert!(!state.borrow().refusing, "runs start here again");
    let info = live_host(fixture.location()).unwrap();
    assert_eq!((info.mode, info.moving.clone()), (HostMode::Window, None));
    // It serves again, and its own writes go to its own writer.
    let client = HostClient::connect(
        &info.socket.unwrap(),
        fixture.client_options(ClientKind::Cli),
        cx.executor(),
    )
    .unwrap();
    assert_eq!(client.welcomed().mode, HostMode::Window);
    write(&fixture.repo, Some("queued"), "back", cx)
        .await
        .unwrap();
    assert!(fixture.repo.sink().is_none());
}

// A run that became live during the drain (loaded, restarted) moves with the others, and from
// the host's ask until the window attached no run starts here.
#[gpui::test]
async fn a_run_live_by_the_end_of_the_drain_moves_too_and_none_starts_until_the_move_ends(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain".to_string()));
    assert!(
        state.borrow().refusing,
        "no run starts while the writer moves"
    );
    state.borrow_mut().runs.push("run-2".to_string());
    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    let handed = format!("hand over [\"run-1\", \"run-2\"] to {pid}");
    assert!(log(&state).contains(&handed), "{:?}", log(&state));
    assert!(
        state.borrow().refusing,
        "still refused between the release and the attach"
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
    assert!(!state.borrow().refusing, "runs start again once it moved");
}

/// A spawner that starts nothing and names this test process as the host it started.
struct SilentSpawner;

struct SilentChild;

impl SpawnedHost for SilentChild {
    fn try_exit(&mut self) -> Option<String> {
        None
    }

    fn release(self: Box<Self>) {}

    fn pid(&self) -> Option<u32> {
        Some(std::process::id())
    }
}

impl HostSpawner for SilentSpawner {
    fn spawn(&self, _: &str, _: &std::path::Path) -> Result<Box<dyn SpawnedHost>> {
        Ok(Box::new(SilentChild))
    }
}

// `lanes: start` gave up on a host that did not ask in time and told the person it did not
// start: when that host asks late, the window keeps the writer and stays local.
#[gpui::test]
async fn a_host_asking_after_lanes_start_gave_up_on_it_is_refused(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let start_timeout = Duration::from_millis(100);
    let state = window(&fixture, &["run-1"], 0, start_timeout, cx);
    let started = cx.update(|cx| {
        start_handover(
            &fixture.repo,
            Arc::new(SilentSpawner),
            fixture.log(),
            HandoverRequest {
                choice: DrainChoice::Wait,
                ask_again: None,
            },
            cx,
        )
    });
    cx.run_until_parked();
    cx.executor().advance_clock(start_timeout);
    let Err(error) = started.await else {
        panic!("lanes: start ended with the host it gave up on");
    };
    assert!(error.to_string().contains("did not start"), "{error:#}");

    let taken = ask_for_writer(&fixture, cx);
    let Err(refused) = wait_for_writer(&taken, cx) else {
        panic!("a host the window gave up on took the writer");
    };
    assert!(refused.contains("gave up"), "{refused}");
    assert_eq!(log(&state), Vec::<String>::new());
    assert!(!writer_of(&fixture).is_released());
    assert!(!is_remote(&fixture, cx));
    assert!(!state.borrow().refusing);
}

fn lanes_start(
    fixture: &Fixture,
    spawner: Arc<dyn HostSpawner>,
    cx: &mut TestAppContext,
) -> Task<Result<HostClient>> {
    cx.update(|cx| {
        start_handover(
            &fixture.repo,
            spawner,
            fixture.log(),
            HandoverRequest {
                choice: DrainChoice::Wait,
                ask_again: None,
            },
            cx,
        )
    })
}

// The host asks for the writer just as `lanes: start`'s start timeout fires: the move it began
// goes on (starts stay refused), the window is not told the host did not start, and it attaches
// to the host once the host takes the writer.
#[gpui::test]
async fn a_host_asking_as_the_start_timeout_fires_still_takes_the_writer(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let started = lanes_start(&fixture, Arc::new(SilentSpawner), cx);
    cx.run_until_parked();
    // The host's request is served, and the timeout fires before `lanes: start` hears of it.
    let peer = crate::commands::local_identity().unwrap();
    let serving = cx.update(|cx| crate::handover::serve_handover(&fixture.repo, &peer, cx));
    cx.executor().advance_clock(LONG);
    cx.run_until_parked();
    let id = fixture.location().id.clone();
    assert!(cx.update(|cx| is_moving(&id, cx)), "the move goes on");
    assert!(state.borrow().refusing, "no run starts while it moves");
    assert!(
        cx.update(|cx| crate::handover::abandoned_hosts(cx))
            .is_empty()
    );

    finish_nodes(&state);
    let hooks = StateApplyHooks::new();
    let mut writer = None;
    wait_for(cx, |_| {
        if writer.is_none() {
            writer = StateStore::open_writer_with_hooks(fixture.location(), &hooks).ok();
        }
        writer.is_some()
    });
    let (_host, _) = serve_host(&fixture, writer.unwrap(), host_cx);
    let client = started
        .await
        .expect("lanes: start ends with the host's client");
    assert_eq!(client.welcomed().mode, HostMode::Host);
    assert!(serving.await.is_ok());
    assert!(!state.borrow().refusing, "runs start again once it moved");
}

/// A spawner whose host has a pid no process has.
struct DeadSpawner;

struct DeadChild;

impl SpawnedHost for DeadChild {
    fn try_exit(&mut self) -> Option<String> {
        None
    }

    fn release(self: Box<Self>) {}

    fn pid(&self) -> Option<u32> {
        Some(dead_pid(3))
    }
}

impl HostSpawner for DeadSpawner {
    fn spawn(&self, _: &str, _: &std::path::Path) -> Result<Box<dyn SpawnedHost>> {
        Ok(Box::new(DeadChild))
    }
}

// A host `lanes: start` gave up on that ended is forgotten: its pid may be given to another
// process, which is not refused.
#[gpui::test]
async fn an_abandoned_host_that_ended_is_forgotten(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let start_timeout = Duration::from_millis(100);
    let _state = window(&fixture, &["run-1"], 1, start_timeout, cx);
    for spawner in [
        Arc::new(DeadSpawner) as Arc<dyn HostSpawner>,
        Arc::new(SilentSpawner),
    ] {
        let started = lanes_start(&fixture, spawner, cx);
        cx.run_until_parked();
        cx.executor().advance_clock(start_timeout);
        assert!(started.await.is_err());
    }
    assert_eq!(
        cx.update(|cx| crate::handover::abandoned_hosts(cx)),
        vec![std::process::id()],
        "the ended host was dropped"
    );
}

// While the writer moves a request is tried again only when the writer refused it as moving or
// cannot have applied it (never sent, or sent after the writer said it moves); one whose
// connection closed while it was in flight before that may have been applied by the old writer,
// so it is not sent again.
#[test]
fn only_a_moving_refusal_or_a_request_never_applied_is_tried_again() {
    use crate::client::{NotApplied, is_moving};
    let refused = anyhow::anyhow!(MOVING_REFUSAL);
    let never_sent = anyhow::Error::new(NotApplied("the engine host closed".to_string()));
    let in_flight = anyhow::anyhow!("the engine host closed the connection");
    assert!(is_moving(&refused, false));
    assert!(is_moving(&refused, true));
    assert!(is_moving(&never_sent, true));
    assert!(!is_moving(&never_sent, false));
    assert!(!is_moving(&in_flight, true));
}

// A connection says the writer moves only while its latest answer, by request order, refused a
// request as moving: once a later request is answered the move was cancelled, so a request then
// lost to a closed connection may have been applied and is not tried again.
#[test]
fn a_cancelled_move_ends_the_moving_refusal_of_a_connection() {
    use crate::client::MovingRefusals;
    let mut refusals = MovingRefusals::default();
    assert!(!refusals.moving());
    refusals.heard(0, false);
    assert!(!refusals.moving());
    refusals.heard(3, true);
    assert!(refusals.moving(), "refused as moving");
    // An earlier request answered late says nothing of the move.
    refusals.heard(2, false);
    assert!(refusals.moving());
    refusals.heard(4, false);
    assert!(!refusals.moving(), "answered again: the move was cancelled");
    refusals.heard(5, true);
    assert!(refusals.moving(), "a new move");
}

fn options_as(fixture: &Fixture, own_pid: u32) -> ClientOptions {
    let mut options = fixture.client_options(ClientKind::Window);
    options.own_pid = own_pid;
    options.moving_timeout = LONG;
    options
}

/// A pid no process has: above the largest pid Linux hands out (`pid_max` is at most 2^22).
fn dead_pid(n: u32) -> u32 {
    i32::MAX as u32 - n
}

// A third process opening the project while `moving` is set does not keep the lock: it waits
// for the move to end and attaches to the writer as a client.
#[gpui::test]
async fn an_opener_during_a_move_waits_and_attaches_as_a_client(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let mut host = cx
        .update(|cx| WriterHost::start(fixture.repo.clone(), fixture.writer_options(true), cx))
        .unwrap();
    // The writer moves to this (live) process; the lock is free while it moves.
    host.set_moving(Some(Moving::to(std::process::id())))
        .unwrap();
    futures::executor::block_on(fixture.repo.release_writer(LONG)).unwrap();

    let location = fixture.location().clone();
    let options = options_as(&fixture, 1);
    let executor = cx.executor();
    let (sender, opened) = mpsc::channel();
    std::thread::spawn(move || {
        let hooks = StateApplyHooks::new();
        sender
            .send(open_state_with_hooks(&location, &hooks, options, executor))
            .ok();
    });
    // While it waits, the lock is never kept by it: the window takes it back at once.
    let mut retaken = false;
    wait_for(cx, |_| {
        retaken = retaken || fixture.repo.retake_writer().is_ok();
        retaken
    });
    assert!(
        opened.try_recv().is_err(),
        "it waits while the move is recorded"
    );
    cx.update(|cx| host.take_back(cx)).unwrap();

    let mut result = None;
    wait_for(cx, |_| {
        if let Ok(opened) = opened.try_recv() {
            result = Some(opened);
        }
        result.is_some()
    });
    match result.unwrap().unwrap() {
        OpenedState::Client { host: info, .. } => assert_eq!(info.mode, HostMode::Window),
        OpenedState::Writer(_) => panic!("an opener during a move took the writer"),
    }
}

// A take-back whose socket cannot be bound again still clears the move in `host.json`, so no
// opener waits on a move that ended: it names the window with no move and no socket.
#[gpui::test]
async fn a_take_back_that_cannot_serve_still_clears_the_move(cx: &mut TestAppContext) {
    let fixture = fixture();
    let mut host = cx
        .update(|cx| WriterHost::start(fixture.repo.clone(), fixture.writer_options(true), cx))
        .unwrap();
    let socket = host.info().socket.clone().unwrap();
    host.set_moving(Some(Moving::to(std::process::id())))
        .unwrap();
    host.stop_accepting();
    // Something that is not a socket now sits where the socket would be bound again.
    std::fs::create_dir_all(socket.join("blocked")).unwrap();

    assert!(cx.update(|cx| host.take_back(cx)).is_err());
    let info = read_host_file(fixture.location()).unwrap();
    assert_eq!(
        (info.pid, info.moving, info.socket),
        (std::process::id(), None, None)
    );
}

// An opener that read `host.json` before the move was written and then won the lock reads it
// again and lets the lock go at once.
#[gpui::test]
async fn a_late_opener_that_wins_the_lock_during_a_move_lets_it_go(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let mut host = cx
        .update(|cx| WriterHost::start(fixture.repo.clone(), fixture.writer_options(false), cx))
        .unwrap();
    host.set_moving(Some(Moving::to(std::process::id())))
        .unwrap();
    futures::executor::block_on(fixture.repo.release_writer(LONG)).unwrap();

    let hooks = StateApplyHooks::new();
    let won = take_writer_checked(fixture.location(), &hooks, 1).unwrap();
    assert!(won.is_none(), "the late winner let the lock go");
    // The lock is free again for the process the writer moves to.
    let next = StateStore::open_writer_with_hooks(fixture.location(), &hooks).unwrap();
    drop(next);
    // The process the writer moves to keeps it.
    let kept = take_writer_checked(fixture.location(), &hooks, std::process::id()).unwrap();
    assert!(kept.is_some());
}

// A move whose window and host both died is stale: the next opener ignores it and takes the
// writer, and its own `host.json` clears it.
#[gpui::test]
async fn a_stale_move_with_both_processes_dead_is_ignored(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let location = fixture.location().clone();
    let dead_window = dead_pid(1);
    let dead_host = dead_pid(2);
    futures::executor::block_on(fixture.repo.release_writer(LONG)).unwrap();
    write_host_file(
        &location,
        &HostInfo {
            pid: dead_window,
            start_time: None,
            socket: None,
            bongos_version: VERSION.to_string(),
            mode: HostMode::Window,
            started: chrono::Utc::now(),
            moving: Some(Moving {
                to_pid: dead_host,
                to_start_time: None,
            }),
        },
    )
    .unwrap();
    let hooks = StateApplyHooks::new();
    let opened =
        open_state_with_hooks(&location, &hooks, options_as(&fixture, 1), cx.executor()).unwrap();
    let OpenedState::Writer(writer) = opened else {
        panic!("a stale move kept the opener from the writer");
    };
    let repo = ProjectRepo::new(location.clone(), fixture.repo.git().clone())
        .with_state(StateHandle::Writer(writer));
    let host = cx
        .update(|cx| WriterHost::start(repo, fixture.writer_options(false), cx))
        .unwrap();
    assert_eq!(read_host_file(&location).unwrap().moving, None);
    assert!(matches!(
        StateStore::open_writer_with_hooks(&location, &hooks),
        Err(OpenError::HeldBy { .. })
    ));
    drop(host);
}

// The window and the host both killed mid-handover: the next opener clears the stale move,
// takes the writer and starts its writer participants, where the startup recovery runs (the
// lanes register theirs). A second opener after the same crash attaches to it as a client and
// recovers nothing itself.
#[gpui::test]
async fn after_both_die_mid_handover_one_opener_clears_the_move_and_recovers(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let location = fixture.location().clone();
    let id = location.id.clone();
    futures::executor::block_on(fixture.repo.release_writer(LONG)).unwrap();
    write_host_file(
        &location,
        &HostInfo {
            pid: dead_pid(20),
            start_time: Some(3),
            socket: None,
            bongos_version: VERSION.to_string(),
            mode: HostMode::Window,
            started: chrono::Utc::now(),
            moving: Some(Moving {
                to_pid: dead_pid(21),
                to_start_time: Some(4),
            }),
        },
    )
    .unwrap();
    let recovered = Rc::new(RefCell::new(Vec::new()));
    FakeParticipant::register("recovery", &recovered, None, None, cx);

    let hooks = StateApplyHooks::new();
    let opened =
        open_state_with_hooks(&location, &hooks, options_as(&fixture, 1), cx.executor()).unwrap();
    let OpenedState::Writer(writer) = opened else {
        panic!("a stale move kept the first opener from the writer");
    };
    let repo = ProjectRepo::new(location.clone(), fixture.repo.git().clone())
        .with_state(StateHandle::Writer(writer));
    let host = cx
        .update(|cx| WriterHost::start(repo, fixture.writer_options(true), cx))
        .unwrap();
    assert_eq!(read_host_file(&location).unwrap().moving, None);
    assert_eq!(*recovered.borrow(), [format!("recovery started {id}")]);

    let second_location = location.clone();
    let options = options_as(&fixture, 2);
    let executor = cx.executor();
    let (sender, opened) = mpsc::channel();
    std::thread::spawn(move || {
        let hooks = StateApplyHooks::new();
        sender
            .send(open_state_with_hooks(
                &second_location,
                &hooks,
                options,
                executor,
            ))
            .ok();
    });
    let mut result = None;
    wait_for(cx, |_| {
        if let Ok(opened) = opened.try_recv() {
            result = Some(opened);
        }
        result.is_some()
    });
    match result.unwrap().unwrap() {
        OpenedState::Client { host: info, .. } => {
            assert_eq!((info.pid, info.moving), (std::process::id(), None))
        }
        OpenedState::Writer(_) => panic!("two openers after the crash both took the writer"),
    }
    assert_eq!(recovered.borrow().len(), 1, "only the writer recovered");
    drop(host);
}

/// A writer participant that records what it is told in `log`, with how many nodes ran when it
/// was drained. With a gate, its drain resolves only once the gate is opened.
struct FakeParticipant {
    name: &'static str,
    log: Rc<RefCell<Vec<String>>>,
    runs: Option<Rc<RefCell<FakeState>>>,
    gate: RefCell<Option<oneshot::Receiver<()>>>,
    /// Its undrain fails (after recording it).
    fails: bool,
}

impl FakeParticipant {
    fn register(
        name: &'static str,
        log: &Rc<RefCell<Vec<String>>>,
        runs: Option<&Rc<RefCell<FakeState>>>,
        gate: Option<oneshot::Receiver<()>>,
        cx: &mut TestAppContext,
    ) {
        Self::register_with(name, log, runs, gate, false, cx);
    }

    fn register_with(
        name: &'static str,
        log: &Rc<RefCell<Vec<String>>>,
        runs: Option<&Rc<RefCell<FakeState>>>,
        gate: Option<oneshot::Receiver<()>>,
        fails: bool,
        cx: &mut TestAppContext,
    ) {
        let participant = Rc::new(Self {
            name,
            log: log.clone(),
            runs: runs.cloned(),
            gate: RefCell::new(gate),
            fails,
        });
        cx.update(|cx| WriterParticipants::register(participant, cx));
    }

    fn push(&self, line: String) {
        self.log.borrow_mut().push(line.clone());
        if let Some(runs) = &self.runs {
            runs.borrow_mut().log.push(line);
        }
    }
}

impl WriterParticipant for FakeParticipant {
    fn started(&self, project: &str, _cx: &mut App) {
        self.push(format!("{} started {project}", self.name));
    }

    fn drain(&self, project: &str, cx: &mut App) -> Task<()> {
        let busy = self.runs.as_ref().map_or(0, |runs| runs.borrow().busy);
        self.push(format!("{} drain {project} busy={busy}", self.name));
        match self.gate.borrow_mut().take() {
            Some(gate) => cx.background_spawn(async move {
                gate.await.ok();
            }),
            None => Task::ready(()),
        }
    }

    fn undrain(&self, project: &str, _cx: &mut App) -> Result<()> {
        self.push(format!("{} undrain {project}", self.name));
        if self.fails {
            anyhow::bail!("{} cannot undrain", self.name);
        }
        Ok(())
    }
}

fn check_key() -> NodeKey {
    NodeKey::new("check", 1)
}

fn wait_request() -> HandoverRequest {
    HandoverRequest {
        choice: DrainChoice::Wait,
        ask_again: None,
    }
}

// `lanes: start` with a hand-started run in a check and another in an agent node: task checks'
// drain names run-1's check key, which alone gets `ChecksDrained`, before the runs are handed
// over; the other run's node finishes first.
#[gpui::test]
async fn a_run_in_a_check_gets_checks_drained_for_exactly_the_returned_key(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = Rc::new(RefCell::new(FakeState {
        busy: 2,
        runs: vec!["run-1".to_string(), "run-2".to_string()],
        checking: vec![("run-1".to_string(), check_key())],
        stopped_checks: vec![("run-1".to_string(), check_key())],
        ..FakeState::default()
    }));
    let state = window_from(&fixture, state, LONG, cx);
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| expect_handover(&id, wait_request(), cx))
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| {
        log(&state).contains(&format!("checks drained run-1 {}", check_key()))
    });
    assert_eq!(state.borrow().busy, 1, "run-2's node still runs");
    assert!(taken.try_recv().is_err(), "the host waits on the lock");

    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            format!("checks drained run-1 {}", check_key()),
            "live".to_string(),
            format!("hand over [\"run-1\", \"run-2\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    assert!(watch.done.await.unwrap().is_ok());
}

// A run in a `checks` node whose check would run for hours: task checks stop it, the key is
// parked by `ChecksDrained`, and the writer moves with no node finishing and no clock moving.
#[gpui::test]
async fn a_check_that_would_run_for_hours_does_not_delay_the_drain(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = Rc::new(RefCell::new(FakeState {
        busy: 1,
        runs: vec!["run-1".to_string()],
        checking: vec![("run-1".to_string(), check_key())],
        stopped_checks: vec![("run-1".to_string(), check_key())],
        ..FakeState::default()
    }));
    let state = window_from(&fixture, state, LONG, cx);
    let taken = ask_for_writer(&fixture, cx);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let log = log(&state);
    let drained = format!("checks drained run-1 {}", check_key());
    let at = |line: &str| log.iter().position(|logged| logged == line).unwrap();
    assert!(at(&drained) < at(&format!("hand over [\"run-1\"] to {}", std::process::id())));
    assert!(!log.iter().any(|line| line.starts_with("interrupt")));
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
}

// A writer participant is started in the window when it becomes the writer, drained during the
// handover once no node runs, and started in the host once it takes the writer.
#[gpui::test]
async fn a_writer_participant_starts_in_the_window_drains_at_the_handover_and_starts_in_the_host(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let state = Rc::new(RefCell::new(FakeState {
        busy: 1,
        runs: vec!["run-1".to_string()],
        ..FakeState::default()
    }));
    let in_window = Rc::new(RefCell::new(Vec::new()));
    FakeParticipant::register("p", &in_window, Some(&state), None, cx);
    let in_host = Rc::new(RefCell::new(Vec::new()));
    FakeParticipant::register("h", &in_host, None, None, host_cx);
    let state = window_from(&fixture, state, LONG, cx);
    assert_eq!(*in_window.borrow(), vec![format!("p started {id}")]);

    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    assert_eq!(in_window.borrow().len(), 1, "not drained while a node runs");
    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    assert_eq!(
        *in_window.borrow(),
        vec![format!("p started {id}"), format!("p drain {id} busy=0")]
    );
    assert!(in_host.borrow().is_empty());
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    assert_eq!(*in_host.borrow(), vec![format!("h started {id}")]);
    wait_for(cx, |cx| is_remote(&fixture, cx));
    assert_eq!(
        in_window.borrow().len(),
        2,
        "never undrained after the move"
    );
}

// Two participants, a node still running and a check stopping: the runs' and task checks' drains
// start together; the check is parked and the node finishes; only then participant 1 drains, and
// participant 2 only once participant 1 resolved; then the runs are handed over.
#[gpui::test]
async fn participants_drain_in_order_only_after_the_runs_and_checks_drained(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let state = Rc::new(RefCell::new(FakeState {
        busy: 2,
        runs: vec!["run-1".to_string(), "run-2".to_string()],
        checking: vec![("run-1".to_string(), check_key())],
        stopped_checks: vec![("run-1".to_string(), check_key())],
        hold_checks: true,
        ..FakeState::default()
    }));
    let participants = Rc::new(RefCell::new(Vec::new()));
    let (open_first, first_gate) = oneshot::channel();
    FakeParticipant::register("p1", &participants, Some(&state), Some(first_gate), cx);
    FakeParticipant::register("p2", &participants, Some(&state), None, cx);
    let state = window_from(&fixture, state, LONG, cx);
    let drains = |participants: &Rc<RefCell<Vec<String>>>| {
        participants
            .borrow()
            .iter()
            .filter(|line| line.contains(" drain "))
            .cloned()
            .collect::<Vec<_>>()
    };

    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    let started: Vec<String> = log(&state).into_iter().skip(2).collect();
    assert_eq!(started, vec!["live", "drain", "drain checks"]);

    release_checks(&state);
    let drained = format!("checks drained run-1 {}", check_key());
    wait_for(cx, |_| log(&state).contains(&drained));
    assert!(drains(&participants).is_empty(), "a node still runs");

    finish_nodes(&state);
    wait_for(cx, |_| !drains(&participants).is_empty());
    assert_eq!(drains(&participants), vec![format!("p1 drain {id} busy=0")]);
    assert!(taken.try_recv().is_err(), "the host waits on the lock");

    open_first.send(()).unwrap();
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            format!("p1 started {id}"),
            format!("p2 started {id}"),
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            drained,
            format!("p1 drain {id} busy=0"),
            format!("p2 drain {id} busy=0"),
            "live".to_string(),
            format!("hand over [\"run-1\", \"run-2\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
}

/// run-1 in a check whose run is still stopping (task checks' drain waits for
/// [`release_checks`]), and `busy` nodes in all.
fn checking_window(
    fixture: &Fixture,
    runs: &[&str],
    busy: usize,
    cx: &mut TestAppContext,
) -> Rc<RefCell<FakeState>> {
    let state = Rc::new(RefCell::new(FakeState {
        busy,
        runs: runs.iter().map(ToString::to_string).collect(),
        checking: vec![("run-1".to_string(), check_key())],
        stopped_checks: vec![("run-1".to_string(), check_key())],
        hold_checks: true,
        ..FakeState::default()
    }));
    window_from(fixture, state, LONG, cx)
}

// "Cancel" chosen up front while task checks still stop a check: the window keeps the writer
// only once their drain resolved and the check's node got `ChecksDrained`, and undrains task
// checks after that, so the node starts again from its kept parts instead of taking its
// cancelled run as its result.
#[gpui::test]
async fn a_cancel_while_a_check_stops_records_checks_drained_before_undraining_checks(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = checking_window(&fixture, &["run-1"], 1, cx);
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Cancel,
                    ask_again: None,
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    cx.run_until_parked();
    assert_eq!(log(&state), vec!["live", "drain", "drain checks"]);
    assert!(taken.try_recv().is_err(), "the window has not answered yet");

    release_checks(&state);
    let Err(refused) = wait_for_writer(&taken, cx) else {
        panic!("the host took the writer of a cancelled handover");
    };
    assert!(refused.contains(crate::CANCELLED), "{refused}");
    assert!(watch.done.await.unwrap().is_err());
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            format!("checks drained run-1 {}", check_key()),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    assert!(!writer_of(&fixture).is_released());
}

// Asked again past the timeout, the person cancels while a check is still stopping: as with a
// cancel up front, the check's node gets `ChecksDrained` before task checks are undrained.
#[gpui::test]
async fn a_cancel_asked_again_while_a_check_stops_records_checks_drained_first(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = checking_window(&fixture, &["run-1", "run-2"], 2, cx);
    let asked = Rc::new(RefCell::new(Vec::new()));
    let prompt_asked = asked.clone();
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Wait,
                    ask_again: Some(Rc::new(move |busy, _cx| {
                        prompt_asked.borrow_mut().push(busy);
                        Task::ready(DrainChoice::Cancel)
                    })),
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    cx.executor().advance_clock(HANDOVER_TIMEOUT);
    cx.run_until_parked();
    assert_eq!(*asked.borrow(), vec![2]);
    assert_eq!(
        log(&state),
        vec!["live", "drain", "drain checks"],
        "nothing undrained while the check stops"
    );
    assert!(taken.try_recv().is_err(), "the window has not answered yet");

    release_checks(&state);
    let Err(refused) = wait_for_writer(&taken, cx) else {
        panic!("the host took the writer of a cancelled handover");
    };
    assert!(refused.contains(crate::CANCELLED), "{refused}");
    assert!(watch.done.await.unwrap().is_err());
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            format!("checks drained run-1 {}", check_key()),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
}

// "Interrupt them now" chosen up front while run-1's only node in flight is a stopping check:
// the check's node gets `ChecksDrained` before the runs are interrupted, so run-1 is parked, not
// recorded interrupted.
#[gpui::test]
async fn interrupt_them_now_waits_for_the_stopped_checks_to_be_recorded_first(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = checking_window(&fixture, &["run-1"], 1, cx);
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::InterruptNow,
                    ask_again: None,
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    cx.run_until_parked();
    assert_eq!(
        log(&state),
        vec!["live", "drain", "drain checks"],
        "not interrupted before the stopped check is recorded"
    );

    release_checks(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            format!("checks drained run-1 {}", check_key()),
            "interrupt [\"run-1\"]".to_string(),
            "live".to_string(),
            format!("hand over [\"run-1\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    assert!(watch.done.await.unwrap().is_ok());
}

// With no participant and no check running, the drain is the runs' alone: the writer moves once
// the running node finishes, with nothing recorded for checks.
#[gpui::test]
async fn with_no_participant_and_no_check_the_drain_is_the_runs_alone(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let state = window(&fixture, &["run-1"], 1, LONG, cx);
    let taken = ask_for_writer(&fixture, cx);
    wait_for(cx, |_| log(&state).contains(&"drain checks".to_string()));
    assert!(
        taken.try_recv().is_err(),
        "the host waits on the runs' drain"
    );
    finish_nodes(&state);
    let writer = wait_for_writer(&taken, cx).unwrap();
    let pid = std::process::id();
    assert_eq!(
        log(&state),
        vec![
            "live".to_string(),
            "drain".to_string(),
            "drain checks".to_string(),
            "live".to_string(),
            format!("hand over [\"run-1\"] to {pid}"),
            "undrain checks".to_string(),
            "undrain".to_string(),
        ]
    );
    let (_host, _) = serve_host(&fixture, writer, host_cx);
    wait_for(cx, |cx| is_remote(&fixture, cx));
}

/// Stands in for the system's answer to whether a process runs: every process runs until
/// [`Self::kill`]; counts the questions.
#[derive(Clone, Default)]
struct FakeLiveness {
    killed: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

impl FakeLiveness {
    fn install(&self, cx: &mut TestAppContext) {
        let this = self.clone();
        cx.update(|cx| {
            crate::set_process_check(
                Arc::new(move |_, _| {
                    this.asked.fetch_add(1, Ordering::SeqCst);
                    !this.killed.load(Ordering::SeqCst)
                }),
                cx,
            )
        });
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
    }

    fn revive(&self) {
        self.killed.store(false, Ordering::SeqCst);
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

/// A host that asks the window for the writer and never waits on the lock: the connection and
/// the request, which resolves once the window released the writer.
fn ask_and_never_lock(
    fixture: &Fixture,
    cx: &TestAppContext,
) -> (HostClient, Task<Result<serde_json::Value>>) {
    let socket = live_host(fixture.location()).unwrap().socket.unwrap();
    let asking = HostClient::connect(
        &socket,
        fixture.client_options(ClientKind::Cli),
        cx.executor(),
    )
    .unwrap();
    let released =
        cx.background_spawn(asking.request_unbounded(HostCommand::Host(crate::HostCtl::Handover)));
    (asking, released)
}

/// Exactly one writer, in the window: it holds the lock (another open is refused), its writer
/// is not released, and `host.json` names it with no move.
fn assert_one_writer_in_the_window(fixture: &Fixture) {
    assert!(!writer_of(fixture).is_released());
    let hooks = StateApplyHooks::new();
    assert!(matches!(
        StateStore::open_writer_with_hooks(fixture.location(), &hooks),
        Err(OpenError::HeldBy { .. })
    ));
    let info = live_host(fixture.location()).unwrap();
    assert_eq!((info.mode, info.moving), (HostMode::Window, None));
}

/// The window with live run-1 and the participants `names` (registered in that order, those in
/// `failing` failing to undrain), whose handovers ask the returned liveness.
fn window_with_participants(
    fixture: &Fixture,
    names: &[&'static str],
    failing: &[&'static str],
    cx: &mut TestAppContext,
) -> (Rc<RefCell<FakeState>>, FakeLiveness) {
    let state = Rc::new(RefCell::new(FakeState {
        runs: vec!["run-1".to_string()],
        ..FakeState::default()
    }));
    let participants = Rc::new(RefCell::new(Vec::new()));
    for name in names {
        FakeParticipant::register_with(
            name,
            &participants,
            Some(&state),
            None,
            failing.contains(name),
            cx,
        );
    }
    let state = window_from(fixture, state, LONG, cx);
    let liveness = FakeLiveness::default();
    liveness.install(cx);
    (state, liveness)
}

/// One handover to a host that asks, never takes the lock and is then killed while `moving`
/// names it. Checks the window took the writer back and said the host did not start; returns
/// what the window logged from the kill on.
async fn kill_the_host_during_the_move(
    fixture: &Fixture,
    state: &Rc<RefCell<FakeState>>,
    liveness: &FakeLiveness,
    cx: &mut TestAppContext,
) -> Vec<String> {
    liveness.revive();
    let id = fixture.location().id.clone();
    let watch = cx
        .update(|cx| expect_handover(&id, wait_request(), cx))
        .unwrap();
    let (_asking, released) = ask_and_never_lock(fixture, cx);
    released.await.unwrap();
    assert!(writer_of(fixture).is_released());
    let moving = read_host_file(fixture.location()).unwrap().moving;
    assert_eq!(moving.map(|m| m.to_pid), Some(std::process::id()));
    let before = log(state).len();

    liveness.kill();
    let Err(error) = watch.done.await.unwrap() else {
        panic!("the window attached to a host that was killed");
    };
    assert_eq!(error, did_not_start(&fixture.log()));
    assert_one_writer_in_the_window(fixture);
    assert!(!state.borrow().refusing, "runs start here again");
    log(state)[before..].to_vec()
}

/// What a take-back logs: the participants `undrained` in that order, task checks, the runs,
/// the participants `started` in that order, then run-1 loads again.
fn take_back_order(id: &str, undrained: &[&str], started: &[&str]) -> Vec<String> {
    let mut order: Vec<String> = undrained
        .iter()
        .map(|name| format!("{name} undrain {id}"))
        .collect();
    order.push("undrain checks".to_string());
    order.push("undrain".to_string());
    order.extend(started.iter().map(|name| format!("{name} started {id}")));
    order.push("reload [\"run-1\"]".to_string());
    order
}

// The host is killed while `moving` names it: the window takes the writer back and undrains in
// the reverse of the drain: participant 2, participant 1, task checks (`ChecksEngine::undrain`),
// the runs (`Runtime::undrain`); then the participants start and the handed-over runs load
// again. A second handover killed the same way ends the same way, with one writer each time.
#[gpui::test]
async fn a_host_killed_during_the_move_gives_the_writer_back_undrained_in_reverse(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let (state, liveness) = window_with_participants(&fixture, &["p1", "p2"], &[], cx);
    for _ in 0..2 {
        let logged = kill_the_host_during_the_move(&fixture, &state, &liveness, cx).await;
        assert_eq!(logged, take_back_order(&id, &["p2", "p1"], &["p1", "p2"]));
    }
    // The writer works here again.
    write(&fixture.repo, None, "back", cx).await.unwrap();
}

// A single participant undrains, then starts again.
#[gpui::test]
async fn a_single_participant_undrains_then_starts_at_a_take_back(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let (state, liveness) = window_with_participants(&fixture, &["p1"], &[], cx);
    let logged = kill_the_host_during_the_move(&fixture, &state, &liveness, cx).await;
    assert_eq!(logged, take_back_order(&id, &["p1"], &["p1"]));
}

// A participant whose undrain fails does not stop the take-back: the others undrain, task checks
// and the runs undrain, every participant starts, and the window ends with the one writer.
#[gpui::test]
async fn a_failing_participant_undrain_does_not_stop_the_take_back(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let (state, liveness) = window_with_participants(&fixture, &["p1", "p2"], &["p2"], cx);
    let logged = kill_the_host_during_the_move(&fixture, &state, &liveness, cx).await;
    assert_eq!(logged, take_back_order(&id, &["p2", "p1"], &["p1", "p2"]));
}

// A third process holds the lock as the take-back begins, past the time the window logs it
// still waits: the window never gives up (that would leave no writer, `moving` set and the runs
// unloaded), keeps trying while it holds it, and takes the writer once it lets go.
#[gpui::test]
async fn a_take_back_waits_out_a_third_process_holding_the_lock(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let (state, liveness) = window_with_participants(&fixture, &["p1"], &[], cx);
    // Every failed try is past the warning.
    cx.update(|cx| crate::set_take_back_warn_every(Duration::ZERO, cx));
    let mut watch = cx
        .update(|cx| expect_handover(&id, wait_request(), cx))
        .unwrap();
    let (_asking, released) = ask_and_never_lock(&fixture, cx);
    released.await.unwrap();
    let hooks = StateApplyHooks::new();
    let third = StateStore::open_writer_with_hooks(fixture.location(), &hooks).unwrap();

    let asked = liveness.asked();
    liveness.kill();
    // The window looked at the host at least three more times and found it gone each time.
    wait_for(cx, |_| liveness.asked() >= asked + 3);
    assert!(
        writer_of(&fixture).is_released(),
        "the third process holds it"
    );
    assert!(matches!(watch.done.try_recv(), Ok(None)), "still trying");
    assert!(!log(&state).contains(&"reload [\"run-1\"]".to_string()));
    assert!(
        read_host_file(fixture.location()).unwrap().moving.is_some(),
        "the move is not over until the writer is back"
    );

    drop(third);
    let Err(error) = watch.done.await.unwrap() else {
        panic!("the window attached to a host that was killed");
    };
    assert_eq!(error, did_not_start(&fixture.log()));
    assert_one_writer_in_the_window(&fixture);
    let logged = log(&state);
    assert_eq!(
        logged[logged.len() - 5..],
        take_back_order(&id, &["p1"], &["p1"])
    );
}

// A cancelled handover undrains the participants too, in reverse, before task checks and the
// runs, and starts them again: the writer never left.
#[gpui::test]
async fn a_cancel_undrains_the_participants_in_reverse_then_checks_then_runs(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let id = fixture.location().id.clone();
    let (state, _liveness) = window_with_participants(&fixture, &["p1", "p2"], &[], cx);
    let watch = cx
        .update(|cx| {
            expect_handover(
                &id,
                HandoverRequest {
                    choice: DrainChoice::Cancel,
                    ask_again: None,
                },
                cx,
            )
        })
        .unwrap();
    let taken = ask_for_writer(&fixture, cx);
    assert!(wait_for_writer(&taken, cx).is_err());
    assert!(watch.done.await.unwrap().is_err());
    let logged = log(&state);
    let at = logged
        .iter()
        .position(|line| line == "drain checks")
        .unwrap();
    let mut expected = take_back_order(&id, &["p2", "p1"], &["p1", "p2"]);
    expected.pop();
    assert_eq!(logged[at + 1..], expected);
    assert_one_writer_in_the_window(&fixture);
}

// A move whose writer and target both ended (pid and start time checked) is stale: the next
// opener clears it from `host.json` and keeps the writer. A live target, or a live writer, makes
// it not stale: the opener lets the lock go and `moving` stays.
#[test]
fn a_stale_move_is_cleared_by_the_next_opener_and_a_live_one_is_not() {
    let fixture = fixture();
    let location = fixture.location().clone();
    futures::executor::block_on(fixture.repo.release_writer(LONG)).unwrap();
    let (window_pid, host_pid) = (dead_pid(10), dead_pid(11));
    let record = || {
        write_host_file(
            &location,
            &HostInfo {
                pid: window_pid,
                start_time: Some(7),
                socket: None,
                bongos_version: VERSION.to_string(),
                mode: HostMode::Window,
                started: chrono::Utc::now(),
                moving: Some(Moving {
                    to_pid: host_pid,
                    to_start_time: Some(9),
                }),
            },
        )
        .unwrap();
    };
    let hooks = StateApplyHooks::new();
    for live in [
        vec![(host_pid, Some(9))],
        vec![(window_pid, Some(7))],
        vec![(host_pid, Some(9)), (window_pid, Some(7))],
    ] {
        record();
        let alive = |pid: u32, start: Option<u64>| live.contains(&(pid, start));
        let won = crate::take_writer_checked_by(&location, &hooks, 1, &alive).unwrap();
        assert!(won.is_none(), "{live:?} live: the opener lets the lock go");
        assert!(read_host_file(&location).unwrap().moving.is_some());
    }
    // Both pids run again, but as other processes (other start times): still stale.
    record();
    let reused = |_: u32, start: Option<u64>| start == Some(1);
    let kept = crate::take_writer_checked_by(&location, &hooks, 1, &reused).unwrap();
    assert!(kept.is_some(), "a stale move does not hold the opener off");
    assert_eq!(read_host_file(&location).unwrap().moving, None);
}
