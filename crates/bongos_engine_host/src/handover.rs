//! The handover: a window that holds a project's writer gives it to a new engine host process.
//!
//! The host connects to the window's socket and asks for the writer (`Host(Handover)`), then
//! waits on the writer lock with a blocking `flock`. The window, in order:
//! 1. answers every other request with [`MOVING_REFUSAL`] (clients try again);
//! 2. drains the workflow engine and task checks together: no new node starts, the nodes
//!    running finish and are recorded; runs with no process (a gate, a sleeping poll) are handed
//!    over as they are. Task checks stop their check runs; each workflow checks node whose check
//!    was stopped gets `ChecksDrained` and counts as parked, so a long check never delays the
//!    drain, and the run starts that key again from its kept parts in the host. When a node still
//!    runs after `lanes.host.handover_timeout_seconds`, the person is asked again whether to wait
//!    or to interrupt the runs now (recorded interrupted, as at quit). Once both drains resolved,
//!    the writer participants ([`WriterParticipants`]) drain one after the other, in
//!    registration order, so none drains while a node process runs;
//! 3. writes `host.json` with `moving: { to_pid }`, appends `HandedOver` to every live run of the
//!    project and unloads them (it never writes to them again), sends its own later writes to a
//!    queue, tells its clients the writer moves, stops taking clients, removes the socket,
//!    releases the writer ([`ProjectRepo::release_writer`]) and answers `released`;
//! 4. the host takes the lock, writes `host.json` (mode host, no move), serves the socket and
//!    loads the runs, which continue from their checkpoints;
//! 5. the window connects to the host as a client, sends its writes there (the queued ones
//!    first, in order) and reaches the engines through it from then on.
//!
//! When the host never takes the lock (it died, or the start timeout passed with the lock free),
//! the window takes the writer back: `moving` is cleared, the lock taken again, the queued writes
//! applied here, the runs loaded again, and the person told "the engine host did not start".
//!
//! While `moving` is set no other process tries the lock: an opener waits for the move to end
//! and attaches to the new writer. One that read `host.json` before `moving` was written and
//! then won the lock reads it again right after ([`take_writer_checked`]) and lets the lock go.
//!
//! A cancelled handover undrains the runs and task checks here, only once task checks' drain
//! resolved and each node whose check it stopped got `ChecksDrained` (so that node starts again
//! from its kept parts); "interrupt them now" waits for that too. Undraining the writer
//! participants on a take-back belongs with the take-back.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use bongos_checks::ChecksEngine;
use bongos_project_repo::{
    LocalSink, Location, OpenError, PeerIdentity, ProjectRepo, RepoCommand, RepoReply,
    StateApplyHooks, StateEvent, StateOp, StateSink, StateStore, StateWriter,
};
use bongos_project_repo_model::Author;
use bongos_workflow::WorkflowEngine;
use bongos_workflow_model::NodeKey;
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Global, Task};
use parking_lot::Mutex;
use serde_json::json;

use crate::access::{EngineAccess, EngineAccesses};
use crate::client::{ClientOptions, HostClient};
use crate::ensure::{HostSpawner, POLL_INTERVAL};
use crate::host_file::{HostInfo, Moving, move_in_progress, process_is, read_host_file};
use crate::participants::WriterParticipants;
use crate::protocol::{HostCommand, HostCtl, HostEvent, HostTopic};
use crate::window::WriterHosts;

pub use crate::client::MOVING_REFUSAL;

/// What the person chose for the runs that still run a node when the writer moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainChoice {
    /// Let the nodes finish here first.
    Wait,
    /// Stop their processes now; the runs are recorded interrupted, as at quit, and wait on the
    /// person in the engine host.
    InterruptNow,
    /// Keep the writer here.
    Cancel,
}

/// Asks the person again, with the number of runs still running a node, after the drain ran
/// past its timeout.
pub type DrainPrompt = Rc<dyn Fn(usize, &mut App) -> Task<DrainChoice>>;

/// What a window asked for when it started the handover itself (`lanes: start`).
#[derive(Clone)]
pub struct HandoverRequest {
    /// What the person chose up front for the runs still running a node.
    pub choice: DrainChoice,
    /// Asks again when the drain runs past its timeout; `None` keeps waiting.
    pub ask_again: Option<DrainPrompt>,
}

/// What the window says when the person cancelled the handover.
pub const CANCELLED: &str = "the person kept the writer in the window; the runs stay there";

/// What the window says after taking the writer back.
pub fn did_not_start(log: &Path) -> String {
    format!("the engine host did not start; see {}", log.display())
}

