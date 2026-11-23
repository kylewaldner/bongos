//! The server of a project's socket, the same in a window and in a host process. One thread
//! accepts connections and one thread per connection reads its lines; the `Hello` is checked and
//! the peer's identity read there, once per connection, and every request then goes to the app,
//! which runs it through [`crate::commands::dispatch`] and writes the reply back. Every committed
//! state transaction reaches the subscribed clients as a `StateChanged` event.

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use bongos_project_repo::{PeerIdentity, ProjectRepo, StateEvent, StateWriter, peer_identity};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Task};
use parking_lot::Mutex;

use crate::host_file::HostInfo;
use crate::protocol::{
    ClientMessage, HostCommand, HostEvent, HostResult, HostTopic, MAX_LINE_BYTES, PROTOCOL_VERSION,
    ServerMessage, encode_line, version_refusal,
};

/// How long a new connection may take to say `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How many lines may wait for a slow client before the connection is dropped.
pub(crate) const OUTBOX_LINES: usize = 256;

/// One accepted connection after its `Hello`.
struct Connection {
    id: u64,
    peer: PeerIdentity,
    /// Shut down to end the connection's threads.
    stream: UnixStream,
    /// The lines the connection's writing thread sends, so no caller ever blocks on a client.
    outbox: SyncSender<Vec<u8>>,
    /// `None` until the client subscribes; an empty set is every topic.
    topics: Mutex<Option<HashSet<HostTopic>>>,
}

impl Connection {
    /// Queues `message` without blocking; false when the connection is gone. A client whose
    /// queue is full is not reading, so its connection is ended.
    fn send(&self, message: &ServerMessage) -> bool {
        let Ok(line) = encode_line(message) else {
            return false;
        };
        match self.outbox.try_send(line) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::warn!(
                    "dropping the connection of pid {}: it stopped reading",
                    self.peer.pid()
                );
                self.close();
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Ends the connection: its reading thread sees the end of the stream and forgets it, and its
    /// writing thread's write fails.
    fn close(&self) {
        self.stream.shutdown(std::net::Shutdown::Both).ok();
    }

    fn wants(&self, topic: HostTopic) -> bool {
        match &*self.topics.lock() {
            Some(topics) => topics.is_empty() || topics.contains(&topic),
            None => false,
        }
    }
}

struct Shared {
    socket: PathBuf,
    stopping: AtomicBool,
    next_connection: AtomicU64,
    connections: Mutex<Vec<Arc<Connection>>>,
    /// Told each time a client connects or goes.
    watchers: Mutex<Vec<mpsc::UnboundedSender<()>>>,
}

impl Shared {
    fn broadcast(&self, event: &HostEvent) {
        let message = ServerMessage::Event {
            event: event.clone(),
        };
        let connections: Vec<_> = self
            .connections
            .lock()
            .iter()
            .filter(|connection| connection.wants(event.topic()))
            .cloned()
            .collect();
        for connection in connections {
            connection.send(&message);
        }
    }

    fn forget(&self, id: u64) {
        self.connections
            .lock()
            .retain(|connection| connection.id != id);
        self.connections_changed();
    }

