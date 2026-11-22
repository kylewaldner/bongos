//! A window opening projects as the first process (the writer, serving the project's socket while
//! lanes are on) or as a later one (a client of the writer, reading the store itself and sending
//! every write through the socket). [`install_window`] makes the app's projects open this way.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bongos_project_repo::{ClientKind, StateEvent};
use bongos_work::{OpenedProject, ProjectOpening};
use bongos_workflow::{WorkflowEngine, WorkflowEvent};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, BorrowAppContext as _, Global};
use parking_lot::Mutex;
use settings::SettingsStore;

use crate::access::{EngineAccess, EngineAccesses};
use crate::attach::{OpenedState, open_state};
use crate::client::{ClientOptions, HostClient};
use crate::protocol::{HostEvent, HostMode, HostTopic};
use crate::writer_host::{WriterHost, WriterOptions};

/// What a window needs to host or attach to a project.
#[derive(Clone)]
pub struct WindowHosting {
    pub bongos_version: String,
    /// The log a client names when the writer does not answer.
    pub log: PathBuf,
    /// Where the sockets go: [`crate::default_hosts_dir`] outside tests.
    pub hosts_dir: PathBuf,
    /// Whether `lanes.enabled` is on: while it is, the writer serves its socket.
    pub lanes_enabled: Rc<dyn Fn(&App) -> bool>,
}

/// The projects this window holds the writer of, announced in `host.json` and served on their
/// sockets while lanes are on.
#[derive(Default)]
pub struct WriterHosts(Vec<WriterHost>);

impl Global for WriterHosts {}

impl WriterHosts {
    /// Sends `event` to every client of every project this process serves.
    pub fn broadcast(event: &HostEvent, cx: &App) {
        let Some(hosts) = cx.try_global::<Self>() else {
            return;
        };
        for host in &hosts.0 {
            if let Some(server) = host.server() {
                server.broadcast(event);
            }
        }
    }

    pub fn hosts(&self) -> &[WriterHost] {
        &self.0
    }
}

/// Makes every project this process opens from now on open through the engine host's rules.
/// Call after `bongos_work::init`.
pub fn install_window(hosting: WindowHosting, cx: &mut App) {
    let clients: Arc<Mutex<HashMap<String, HostClient>>> = Arc::default();
    let executor = cx.background_executor().clone();
    let options = ClientOptions::new(
        ClientKind::Window,
        hosting.bongos_version.clone(),
        hosting.log.clone(),
    );
    let opener_clients = clients.clone();
    let opener = Arc::new(
        move |location: &bongos_project_repo::Location| -> Result<_> {
            match open_state(location, options.clone(), executor.clone())? {
                OpenedState::Writer(writer) => Ok(OpenedProject::Writer(writer)),
                OpenedState::Client { client, reader, .. } => {
                    let events = client.subscribe(Some(vec![HostTopic::State]))?;
                    let changes = state_changes(events, &executor);
                    opener_clients
                        .lock()
                        .insert(location.id.clone(), client.clone());
                    Ok(OpenedProject::Client {
                        reader,
                        sink: Arc::new(client),
                        changes: Some(changes),
                    })
                }
            }
        },
    );
    let served_hosting = hosting.clone();
    let served = Rc::new(
        move |loaded: &Arc<bongos_project_repo::LoadedProject>, cx: &mut App| {
            serve(&served_hosting, &clients, loaded.repo(), cx);
        },
    );
    ProjectOpening::install(opener, Some(served), cx);
    // The writer serves its socket only while lanes are on.
    cx.observe_global::<SettingsStore>(move |cx| {
        if cx.has_global::<WriterHosts>() {
            set_serving((hosting.lanes_enabled)(cx), cx);
        }
    })
    .detach();
    WorkflowEngine::observe(cx, |event, cx| {
        let WorkflowEvent::Changed {
            run,
            task_id,
            state,
        } = event;
        WriterHosts::broadcast(
            &HostEvent::RunChanged {
                run: run.clone(),
                subject: task_id.clone(),
                lifecycle: format!("{state:?}"),
            },
            cx,
        );
    });
}

fn set_serving(serve: bool, cx: &mut App) {
    cx.update_global::<WriterHosts, _>(|hosts, cx| {
        for host in &mut hosts.0 {
            if let Err(error) = host.set_serving(serve, cx) {
                log::error!("the engine host socket: {error:#}");
            }
        }
    });
}

/// The state events of the writer as the project's own change events.
fn state_changes(
    mut events: mpsc::UnboundedReceiver<HostEvent>,
    executor: &gpui::BackgroundExecutor,
) -> mpsc::UnboundedReceiver<StateEvent> {
    let (sender, receiver) = mpsc::unbounded();
    executor
        .spawn(async move {
            while let Some(event) = events.next().await {
                if let HostEvent::StateChanged {
                    generation,
                    task_ids,
                    namespaces,
                } = event
                {
                    let event = StateEvent::Changed {
                        generation,
                        task_ids,
                        namespaces,
                    };
                    if sender.unbounded_send(event).is_err() {
                        return;
                    }
                }
            }
        })
        .detach();
    receiver
}

/// Runs once a project is served: the writer announces itself, a client points its writes at
/// the writer.
fn serve(
    hosting: &WindowHosting,
    clients: &Mutex<HashMap<String, HostClient>>,
    repo: &bongos_project_repo::ProjectRepo,
    cx: &mut App,
) {
    let id = repo.location().id.clone();
    let holds_writer = repo.state().and_then(|state| state.writer()).is_some();
    if holds_writer {
        let mut options =
            WriterOptions::window(&hosting.bongos_version, (hosting.lanes_enabled)(cx));
        options.hosts_dir = hosting.hosts_dir.clone();
        debug_assert_eq!(options.mode, HostMode::Window);
        match WriterHost::start(repo.clone(), options, cx) {
            Ok(host) => cx.default_global::<WriterHosts>().0.push(host),
            Err(error) => log::error!("announcing the engine host of {id}: {error:#}"),
        }
        let access = EngineAccess::Local(repo.clone());
        access.hook_up_checks(cx);
        EngineAccesses::set(id, access, cx);
    } else if let Some(client) = clients.lock().remove(&id) {
        let access = EngineAccess::Remote(client);
        access.hook_up_checks(cx);
        EngineAccesses::set(id, access, cx);
    }
}