/// The runs of the project being handed over, as this process's engine holds them.
pub trait HandoverRuns {
    /// The live runs of `repo`'s project. Refused when the runs cannot all be known yet.
    fn live_runs(&self, repo: &ProjectRepo, cx: &mut App) -> Result<Vec<String>>;
    /// How many of `runs` run a node now.
    fn in_flight(&self, runs: &[String], cx: &App) -> usize;
    /// Starts no new node; resolves once no node process runs.
    fn drain(&self, cx: &mut App) -> oneshot::Receiver<()>;
    /// Lets nodes start again.
    fn undrain(&self, cx: &mut App);
    /// Task checks' drain: stops every check run and resolves with the workflow nodes, as
    /// `(run, key)`, whose checks it stopped.
    fn drain_checks(&self, cx: &mut App) -> Task<Vec<(String, NodeKey)>>;
    /// Task checks take runs again.
    fn undrain_checks(&self, cx: &mut App);
    /// Records `ChecksDrained { key }` in `run`: the key counts as parked and the run starts it
    /// again from its kept parts wherever it is loaded next.
    fn checks_drained(&self, run: &str, key: &NodeKey, cx: &mut App);
    /// Stops the processes of `runs` and records them interrupted.
    fn interrupt(&self, runs: &[String], cx: &mut App);
    /// `HandedOver { to_pid }` to each of `runs`, then unloads them.
    fn hand_over(&self, to_pid: u32, runs: &[String], cx: &mut App);
    /// Loads again those of `runs` on disk this process does not hold (a take-back).
    fn reload(&self, runs: &[String], cx: &mut App);
    /// While `on`, no run starts under `files` (a project's files) here.
    fn refuse_starts(&self, files: &Path, on: bool, cx: &mut App);
}

/// [`HandoverRuns`] of the app's workflow engine.
pub struct EngineRuns;

impl HandoverRuns for EngineRuns {
    fn live_runs(&self, repo: &ProjectRepo, cx: &mut App) -> Result<Vec<String>> {
        let Some(engine) = WorkflowEngine::ensure(cx) else {
            return Ok(Vec::new());
        };
        if !engine.runs_loaded(cx) {
            bail!("this window has not loaded its runs yet; try again in a moment");
        }
        Ok(engine.live_runs_under(&repo.location().files))
    }

    fn in_flight(&self, runs: &[String], cx: &App) -> usize {
        cx.try_global::<WorkflowEngine>()
            .map_or(0, |engine| engine.runs_in_flight(runs))
    }

    fn drain(&self, cx: &mut App) -> oneshot::Receiver<()> {
        match cx.try_global::<WorkflowEngine>() {
            Some(engine) => engine.drain(),
            None => {
                let (sender, receiver) = oneshot::channel();
                sender.send(()).ok();
                receiver
            }
        }
    }

    fn undrain(&self, cx: &mut App) {
        if let Some(engine) = cx.try_global::<WorkflowEngine>() {
            engine.undrain();
        }
    }

    fn drain_checks(&self, cx: &mut App) -> Task<Vec<(String, NodeKey)>> {
        match ChecksEngine::global(cx) {
            Some(engine) => engine.update(cx, |engine, cx| engine.drain(cx)),
            None => Task::ready(Vec::new()),
        }
    }

    fn undrain_checks(&self, cx: &mut App) {
        if let Some(engine) = ChecksEngine::global(cx) {
            engine.update(cx, |engine, cx| engine.undrain(cx));
        }
    }

    fn checks_drained(&self, run: &str, key: &NodeKey, cx: &mut App) {
        let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() else {
            return;
        };
        if let Err(error) = engine.checks_drained(run, key, cx) {
            // An interrupted run, or one no longer loaded: its key is not in flight here.
            log::warn!("recording the drained check {key} of run {run}: {error}");
        }
    }

    fn interrupt(&self, runs: &[String], cx: &mut App) {
        if let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() {
            engine.interrupt_runs(runs, cx);
        }
    }

    fn hand_over(&self, to_pid: u32, runs: &[String], cx: &mut App) {
        if let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() {
            engine.hand_over_runs(to_pid, runs, cx);
        }
    }

    fn reload(&self, runs: &[String], cx: &mut App) {
        if let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() {
            engine.reload_runs(runs, cx);
        }
    }

    fn refuse_starts(&self, files: &Path, on: bool, cx: &mut App) {
        if let Some(engine) = cx.try_global::<WorkflowEngine>() {
            engine.refuse_starts_under(files, on);
        }
    }
}

/// The times of a handover, read from the lanes settings when it starts.
#[derive(Clone, Copy, Debug)]
pub struct HandoverTimes {
    /// `lanes.host.start_timeout_seconds`: how long the window waits for the host to take the
    /// writer before it takes it back, and how long clients try again.
    pub start_timeout: Duration,
    /// `lanes.host.handover_timeout_seconds`: how long the drain waits before asking again.
    pub handover_timeout: Duration,
}

/// How this window takes part in handovers.
struct Handovers {
    runs: Rc<dyn HandoverRuns>,
    times: Rc<dyn Fn(&App) -> HandoverTimes>,
    client: ClientOptions,
    /// The projects whose writer is moving away now, with their files (no run starts under them
    /// until the move ends).
    moving: HashMap<String, PathBuf>,
    /// What `lanes: start` asked for, by project, until the host asks for the writer.
    requests: HashMap<String, HandoverRequest>,
    /// Told when the host asked for the writer.
    began: HashMap<String, Vec<oneshot::Sender<()>>>,
    /// Told how the handover ended: the client of the new writer, or why the writer stayed.
    done: HashMap<String, Vec<oneshot::Sender<Result<HostClient, String>>>>,
    /// The engine hosts `lanes: start` gave up on (they did not ask in time), by pid: a request
    /// they make late is refused, since the window already told the person they did not start.
    /// Keyed by pid with the process's start time, so a later process given the same pid is not
    /// refused; entries whose process ended are dropped.
    abandoned: HashMap<u32, Option<u64>>,
}

