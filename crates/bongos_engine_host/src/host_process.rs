//! The engine host process of one project (`bongos --engine-host <project id>`): it takes the
//! project's writer lock, or exits at once when another process holds it; announces itself in
//! `host.json` in host mode and serves the project's socket; and exits once it has been idle
//! (no client connected, no run active, no dispatcher running) for `lanes.host.idle_exit_seconds`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bongos_project_repo::{
    Location, OpenError, ProjectRepo, StateApplyHooks, StateStore, StateWriter,
};
use bongos_workflow::WorkflowEngine;
use futures::channel::mpsc;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, Global, Task};

use crate::access::{EngineAccess, EngineAccesses};
use crate::client::ClientOptions;
use crate::handover::{take_writer_checked, take_writer_from_window};
use crate::host_file::live_host;
use crate::protocol::{HostEvent, HostMode};
use crate::window::WriterHosts;
use crate::writer_host::{WriterHost, WriterOptions};

/// Why this process does not become the engine host of `location`: another live process holds
/// its writer.
pub fn held_by_message(location: &Location, pid: Option<u32>) -> String {
    let id = &location.id;
    match live_host(location) {
        Some(info) if info.mode == HostMode::Window => format!(
            "{id} is open in a bongos window (pid {}, started {}), which holds its writer; \
             the engine host does not start beside it",
            info.pid,
            info.started.to_rfc3339()
        ),
        Some(info) => format!(
            "an engine host for {id} is already running (pid {}, started {})",
            info.pid,
            info.started.to_rfc3339()
        ),
        None => format!(
            "an engine host for {id} is already running (pid {}, started unknown)",
            pid.map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
        ),
    }
}

/// Takes `location`'s writer for the engine host. `Err` carries the message to exit with: the
/// writer is held by another live process ([`held_by_message`]), or the store failed. The lock
/// is an `flock` the system releases when this process dies, so a dead host never holds it.
pub fn open_host_writer(
    location: &Location,
    hooks: &StateApplyHooks,
) -> Result<StateWriter, String> {
    match StateStore::open_writer_with_hooks(location, hooks) {
        Ok(writer) => Ok(writer),
        Err(OpenError::HeldBy { pid }) => Err(held_by_message(location, pid)),
        Err(OpenError::Store(error)) => {
            Err(format!("opening the state of {}: {error:#}", location.id))
        }
    }
}

/// [`open_host_writer`] for an engine host that takes the writer over from a window: when a live
/// window holds it (`host.json` mode window, no move in progress), the host asks that window for
/// it and waits on the lock until the window lets go ([`take_writer_from_window`]). When the lock
/// was free but `host.json` records a move to another live process, the lock is let go again and
/// the host does not start. Blocks while the window drains.
pub fn take_host_writer(
    location: &Location,
    hooks: &'static StateApplyHooks,
    options: ClientOptions,
    start_timeout: std::time::Duration,
    executor: gpui::BackgroundExecutor,
) -> Result<StateWriter, String> {
    match take_writer_checked(location, hooks, options.own_pid) {
        Ok(Some(writer)) => Ok(writer),
        Ok(None) => Err(format!(
            "the writer of {} is moving to another process; this engine host does not start",
            location.id
        )),
        Err(OpenError::HeldBy { pid }) => match live_host(location) {
            Some(window) if window.mode == HostMode::Window && window.moving.is_none() => {
                log::info!(
                    "{} is held by a window (pid {}); asking it for the writer",
                    location.id,
                    window.pid
                );
                take_writer_from_window(location, hooks, &window, options, start_timeout, executor)
            }
            _ => Err(held_by_message(location, pid)),
        },
        Err(OpenError::Store(error)) => {
            Err(format!("opening the state of {}: {error:#}", location.id))
        }
    }
}

/// Whether something outside the engine host's own bookkeeping keeps it busy (the lanes'
/// dispatcher is running).
pub type HostBusy = Rc<dyn Fn(&App) -> bool>;

/// What a stop at the job boundary asks first, so that [`HostBusy`] can turn false: the lanes'
/// dispatcher picks no new job, lets its running jobs finish and then ends.
pub type HostDrain = Rc<dyn Fn(&mut App)>;

