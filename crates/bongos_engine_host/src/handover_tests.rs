//! The handover through real sockets and a real state store in a temporary folder: the window is
//! one test app holding the writer, the engine host's side runs on a thread of its own (asking
//! for the writer and waiting on the lock as the host process does) and then serves the project
//! from a second test app. The window's runs are a recording fake.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use bongos_project_repo::{
    ClientKind, Location, OpenError, ProjectRepo, StateApplyHooks, StateHandle, StateOp,
    StateStore, StateWriter,
};
use bongos_project_repo_model::Author;
use futures::channel::oneshot;
use gpui::{App, AppContext as _, Task, TestAppContext};

use crate::engine_host_tests::{Fixture, VERSION, branch_of, fixture, set_branch, wait_for};
use crate::{
    ClientOptions, DrainChoice, EngineAccess, EngineAccesses, HandoverRequest, HandoverRuns,
    HandoverTimes, HostClient, HostCommand, HostInfo, HostMode, MOVING_REFUSAL, MOVING_RETRY,
    Moving, OpenedState, WriterHost, WriterHosts, WriterOptions, did_not_start, expect_handover,
    install_handovers, live_host, open_state_with_hooks, read_host_file, take_writer_checked,
    take_writer_from_window, write_host_file,
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
}

/// The window's runs: `runs` are live, `busy` of them run a node until [`finish_nodes`].
struct FakeRuns {
    state: Rc<RefCell<FakeState>>,
    runs: Vec<String>,
    location: Location,
    writer: Arc<StateWriter>,
}

impl HandoverRuns for FakeRuns {
    fn live_runs(&self, _repo: &ProjectRepo, _cx: &mut App) -> Result<Vec<String>> {
        self.state.borrow_mut().log.push("live".to_string());
        Ok(self.runs.clone())
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

    fn reload(&self, _cx: &mut App) {
        self.state.borrow_mut().log.push("reload".to_string());
    }
}

fn finish_nodes(state: &Rc<RefCell<FakeState>>) {
    let mut state = state.borrow_mut();
    state.busy = 0;
    if let Some(drain) = state.drain.take() {
        drain.send(()).ok();
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
        ..FakeState::default()
    }));
    let fake = FakeRuns {
        state: state.clone(),
        runs: runs.iter().map(ToString::to_string).collect(),
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
    cx.update(|cx| EngineAccesses::get(&id, cx)).is_some_and(|access| !access.is_local())
}

/// Moves the branch of task `l1` from `from` to `to` (a compare-and-set, so a write that does
/// not land in order is a conflict that changes nothing).
fn write(repo: &ProjectRepo, from: Option<&str>, to: &str, cx: &TestAppContext) -> Task<Result<()>> {
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
    write(&fixture.repo, Some("before"), "during", cx).await.unwrap();
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
            format!("hand over [\"run-1\"] to {pid}"),
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
    write(&fixture.repo, Some("queued"), "later", cx).await.unwrap();
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
            format!("hand over [] to {pid}"),
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
            "interrupt [\"run-1\"]".to_string(),
            format!("hand over [\"run-1\"] to {pid}"),
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
    assert_eq!(log(&state), vec!["live", "drain", "undrain"]);
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
    let released = cx.background_spawn(
        asking.request_unbounded(HostCommand::Host(crate::HostCtl::Handover)),
    );
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
    assert!(log(&state).contains(&"reload".to_string()));
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
    write(&fixture.repo, Some("queued"), "back", cx).await.unwrap();
    assert!(fixture.repo.sink().is_none());
}

fn options_as(fixture: &Fixture, own_pid: u32) -> ClientOptions {
    let mut options = fixture.client_options(ClientKind::Window);
    options.own_pid = own_pid;
    options.moving_timeout = LONG;
    options
}

/// A pid that ran and ended.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
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
    assert!(opened.try_recv().is_err(), "it waits while the move is recorded");
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
    let dead_window = dead_pid();
    let dead_host = dead_pid();
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