impl Handovers {
    /// Records that `lanes: start` gave up on engine host `pid`, dropping the hosts that ended.
    fn abandon(&mut self, pid: u32) {
        self.abandoned
            .retain(|pid, start_time| process_is(*pid, *start_time));
        self.abandoned
            .insert(pid, bongos_workflow::process::start_time(pid));
    }

    /// Whether `peer` is a host `lanes: start` gave up on; it is refused once.
    fn take_abandoned(&mut self, pid: u32) -> bool {
        self.abandoned
            .remove(&pid)
            .is_some_and(|start_time| process_is(pid, start_time))
    }
}

/// The pids of the engine hosts `lanes: start` gave up on, for tests.
#[cfg(test)]
pub(crate) fn abandoned_hosts(cx: &App) -> Vec<u32> {
    cx.try_global::<Handovers>()
        .map(|handovers| handovers.abandoned.keys().copied().collect())
        .unwrap_or_default()
}

impl Global for Handovers {}

/// Makes this process hand its writers over when an engine host asks: `runs` are the runs it
/// moves, `times` reads the timeouts, `client` is how it then connects to the new writer.
pub fn install_handovers(
    runs: Rc<dyn HandoverRuns>,
    times: Rc<dyn Fn(&App) -> HandoverTimes>,
    client: ClientOptions,
    cx: &mut App,
) {
    cx.set_global(Handovers {
        runs,
        times,
        client,
        moving: HashMap::new(),
        requests: HashMap::new(),
        began: HashMap::new(),
        done: HashMap::new(),
        abandoned: HashMap::new(),
    });
}

/// Whether the writer of `project_id` is moving away from this process now.
pub fn is_moving(project_id: &str, cx: &App) -> bool {
    cx.try_global::<Handovers>()
        .is_some_and(|handovers| handovers.moving.contains_key(project_id))
}

fn finish(project_id: &str, outcome: Result<HostClient, String>, cx: &mut App) {
    if !cx.has_global::<Handovers>() {
        return;
    }
    let handovers = cx.global_mut::<Handovers>();
    let files = handovers.moving.remove(project_id);
    handovers.requests.remove(project_id);
    handovers.began.remove(project_id);
    let watchers = handovers.done.remove(project_id).unwrap_or_default();
    let runs = handovers.runs.clone();
    if let Some(files) = files {
        runs.refuse_starts(&files, false, cx);
    }
    for watcher in watchers {
        watcher.send(outcome.clone()).ok();
    }
}

/// Runs `f` on this process's [`crate::WriterHost`] of `project_id`.
fn with_writer_host<T>(
    project_id: &str,
    cx: &mut App,
    f: impl FnOnce(&mut crate::WriterHost, &mut App) -> Result<T>,
) -> Result<T> {
    let mut hosts = std::mem::take(&mut cx.default_global::<WriterHosts>().0);
    let result = match hosts
        .iter_mut()
        .find(|host| host.repo().location().id == project_id)
    {
        Some(host) => f(host, cx),
        None => Err(anyhow!("this window does not announce {project_id}")),
    };
    let mut current = std::mem::take(&mut cx.default_global::<WriterHosts>().0);
    hosts.append(&mut current);
    cx.default_global::<WriterHosts>().0 = hosts;
    result
}