/// How the engine host process serves its project.
#[derive(Clone)]
pub struct HostProcessOptions {
    pub bongos_version: String,
    /// Where the socket goes: [`crate::default_hosts_dir`] outside tests.
    pub hosts_dir: std::path::PathBuf,
    /// `lanes.host.idle_exit_seconds`.
    pub idle_exit: Duration,
}

/// Marks this process as an engine host process, which runs the lanes' dispatcher.
pub struct EngineHostProcess {
    _idle: Task<()>,
}

impl Global for EngineHostProcess {}

impl EngineHostProcess {
    /// Whether this process is an engine host process.
    pub fn is_running(cx: &App) -> bool {
        cx.has_global::<Self>()
    }

    /// Serves `repo`, whose writer this process holds, as its engine host: `host.json` in host
    /// mode, the socket (a stale one left by a dead writer is removed first), the engine access
    /// of the project. Once idle for `options.idle_exit` it stops serving ([`stop_serving`]) and
    /// calls `on_exit`; asked to stop at the job boundary, it calls `drain` and then waits until
    /// no run is active and `busy` is false.
    pub fn start(
        repo: ProjectRepo,
        options: HostProcessOptions,
        busy: HostBusy,
        drain: HostDrain,
        on_exit: Box<dyn FnOnce(&mut App)>,
        cx: &mut App,
    ) -> Result<()> {
        let writer_options = WriterOptions {
            mode: HostMode::Host,
            bongos_version: options.bongos_version.clone(),
            hosts_dir: options.hosts_dir.clone(),
            serve_socket: true,
        };
        let id = repo.location().id.clone();
        let host = WriterHost::start(repo.clone(), writer_options, cx)?;
        log::info!(
            "engine host of {id} serving {}",
            host.info().socket.as_deref().map_or_else(
                || "no socket".to_string(),
                |path| path.display().to_string()
            )
        );

        let (pokes, poked) = mpsc::unbounded::<()>();
        if let Some(server) = host.server() {
            forward(server.watch_connections(), pokes.clone(), cx);
        }
        if let Some(writer) = repo.state().and_then(|state| state.writer()) {
            forward(writer.subscribe().map(|_| ()), pokes.clone(), cx);
        }
        let workflow_pokes = pokes;
        WorkflowEngine::observe(cx, move |event, _| {
            let bongos_workflow::WorkflowEvent::Changed {
                run,
                task_id,
                state,
            } = event;
            log::info!("run {run} of {task_id}: {state:?}");
            workflow_pokes.unbounded_send(()).ok();
        });
        cx.default_global::<WriterHosts>().0.push(host);
        crate::window::broadcast_runs(cx);
        let access = EngineAccess::Local(repo);
        access.hook_up_checks(cx);
        EngineAccesses::set(id.clone(), access, cx);

        let idle_exit = options.idle_exit;
        let (stop_sender, mut stop_requests) = mpsc::unbounded::<bool>();
        cx.set_global(StopRequests(stop_sender));
        let idle = cx.spawn(async move |cx| {
            let requested = {
                let idle = wait_until_idle(idle_exit, &busy, poked, cx).fuse();
                futures::pin_mut!(idle);
                futures::select! {
                    _ = idle => None,
                    request = stop_requests.next() => request,
                }
            };
            let reason = match requested {
                None => {
                    log::info!(
                        "engine host of {id} stopping: idle for {}s",
                        idle_exit.as_secs()
                    );
                    "the engine host was idle"
                }
                Some(at_job_boundary) => {
                    log::info!("engine host of {id} stopping: asked to stop");
                    if at_job_boundary {
                        // Nothing else stops the dispatcher, which would keep starting jobs.
                        cx.update(|cx| drain(cx));
                        wait_until_no_runs(&busy, cx).await;
                    }
                    "the engine host was stopped"
                }
            };
            cx.update(|cx| {
                stop_serving(reason, cx);
                on_exit(cx);
            });
        });
        cx.set_global(Self { _idle: idle });
        Ok(())
    }
}