    fn connections_changed(&self) {
        self.watchers
            .lock()
            .retain(|watcher| watcher.unbounded_send(()).is_ok());
    }
}

type Incoming = (Arc<Connection>, u64, HostCommand);

/// A project's socket being served. Dropping it stops the server, ends every connection and
/// removes the socket file.
pub struct EngineServer {
    shared: Arc<Shared>,
    _requests: Task<()>,
    _events: Task<()>,
}

impl EngineServer {
    /// Serves `listener` (bound at `info.socket`) for `repo`, whose writer this process holds.
    pub fn start(
        listener: UnixListener,
        repo: ProjectRepo,
        info: HostInfo,
        cx: &mut App,
    ) -> Result<Self> {
        let writer = repo
            .state()
            .and_then(|state| state.writer())
            .cloned()
            .context("serving a project's socket needs its writer")?;
        let socket = info
            .socket
            .clone()
            .context("host.json names no socket to serve")?;
        let shared = Arc::new(Shared {
            socket,
            stopping: AtomicBool::new(false),
            next_connection: AtomicU64::new(0),
            connections: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
        });
        let (incoming, mut requests) = mpsc::unbounded::<Incoming>();

        let status = serde_json::to_value(&info)?;
        let requests_task = cx.spawn(async move |cx| {
            while let Some((connection, id, command)) = requests.next().await {
                let stop = match &command {
                    HostCommand::Host(crate::protocol::HostCtl::Stop { at_job_boundary }) => cx
                        .update(|cx| crate::host_process::stop_request(cx))
                        .map(|sender| (sender, *at_job_boundary)),
                    _ => None,
                };
                let reply = cx.update(|cx| {
                    crate::commands::dispatch(&repo, command, &connection.peer, status.clone(), cx)
                });
                cx.background_executor()
                    .spawn(async move {
                        let result = match reply.await {
                            Ok(value) => HostResult::Ok(value),
                            Err(error) => HostResult::Refused(format!("{error:#}")),
                        };
                        let accepted = matches!(result, HostResult::Ok(_));
                        connection.send(&ServerMessage::Reply { id, result });
                        if let (true, Some((sender, at_job_boundary))) = (accepted, stop) {
                            sender.unbounded_send(at_job_boundary).ok();
                        }
                    })
                    .detach();
            }
        });

        let mut state_events = writer.subscribe();
        let events_shared = shared.clone();
        let events_task = cx.background_spawn(async move {
            while let Some(event) = state_events.next().await {
                let StateEvent::Changed {
                    generation,
                    task_ids,
                    namespaces,
                } = event;
                events_shared.broadcast(&HostEvent::StateChanged {
                    generation,
                    task_ids,
                    namespaces,
                });
            }
        });

        let accept_shared = shared.clone();
        let welcome = Welcome {
            version: info.bongos_version.clone(),
            mode: info.mode,
            writer,
        };
        std::thread::Builder::new()
            .name("engine-host-accept".to_string())
            .spawn(move || accept_loop(listener, accept_shared, incoming, welcome))
            .context("starting the socket's thread")?;

        Ok(Self {
            shared,
            _requests: requests_task,
            _events: events_task,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.shared.socket
    }

    /// Sends `event` to every client subscribed to its topic.
    pub fn broadcast(&self, event: &HostEvent) {
        self.shared.broadcast(event);
    }

    /// How many clients are connected now.
    pub fn connection_count(&self) -> usize {
        self.shared.connections.lock().len()
    }

    /// Stops taking new connections and removes the socket file, so the next writer can bind
    /// the same path; the connections already made go on (a reply in flight still reaches its
    /// client) until the server is dropped. Dropping it afterwards leaves the path alone.
    pub fn stop_accepting(&self) {
        if self.shared.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wakes the accepting thread, which sees `stopping` and ends.
        UnixStream::connect(&self.shared.socket).ok();
        std::fs::remove_file(&self.shared.socket).ok();
    }

    /// A message each time a client connects or goes, from now on.
    pub fn watch_connections(&self) -> mpsc::UnboundedReceiver<()> {
        let (sender, receiver) = mpsc::unbounded();
        self.shared.watchers.lock().push(sender);
        receiver
    }
}

impl Drop for EngineServer {
    fn drop(&mut self) {
        // After `stop_accepting` the path may be another writer's socket already.
        let accepting = !self.shared.stopping.swap(true, Ordering::SeqCst);
        if accepting {
            // Wakes the accepting thread, which sees `stopping` and ends.
            UnixStream::connect(&self.shared.socket).ok();
        }
        for connection in self.shared.connections.lock().drain(..) {
            connection.close();
        }
        if accepting {
            std::fs::remove_file(&self.shared.socket).ok();
        }
    }
}

#[derive(Clone)]
struct Welcome {
    version: String,
    mode: crate::protocol::HostMode,
    writer: Arc<StateWriter>,
}

fn accept_loop(
    listener: UnixListener,
    shared: Arc<Shared>,
    incoming: mpsc::UnboundedSender<Incoming>,
    welcome: Welcome,
) {
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("accepting on {}: {error}", shared.socket.display());
                continue;
            }
        };
        let shared = shared.clone();
        let incoming = incoming.clone();
        let welcome = welcome.clone();
        let spawned = std::thread::Builder::new()
            .name("engine-host-connection".to_string())
            .spawn(move || {
                if let Err(error) = serve_connection(stream, &shared, incoming, &welcome) {
                    log::info!("engine host connection ended: {error:#}");
                }
            });
        if let Err(error) = spawned {
            log::error!("starting a connection's thread: {error}");
        }
    }
}