/// Serves `Host(Handover)` from `peer`: hands the writer of `repo` over to it. Answers
/// `{"released": true}` once the writer is released; the switch to the new writer then goes on
/// by itself.
pub(crate) fn serve_handover(
    repo: &ProjectRepo,
    peer: &PeerIdentity,
    cx: &mut App,
) -> Task<Result<serde_json::Value>> {
    let id = repo.location().id.clone();
    if cx.has_global::<Handovers>() && cx.global_mut::<Handovers>().take_abandoned(peer.pid()) {
        return Task::ready(Err(anyhow!(
            "this window gave up on engine host pid {} before it asked for the writer",
            peer.pid()
        )));
    }
    let Some(handovers) = cx.try_global::<Handovers>() else {
        return Task::ready(Err(anyhow!(
            "this engine host does not hand its writer over"
        )));
    };
    if !peer.same_executable() {
        return Task::ready(Err(anyhow!(
            "only an engine host of this bongos takes the writer over"
        )));
    }
    if handovers.moving.contains_key(&id) {
        return Task::ready(Err(anyhow!(MOVING_REFUSAL)));
    }

    if repo.state().and_then(|state| state.writer()).is_none() {
        return Task::ready(Err(anyhow!(
            "this process does not hold the writer of {id}"
        )));
    }
    let runs_source = handovers.runs.clone();
    let times = (handovers.times)(cx);
    let client = handovers.client.clone();
    let runs = match runs_source.live_runs(repo, cx) {
        Ok(runs) => runs,
        Err(error) => return Task::ready(Err(error)),
    };
    let to = Moving::to(peer.pid());
    let files = repo.location().files.clone();
    // From here until the move ends no run of the project starts here: the host loads the
    // project's runs from disk, so a run begun here now would run in both processes.
    runs_source.refuse_starts(&files, true, cx);
    let handovers = cx.global_mut::<Handovers>();
    let request = handovers.requests.remove(&id);
    handovers.moving.insert(id.clone(), files);
    for watcher in handovers.began.remove(&id).unwrap_or_default() {
        watcher.send(()).ok();
    }
    log::info!(
        "handing the writer of {id} over to pid {} ({} live runs)",
        to.to_pid,
        runs.len()
    );
    let move_ = Move {
        id,
        repo: repo.clone(),
        to,
        runs,
        handed: Rc::new(RefCell::new(Vec::new())),
        runs_source,
        times,
        client,
        queue: Arc::new(QueueSink::new(cx.background_executor().clone())),
    };
    cx.spawn(async move |cx| {
        let mut moving_written = false;
        match move_.release(request, &mut moving_written, cx).await {
            Ok(()) => {
                move_.switch(cx);
                Ok(json!({ "released": true }))
            }
            Err(error) => {
                log::warn!("the handover of {} stopped: {error:#}", move_.id);
                cx.update(|cx| move_.keep(moving_written, &error, cx));
                Err(error)
            }
        }
    })
}

/// One handover in progress.
#[derive(Clone)]
struct Move {
    id: String,
    repo: ProjectRepo,
    to: Moving,
    /// The live runs when the host asked; those it waits on and interrupts.
    runs: Vec<String>,
    /// The runs handed over (`HandedOver` appended, unloaded): what a take-back loads again.
    handed: Rc<RefCell<Vec<String>>>,
    runs_source: Rc<dyn HandoverRuns>,
    times: HandoverTimes,
    client: ClientOptions,
    queue: Arc<QueueSink>,
}

impl Move {
    /// Steps 2 and 3: drain, `moving`, `HandedOver`, the queue, stop serving, release.
    async fn release(
        &self,
        request: Option<HandoverRequest>,
        moving_written: &mut bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        self.drain(request, cx).await?;
        cx.update(|cx| -> Result<()> {
            // `moving` is on disk before any run is handed over or the lock let go, so a crash
            // from here on is a move in progress to the next opener.
            with_writer_host(&self.id, cx, |host, _| {
                host.set_moving(Some(self.to.clone()))
            })?;
            *moving_written = true;
            // The live runs again, now: a run that was not live yet when the host asked (loaded
            // since, or restarted before the move refused it) moves too.
            let mut runs = self.runs.clone();
            match self.runs_source.live_runs(&self.repo, cx) {
                Ok(live) => {
                    for run in live {
                        if !runs.contains(&run) {
                            runs.push(run);
                        }
                    }
                }
                Err(error) => log::error!("reading the live runs of {}: {error:#}", self.id),
            }
            self.runs_source.hand_over(self.to.to_pid, &runs, cx);
            *self.handed.borrow_mut() = runs;
            self.repo.replace_sink(self.queue.clone());
            with_writer_host(&self.id, cx, |host, _| {
                if let Some(server) = host.server() {
                    server.broadcast(&HostEvent::WriterMoving {
                        to_pid: self.to.to_pid,
                    });
                }
                host.stop_accepting();
                Ok(())
            })
        })?;
        let repo = self.repo.clone();
        let queue_wait = self.times.start_timeout;
        cx.background_spawn(async move { repo.release_writer(queue_wait).await })
            .await?;
        // The runs and checks of other projects, if any, go on here.
        cx.update(|cx| {
            self.runs_source.undrain(cx);
            self.runs_source.undrain_checks(cx);
        });
        log::info!(
            "released the writer of {} to pid {}",
            self.id,
            self.to.to_pid
        );
        Ok(())
    }

    /// Step 2: no new node starts and task checks stop, together; waits for the running nodes,
    /// asking again past the timeout; then drains the writer participants in order.
    async fn drain(&self, request: Option<HandoverRequest>, cx: &mut AsyncApp) -> Result<()> {
        self.drain_runs_and_checks(request, cx).await?;
        // Only now, with no node process running, the writer participants, one after the other.
        cx.update(|cx| WriterParticipants::drain(&self.id, cx))
            .await;
        Ok(())
    }