/// Where `host stop` asks this process to stop: `true` waits for the jobs at their boundary.
struct StopRequests(mpsc::UnboundedSender<bool>);

impl Global for StopRequests {}

/// The way to ask this engine host process to stop (what `HostCtl::Stop` does), or `None` in a
/// process that is not one.
pub(crate) fn stop_request(cx: &App) -> Option<mpsc::UnboundedSender<bool>> {
    cx.try_global::<StopRequests>()
        .map(|requests| requests.0.clone())
}

/// Resolves once no run moves on by itself and `busy` says nothing is, on the executor's clock. A
/// halted, failed, paused or waiting run waits for a person and is not waited on. Clients do not
/// count: the one that asked to stop is connected.
async fn wait_until_no_runs(busy: &HostBusy, cx: &mut AsyncApp) {
    let executor = cx.background_executor().clone();
    while cx.update(|cx| {
        cx.try_global::<WorkflowEngine>()
            .is_some_and(|engine| !engine.progressing_runs().is_empty())
            || busy(cx)
    }) {
        executor.timer(BUSY_RECHECK).await;
    }
}

fn forward(
    mut from: impl futures::Stream<Item = ()> + Unpin + Send + 'static,
    to: mpsc::UnboundedSender<()>,
    cx: &mut App,
) {
    cx.background_spawn(async move {
        while from.next().await.is_some() {
            if to.unbounded_send(()).is_err() {
                return;
            }
        }
    })
    .detach();
}

/// Whether the engine host is busy: a client is connected, a run is active, or `busy` says so.
pub fn host_is_busy(busy: &HostBusy, cx: &App) -> bool {
    let connected = cx.try_global::<WriterHosts>().is_some_and(|hosts| {
        hosts
            .hosts()
            .iter()
            .filter_map(WriterHost::server)
            .any(|server| server.connection_count() > 0)
    });
    connected
        || cx
            .try_global::<WorkflowEngine>()
            .is_some_and(|engine| !engine.active_runs().is_empty())
        || busy(cx)
}

/// How often a busy host looks again with no poke, in case something ended without one.
pub const BUSY_RECHECK: Duration = Duration::from_secs(30);

/// Resolves once the host has not been busy for `idle_exit` in a row. It looks again at each
/// poke (a client came or went, the state or a run changed), every [`BUSY_RECHECK`] while busy,
/// and when the idle time is up, all on the executor's clock, so a test drives it with a fake
/// clock.
pub async fn wait_until_idle(
    idle_exit: Duration,
    busy: &HostBusy,
    mut pokes: mpsc::UnboundedReceiver<()>,
    cx: &mut AsyncApp,
) {
    let executor = cx.background_executor().clone();
    let mut idle_since: Option<Instant> = None;
    let mut pokes_open = true;
    loop {
        let now = executor.now();
        let left = if cx.update(|cx| host_is_busy(busy, cx)) {
            idle_since = None;
            BUSY_RECHECK
        } else {
            let since = *idle_since.get_or_insert(now);
            let left = (since + idle_exit).saturating_duration_since(now);
            if left.is_zero() {
                return;
            }
            left
        };
        let timer = executor.timer(left).fuse();
        futures::pin_mut!(timer);
        if !pokes_open {
            timer.await;
            continue;
        }
        futures::select! {
            _ = timer => {}
            poke = pokes.next() => pokes_open = poke.is_some(),
        }
    }
}

/// Stops serving every project this process holds the writer of: clients hear `WriterStopping`
/// with `reason`, then the socket and `host.json` (when it still names this process) are
/// removed. The writer lock goes with the process.
pub fn stop_serving(reason: &str, cx: &mut App) {
    let Some(hosts) = cx.try_global::<WriterHosts>() else {
        return;
    };
    let attached = hosts
        .hosts()
        .iter()
        .filter_map(WriterHost::server)
        .any(|server| server.connection_count() > 0);
    if attached {
        WriterHosts::broadcast(
            &HostEvent::WriterStopping {
                reason: reason.to_string(),
            },
            cx,
        );
    }
    let hosts = std::mem::take(&mut cx.default_global::<WriterHosts>().0);
    drop(hosts);
}