/// Reads one line of at most [`MAX_LINE_BYTES`]; `None` at the end of the stream.
pub(crate) fn read_line(reader: &mut BufReader<UnixStream>) -> Result<Option<String>> {
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64)
        .read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && read >= MAX_LINE_BYTES {
        return Err(anyhow!("a line longer than {MAX_LINE_BYTES} bytes"));
    }
    Ok(Some(line))
}

fn serve_connection(
    stream: UnixStream,
    shared: &Arc<Shared>,
    incoming: mpsc::UnboundedSender<Incoming>,
    welcome: &Welcome,
) -> Result<()> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let refuse = |writer: &mut UnixStream, text: String| -> Result<()> {
        writer.write_all(&encode_line(&ServerMessage::Refused {
            text: text.clone(),
        })?)?;
        Err(anyhow!(text))
    };
    let Some(line) = read_line(&mut reader)? else {
        return Ok(());
    };
    let client = match serde_json::from_str::<ClientMessage>(&line) {
        Ok(ClientMessage::Hello {
            protocol,
            bongos_version,
            client,
        }) => {
            if protocol != PROTOCOL_VERSION || bongos_version != welcome.version {
                return refuse(&mut writer, version_refusal(&welcome.version));
            }
            client
        }
        // A line this server cannot read is most likely another protocol's hello.
        _ => return refuse(&mut writer, version_refusal(&welcome.version)),
    };
    let peer = peer_identity(&stream, client).context("reading the peer's identity")?;
    stream.set_read_timeout(None)?;
    let (outbox, lines) = std::sync::mpsc::sync_channel::<Vec<u8>>(OUTBOX_LINES);
    let connection = Arc::new(Connection {
        id: shared.next_connection.fetch_add(1, Ordering::SeqCst),
        peer,
        stream: stream.try_clone()?,
        outbox,
        topics: Mutex::new(None),
    });
    let closer = stream.try_clone()?;
    std::thread::Builder::new()
        .name("engine-host-writer".to_string())
        .spawn(move || write_lines(writer, closer, lines))
        .context("starting a connection's writing thread")?;
    shared.connections.lock().push(connection.clone());
    shared.connections_changed();
    let generation = welcome.writer.generation().unwrap_or(0);
    let welcomed = connection.send(&ServerMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        bongos_version: welcome.version.clone(),
        writer_pid: std::process::id(),
        mode: welcome.mode,
        generation,
    });
    let outcome = if welcomed {
        read_requests(&mut reader, &connection, &incoming)
    } else {
        Ok(())
    };
    shared.forget(connection.id);
    outcome
}

/// Writes a connection's queued lines until every sender is gone; a failed write ends the
/// connection.
fn write_lines(mut writer: UnixStream, closer: UnixStream, lines: Receiver<Vec<u8>>) {
    for line in lines {
        if writer
            .write_all(&line)
            .and_then(|()| writer.flush())
            .is_err()
        {
            closer.shutdown(std::net::Shutdown::Both).ok();
            return;
        }
    }
}

fn read_requests(
    reader: &mut BufReader<UnixStream>,
    connection: &Arc<Connection>,
    incoming: &mpsc::UnboundedSender<Incoming>,
) -> Result<()> {
    while let Some(line) = read_line(reader)? {
        match serde_json::from_str::<ClientMessage>(&line) {
            Ok(ClientMessage::Request { id, command }) => {
                if incoming
                    .unbounded_send((connection.clone(), id, command))
                    .is_err()
                {
                    return Ok(());
                }
            }
            Ok(ClientMessage::Subscribe { topics }) => {
                *connection.topics.lock() = Some(topics.into_iter().collect());
            }
            Ok(ClientMessage::Hello { .. }) => {
                log::info!("ignoring a second hello from pid {}", connection.peer.pid());
            }
            Err(error) => {
                log::warn!(
                    "unreadable request from pid {}: {error}",
                    connection.peer.pid()
                );
                // A request this build cannot read still gets its answer, so its client does
                // not wait for one.
                let id = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64));
                if let Some(id) = id {
                    connection.send(&ServerMessage::Reply {
                        id,
                        result: HostResult::Refused(format!("an unreadable request: {error}")),
                    });
                }
            }
        }
    }
    Ok(())
}