    /// The runs' drain and task checks' drain, started together. Each workflow node whose check
    /// was stopped gets `ChecksDrained` (exactly those task checks name), which parks it, so the
    /// runs' drain then waits only on the other nodes.
    ///
    /// Every way out waits for task checks' drain to resolve and its nodes to be recorded first:
    /// a cancel (then the checks are undrained, and a stopped check whose run ended after that
    /// would no longer count as drained, so its cancelled report would become the node's
    /// result), and "interrupt them now" (a run whose only node in flight is a check is parked,
    /// not recorded interrupted).
    async fn drain_runs_and_checks(
        &self,
        request: Option<HandoverRequest>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (runs_drained, checks_drained) = cx.update(|cx| {
            (
                self.runs_source.drain(cx),
                self.runs_source.drain_checks(cx),
            )
        });
        let runs_source = self.runs_source.clone();
        let recording = cx.clone();
        let checks = async move {
            let stopped = checks_drained.await;
            recording.update(|cx| {
                for (run, key) in &stopped {
                    runs_source.checks_drained(run, key, cx);
                }
            });
        }
        .boxed_local()
        .shared();
        let mut drained = {
            let checks = checks.clone();
            async move {
                checks.await;
                runs_drained.await
            }
        }
        .boxed_local()
        .fuse();
        let mut choice = request
            .as_ref()
            .map_or(DrainChoice::Wait, |request| request.choice);
        loop {
            match choice {
                DrainChoice::Cancel => {
                    checks.await;
                    bail!(CANCELLED);
                }
                DrainChoice::InterruptNow => {
                    checks.clone().await;
                    cx.update(|cx| self.runs_source.interrupt(&self.runs, cx));
                    choice = DrainChoice::Wait;
                }
                DrainChoice::Wait => {
                    let timeout = cx
                        .background_executor()
                        .timer(self.times.handover_timeout)
                        .fuse();
                    futures::pin_mut!(timeout);
                    futures::select! {
                        result = drained => {
                            return result.map_err(|_| anyhow!("the drain was cancelled"));
                        }
                        _ = timeout => {
                            let busy = cx.update(|cx| self.runs_source.in_flight(&self.runs, cx));
                            log::info!(
                                "the handover of {} still waits on {busy} runs after {}s",
                                self.id,
                                self.times.handover_timeout.as_secs()
                            );
                            choice = match request.as_ref().and_then(|r| r.ask_again.clone()) {
                                Some(ask) => cx.update(|cx| ask(busy, cx)).await,
                                None => DrainChoice::Wait,
                            };
                        }
                    }
                }
            }
        }
    }

    /// A handover that stopped before the writer was released: the writer stays here. Once
    /// `moving` was written the runs were handed over too, so this is a take-back: they load
    /// again here and the queued writes land here.
    fn keep(&self, moving_written: bool, error: &anyhow::Error, cx: &mut App) {
        self.runs_source.undrain(cx);
        self.runs_source.undrain_checks(cx);
        if moving_written {
            if let Err(error) = self.repo.retake_writer() {
                log::error!("keeping the writer of {}: {error:#}", self.id);
            }
            self.take_back(&format!("{error:#}"), cx);
            return;
        }
        finish(&self.id, Err(format!("{error:#}")), cx);
    }

    /// Steps 4 and 5 from the window's side, after the release: waits for the new writer and
    /// attaches to it, or takes the writer back.
    fn switch(&self, cx: &mut AsyncApp) {
        let (sender, receiver) = oneshot::channel();
        let waiting = self.clone_for_thread(cx.background_executor().clone());
        let spawned = std::thread::Builder::new()
            .name("engine-host-handover".to_string())
            .spawn(move || {
                sender.send(waiting.wait()).ok();
            });
        if let Err(error) = spawned {
            log::error!("waiting for the engine host: {error}");
        }
        let this = self.clone();
        cx.spawn(async move |cx| {
            let next = receiver
                .await
                .unwrap_or_else(|_| NextWriter::Back("the wait for the host ended".to_string()));
            cx.update(|cx| match next {
                NextWriter::Took(client) => this.attach(client, cx),
                NextWriter::Back(reason) => this.take_back(&reason, cx),
            });
        })
        .detach();
    }

    fn clone_for_thread(&self, executor: BackgroundExecutor) -> Waiting {
        Waiting {
            location: self.repo.location().clone(),
            repo: self.repo.clone(),
            to: self.to.clone(),
            start_timeout: self.times.start_timeout,
            client: self.client.clone(),
            executor,
        }
    }

    /// Step 5: this window is a client of the new writer from now on.
    fn attach(&self, client: HostClient, cx: &mut App) {
        log::info!(
            "the writer of {} moved to pid {}",
            self.id,
            client.welcomed().writer_pid
        );
        // The writer moved for good: this process's backup timer ends.
        self.repo.stop_backups();
        // The views that followed this process's writer follow the new one.
        if let Some(writer) = self
            .repo
            .state()
            .and_then(|state| state.opened_writer())
            .cloned()
        {
            match client.subscribe(Some(vec![HostTopic::State])) {
                Ok(mut events) => cx
                    .background_spawn(async move {
                        while let Some(event) = events.next().await {
                            if let HostEvent::StateChanged {
                                generation,
                                task_ids,
                                namespaces,
                            } = event
                            {
                                writer.relay(StateEvent::Changed {
                                    generation,
                                    task_ids,
                                    namespaces,
                                });
                            }
                        }
                    })
                    .detach(),
                Err(error) => log::error!("following the engine host's changes: {error:#}"),
            }
        }
        self.queue
            .switch_to(Arc::new(client.clone()), cx.background_executor())
            .detach();
        let access = EngineAccess::Remote(client.clone());
        access.hook_up_checks(cx);
        EngineAccesses::set(self.id.clone(), access, cx);
        // The old server's last clients were told the writer moves; they reconnect.
        cx.default_global::<WriterHosts>()
            .0
            .retain(|host| host.repo().location().id != self.id);
        finish(&self.id, Ok(client), cx);
    }

    /// The host never took the writer: it comes back here and the runs continue here.
    fn take_back(&self, reason: &str, cx: &mut App) {
        log::warn!("taking the writer of {} back: {reason}", self.id);
        if let Err(error) = with_writer_host(&self.id, cx, |host, cx| host.take_back(cx)) {
            log::error!("announcing {} again: {error:#}", self.id);
        }
        if let Some(writer) = self.repo.state().and_then(|state| state.writer()).cloned() {
            let local = Arc::new(TakenBack(LocalSink::new(
                writer,
                cx.background_executor().clone(),
            )));
            let flushed = self.queue.switch_to(local, cx.background_executor());
            let repo = self.repo.clone();
            cx.background_spawn(async move {
                flushed.await;
                repo.clear_sink();
            })
            .detach();
        }
        let handed = self.handed.borrow().clone();
        self.runs_source.reload(&handed, cx);
        finish(&self.id, Err(did_not_start(&self.client.log)), cx);
    }
}

/// What the window waits on after the release, off the main thread.
struct Waiting {
    location: Location,
    repo: ProjectRepo,
    to: Moving,
    start_timeout: Duration,
    client: ClientOptions,
    executor: BackgroundExecutor,
}

enum NextWriter {
    Took(HostClient),
    Back(String),
}

impl Waiting {
    /// Waits until `host.json` names a live writer with no move and this window can connect to
    /// it; takes the writer back once the host is gone, or once the start timeout passed and
    /// the lock is free (a third process's momentary hold is waited out).
    fn wait(&self) -> NextWriter {
        let deadline = Instant::now() + self.start_timeout;
        loop {
            if let Some(info) = read_host_file(&self.location)
                && info.moving.is_none()
                && info.is_live()
                && let Some(socket) = &info.socket
            {
                match HostClient::connect(socket, self.client.clone(), self.executor.clone()) {
                    Ok(client) => return NextWriter::Took(client),
                    Err(error) => log::info!("connecting to the engine host: {error:#}"),
                }
            }
            let gone = !self.to.is_live();
            if gone || Instant::now() >= deadline {
                match self.repo.retake_writer() {
                    Ok(()) => {
                        return NextWriter::Back(if gone {
                            format!("pid {} ended before it took the writer", self.to.to_pid)
                        } else {
                            format!(
                                "pid {} did not take the writer within {}s",
                                self.to.to_pid,
                                self.start_timeout.as_secs()
                            )
                        });
                    }
                    // The host holds it (it is still opening the project), or a third process
                    // for an instant: wait.
                    Err(OpenError::HeldBy { .. }) => {}
                    Err(OpenError::Store(error)) => {
                        log::error!("taking the writer back: {error:#}");
                    }
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// The take-back's sink: state writes on this process's writer again. A repository command
/// that waited in the queue carries only a claim of the person, so it is refused and the person
/// tries again.
struct TakenBack(LocalSink);

impl StateSink for TakenBack {
    fn apply(&self, ops: Vec<StateOp>, origin: Author) -> Task<Result<()>> {
        self.0.apply(ops, origin)
    }

    fn repo(&self, _command: RepoCommand) -> Task<Result<RepoReply>> {
        Task::ready(Err(anyhow!(
            "the engine host did not start and the writer is back in this window; try again"
        )))
    }
}

/// Where a window's writes wait while its writer moves: they are kept in order and sent to the
/// next writer once there is one (or applied here after a take-back), then every later write
/// goes straight there.
pub struct QueueSink {
    inner: Mutex<QueueInner>,
    executor: BackgroundExecutor,
}

struct QueueInner {
    target: Target,
    pending: VecDeque<Queued>,
}

enum Target {
    Queueing,
    /// The queued writes are being sent to the sink, oldest first; new ones queue behind.
    Flushing,
    Forwarding(Arc<dyn StateSink>),
}

enum Queued {
    Apply(Vec<StateOp>, Author, oneshot::Sender<Result<()>>),
    Repo(RepoCommand, oneshot::Sender<Result<RepoReply>>),
}

impl QueueSink {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                target: Target::Queueing,
                pending: VecDeque::new(),
            }),
            executor,
        }
    }

    /// How many writes wait.
    pub fn queued(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// Sends the queued writes to `target` one after the other, in the order they came, then
    /// every later write straight to it. Resolves once the queue is empty.
    pub fn switch_to(
        self: &Arc<Self>,
        target: Arc<dyn StateSink>,
        executor: &BackgroundExecutor,
    ) -> Task<()> {
        self.inner.lock().target = Target::Flushing;
        let this = self.clone();
        executor.spawn(async move {
            loop {
                let next = {
                    let mut inner = this.inner.lock();
                    match inner.pending.pop_front() {
                        Some(next) => next,
                        None => {
                            inner.target = Target::Forwarding(target);
                            return;
                        }
                    }
                };
                match next {
                    Queued::Apply(ops, origin, answer) => {
                        answer.send(target.apply(ops, origin).await).ok();
                    }
                    Queued::Repo(command, answer) => {
                        answer.send(target.repo(command).await).ok();
                    }
                }
            }
        })
    }

    /// Forwards with `forward` when the queue is done, else queues with `queued`; both under
    /// one hold of the lock, so no write slips between the last flush and the switch.
    fn send<T: Send + 'static>(
        &self,
        forward: impl FnOnce(&Arc<dyn StateSink>) -> Task<Result<T>>,
        queued: impl FnOnce(oneshot::Sender<Result<T>>) -> Queued,
    ) -> Task<Result<T>> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut inner = self.inner.lock();
            if let Target::Forwarding(target) = &inner.target {
                let target = target.clone();
                drop(inner);
                return forward(&target);
            }
            inner.pending.push_back(queued(sender));
        }
        self.executor.spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("the write was dropped while the writer moved")))
        })
    }
}

impl StateSink for QueueSink {
    fn apply(&self, ops: Vec<StateOp>, origin: Author) -> Task<Result<()>> {
        let forwarded = (ops.clone(), origin.clone());
        self.send(
            move |target| target.apply(forwarded.0, forwarded.1),
            move |answer| Queued::Apply(ops, origin, answer),
        )
    }

    fn repo(&self, command: RepoCommand) -> Task<Result<RepoReply>> {
        let forwarded = command.clone();
        self.send(
            move |target| target.repo(forwarded),
            move |answer| Queued::Repo(command, answer),
        )
    }
}

/// Takes `location`'s writer lock and reads `host.json` again right after: when it records a
/// move in progress to a live process other than `own_pid`, the lock is let go at once and
/// `Ok(None)` says to wait for the move like the others (an opener that read `host.json` before
/// the move was written can still win the lock after the window released it).
pub fn take_writer_checked(
    location: &Location,
    hooks: &StateApplyHooks,
    own_pid: u32,
) -> Result<Option<StateWriter>, OpenError> {
    let writer = StateStore::open_writer_with_hooks(location, hooks)?;
    if let Some(info) = read_host_file(location)
        && let Some(moving) = move_in_progress(&info)
        && moving.to_pid != own_pid
        && moving.is_live()
    {
        log::info!(
            "{}: the writer is moving to pid {}; letting the lock go",
            location.id,
            moving.to_pid
        );
        drop(writer);
        return Ok(None);
    }
    Ok(Some(writer))
}

/// The engine host's side of a handover: asks the window named by `window` (`host.json` of a
/// window holding `location`'s writer) for the writer, and waits on the lock with a blocking
/// `flock` until the window lets it go. Refused with the window's reason when it keeps the
/// writer (the person cancelled, or nothing could be handed over). Blocks: the engine host calls
/// it before it opens the project.
pub fn take_writer_from_window(
    location: &Location,
    hooks: &'static StateApplyHooks,
    window: &HostInfo,
    options: ClientOptions,
    start_timeout: Duration,
    executor: BackgroundExecutor,
) -> Result<StateWriter, String> {
    let socket = window.socket.clone().ok_or_else(|| {
        format!(
            "{} is open in a bongos window (pid {}) with lanes off there; turn lanes on or close it",
            location.id, window.pid
        )
    })?;
    let client = HostClient::connect(&socket, options, executor).map_err(|error| {
        format!(
            "asking the window (pid {}) for the writer: {error:#}",
            window.pid
        )
    })?;

    enum Heard {
        Locked(Result<StateWriter, OpenError>),
        Answered(Result<serde_json::Value>),
    }
    let (sender, heard) = std::sync::mpsc::channel();
    let answer = client.request_unbounded(HostCommand::Host(HostCtl::Handover));
    let answers = sender.clone();
    std::thread::Builder::new()
        .name("engine-host-handover-answer".to_string())
        .spawn(move || {
            answers
                .send(Heard::Answered(futures::executor::block_on(answer)))
                .ok();
        })
        .map_err(|error| format!("waiting for the window's answer: {error}"))?;
    let waiting_location = location.clone();
    std::thread::Builder::new()
        .name("engine-host-lock".to_string())
        .spawn(move || {
            let locked = StateStore::open_writer_waiting(&waiting_location, hooks);
            if let Err(std::sync::mpsc::SendError(Heard::Locked(Ok(writer)))) =
                sender.send(Heard::Locked(locked))
            {
                // Nobody waits any more: the lock goes with the writer.
                drop(writer);
            }
        })
        .map_err(|error| format!("waiting on the writer lock: {error}"))?;

    let locked = |locked: Result<StateWriter, OpenError>| {
        locked.map_err(|error| format!("taking the writer of {}: {error:#}", location.id))
    };
    match heard.recv() {
        Ok(Heard::Locked(writer)) => return locked(writer),
        Ok(Heard::Answered(Ok(_))) => {}
        // The connection ended with no answer: the window let go or died; either way the lock
        // comes now.
        Ok(Heard::Answered(Err(_))) if client.is_closed() => {}
        Ok(Heard::Answered(Err(error))) => {
            return Err(format!(
                "the window (pid {}) kept the writer of {}: {error:#}",
                window.pid, location.id
            ));
        }
        Err(_) => return Err("the handover ended with no answer".to_string()),
    }
    let deadline = Instant::now() + start_timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match heard.recv_timeout(left) {
            Ok(Heard::Locked(writer)) => return locked(writer),
            Ok(Heard::Answered(_)) => {}
            Err(_) => {
                return Err(format!(
                    "the window released the writer of {} but it was not free within {}s",
                    location.id,
                    start_timeout.as_secs()
                ));
            }
        }
    }
}

/// Tells this window's `lanes: start` when the host asked for the writer (`began`) and how the
/// handover ended (`done`).
pub struct HandoverWatch {
    pub began: oneshot::Receiver<()>,
    pub done: oneshot::Receiver<Result<HostClient, String>>,
}

/// Records what the person chose for `project_id`'s next handover and watches it.
pub fn expect_handover(
    project_id: &str,
    request: HandoverRequest,
    cx: &mut App,
) -> Result<HandoverWatch> {
    if !cx.has_global::<Handovers>() {
        bail!("this process does not hand writers over");
    }
    let handovers = cx.global_mut::<Handovers>();
    if handovers.moving.contains_key(project_id) {
        bail!("the writer of {project_id} is moving already");
    }
    let (began_sender, began) = oneshot::channel();
    let (done_sender, done) = oneshot::channel();
    handovers.requests.insert(project_id.to_string(), request);
    handovers
        .began
        .entry(project_id.to_string())
        .or_default()
        .push(began_sender);
    handovers
        .done
        .entry(project_id.to_string())
        .or_default()
        .push(done_sender);
    Ok(HandoverWatch { began, done })
}

/// `lanes: start` in a window that holds `repo`'s writer: starts an engine host with `spawner`
/// (its output appended to `log`), which asks this window for the writer; resolves with the
/// client of the host once this window switched to it. Refused when the host does not ask
/// within the start timeout (it did not start), with the reason the writer stayed here, or
/// with "the engine host did not start; see <log>" after a take-back.
pub fn start_handover(
    repo: &ProjectRepo,
    spawner: Arc<dyn HostSpawner>,
    log: PathBuf,
    request: HandoverRequest,
    cx: &mut App,
) -> Task<Result<HostClient>> {
    let id = repo.location().id.clone();
    let Some(times) = cx.try_global::<Handovers>().map(|h| (h.times)(cx)) else {
        return Task::ready(Err(anyhow!("this process does not hand writers over")));
    };
    let watch = match expect_handover(&id, request, cx) {
        Ok(watch) => watch,
        Err(error) => return Task::ready(Err(error)),
    };
    let spawn_id = id.clone();
    let spawn_log = log.clone();
    let spawning = cx.background_spawn(async move { spawner.spawn(&spawn_id, &spawn_log) });
    cx.spawn(async move |cx| {
        let spawned = spawning.await;
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                cx.update(|cx| finish(&id, Err(String::new()), cx));
                return Err(
                    error.context(format!("starting the engine host; see {}", log.display()))
                );
            }
        };
        let timeout = cx.background_executor().timer(times.start_timeout).fuse();
        futures::pin_mut!(timeout);
        let mut began = watch.began;
        // The timeout first: a host that asked by then is found below all the same.
        let heard = futures::select_biased! {
            _ = timeout => None,
            heard = &mut began => Some(heard.is_ok()),
        };
        let exited = child.try_exit();
        let pid = child.pid();
        if exited.is_none() {
            child.release();
        }
        let asked = match heard {
            Some(asked) => asked,
            // Checked on the main thread, where the host's request is served: when it asked as
            // the timeout fired, the move goes on and its outcome is awaited.
            None => cx.update(|cx| {
                let asked = matches!(began.try_recv(), Ok(Some(()))) || is_moving(&id, cx);
                if asked {
                    return true;
                }
                // It may still ask: refuse it then, as the person was told it did not start.
                if exited.is_none()
                    && let Some(pid) = pid
                    && cx.has_global::<Handovers>()
                {
                    cx.global_mut::<Handovers>().abandon(pid);
                }
                finish(&id, Err(String::new()), cx);
                false
            }),
        };
        if !asked {
            let exited = exited
                .map(|status| format!(" (it exited: {status})"))
                .unwrap_or_default();
            bail!(
                "the engine host did not start within {}s{exited}; see {}",
                times.start_timeout.as_secs(),
                log.display()
            );
        }
        match watch.done.await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(reason)) => Err(anyhow!(reason)),
            Err(_) => Err(anyhow!("the handover ended with no answer")),
        }
    })
}
