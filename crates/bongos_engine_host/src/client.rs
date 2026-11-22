//! A process that does not hold a project's writer reaches it through [`HostClient`]: its writes
//! become requests on the project's socket, and it hears of changes through the events. A writer
//! that does not answer a request in time leaves the client read-only, every write refused with
//! "the engine host is not answering; see <log>", until the writer is heard from again.
//!
//! While the writer moves to another process (a handover), the old writer refuses requests with
//! [`MOVING_REFUSAL`] and then stops serving; the client tries the request again, connecting to
//! the project's socket again once the old writer has gone (the next writer binds the same
//! path), for up to [`ClientOptions::moving_timeout`]. Subscriptions carry over to the new
//! connection.

use std::collections::HashMap;
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo::{ClientKind, RepoCommand, RepoReply, StateOp, StateSink};
use bongos_project_repo_model::Author;
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{BackgroundExecutor, Task};
use parking_lot::Mutex;

use crate::protocol::{
    ClientMessage, HostCommand, HostEvent, HostMode, HostResult, HostTopic, PROTOCOL_VERSION,
    ServerMessage, encode_line,
};
use crate::server::read_line;

/// How long a request waits for the writer's answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a request is tried again while the writer moves (`lanes.host.start_timeout_seconds`
/// by default).
pub const MOVING_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a client waits between two tries while the writer moves.
pub const MOVING_RETRY: Duration = Duration::from_millis(50);

/// What the writer answers every request with while it hands itself over to another process.
pub const MOVING_REFUSAL: &str = "the writer is moving; retry";

/// What a client says when the writer does not answer in time.
pub fn not_answering(log: &Path) -> String {
    format!("the engine host is not answering; see {}", log.display())
}

fn closed_message(log: &Path) -> String {
    format!("the engine host closed the connection; see {}", log.display())
}

/// How a client connects.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub client: ClientKind,
    /// The version this process runs; the server refuses any other.
    pub bongos_version: String,
    pub answer_timeout: Duration,
    /// How long a request is tried again while the writer moves, and how long an opener waits
    /// for a move to end before it attaches.
    pub moving_timeout: Duration,
    /// The writer's log, named when it does not answer.
    pub log: PathBuf,
    /// This process's pid as the move rules compare it with `host.json` (tests set another).
    pub own_pid: u32,
}

impl ClientOptions {
    pub fn new(client: ClientKind, bongos_version: impl Into<String>, log: PathBuf) -> Self {
        Self {
            client,
            bongos_version: bongos_version.into(),
            answer_timeout: ANSWER_TIMEOUT,
            moving_timeout: MOVING_TIMEOUT,
            log,
            own_pid: std::process::id(),
        }
    }
}

/// What the writer said when the client connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Welcomed {
    pub bongos_version: String,
    pub writer_pid: u32,
    pub mode: HostMode,
    pub generation: u64,
}

type Subscribers = Mutex<Vec<(Option<Vec<HostTopic>>, mpsc::UnboundedSender<HostEvent>)>>;

/// What every connection of one client shares.
#[derive(Default)]
struct ClientShared {
    subscribers: Subscribers,
    /// Set when the writer said it moves to another process.
    moving: AtomicBool,
}

/// What one connection's reading thread shares with the client.
#[derive(Default)]
struct ConnectionState {
    pending: Mutex<HashMap<u64, oneshot::Sender<HostResult>>>,
    /// Set when a request timed out; cleared when the writer is heard from.
    not_answering: AtomicBool,
    closed: AtomicBool,
}

struct Connection {
    stream: UnixStream,
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    state: Arc<ConnectionState>,
    subscribed: AtomicBool,
    welcomed: Welcomed,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Ends the reading thread.
        self.stream.shutdown(std::net::Shutdown::Both).ok();
    }
}

struct Inner {
    socket: PathBuf,
    options: ClientOptions,
    shared: Arc<ClientShared>,
    connection: Mutex<Arc<Connection>>,
}

/// A connection to the process that holds a project's writer. Cloning shares the connection.
#[derive(Clone)]
pub struct HostClient {
    inner: Arc<Inner>,
    executor: BackgroundExecutor,
}

impl HostClient {
    /// Connects to the socket at `socket` and says `Hello`. Refused with the server's text on a
    /// protocol or version mismatch, and with [`not_answering`] when it does not answer in time.
    /// Blocks while connecting.
    pub fn connect(
        socket: &Path,
        options: ClientOptions,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let shared = Arc::new(ClientShared::default());
        let connection = open_connection(socket, &options, &shared)?;
        Ok(Self {
            inner: Arc::new(Inner {
                socket: socket.to_path_buf(),
                options,
                shared,
                connection: Mutex::new(Arc::new(connection)),
            }),
            executor,
        })
    }

    pub fn welcomed(&self) -> Welcomed {
        self.inner.connection.lock().welcomed.clone()
    }

    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    pub fn socket(&self) -> &Path {
        &self.inner.socket
    }

    /// Whether writes are refused because the writer did not answer in time.
    pub fn is_read_only(&self) -> bool {
        self.inner
            .connection
            .lock()
            .state
            .not_answering
            .load(Ordering::SeqCst)
    }

    /// Whether the writer ended the connection.
    pub fn is_closed(&self) -> bool {
        self.inner
            .connection
            .lock()
            .state
            .closed
            .load(Ordering::SeqCst)
    }

    /// Sends `command` and resolves with the writer's answer. The socket is written when the
    /// future is first polled, never on the caller's thread. While the writer moves, the request
    /// is tried again (on a new connection once the old writer has gone) until it is answered
    /// or [`ClientOptions::moving_timeout`] passes.
    pub fn request(
        &self,
        command: HostCommand,
    ) -> impl std::future::Future<Output = Result<serde_json::Value>> + Send + 'static {
        let inner = self.inner.clone();
        let executor = self.executor.clone();
        async move {
            let deadline = executor.now() + inner.options.moving_timeout;
            loop {
                let answer = match current_connection(&inner) {
                    Ok(connection) => {
                        ask(&connection, &inner, command.clone(), &executor, true).await
                    }
                    Err(error) => Err(error),
                };
                match answer {
                    Err(error)
                        if is_moving(&error, &inner.shared) && executor.now() < deadline =>
                    {
                        executor.timer(MOVING_RETRY).await;
                    }
                    answer => return answer,
                }
            }
        }
    }

    /// Sends `command` once and waits for its answer however long it takes, with no retry: the
    /// handover, which a drain may hold for a long time. Resolves with an error when the
    /// connection closes first.
    pub fn request_unbounded(
        &self,
        command: HostCommand,
    ) -> impl std::future::Future<Output = Result<serde_json::Value>> + Send + 'static {
        let inner = self.inner.clone();
        let executor = self.executor.clone();
        let connection = self.inner.connection.lock().clone();
        async move { ask(&connection, &inner, command, &executor, false).await }
    }

    /// The writer's events of `topics` (every topic when `None`) from now on, on this
    /// connection and on the ones made after the writer moved.
    pub fn subscribe(
        &self,
        topics: Option<Vec<HostTopic>>,
    ) -> Result<mpsc::UnboundedReceiver<HostEvent>> {
        let (sender, receiver) = mpsc::unbounded();
        self.inner
            .shared
            .subscribers
            .lock()
            .push((topics, sender));
        let connection = self.inner.connection.lock().clone();
        subscribe_once(&connection)?;
        Ok(receiver)
    }

    /// Sends an extension command of `namespace` and resolves with the handler's answer.
    pub fn extension(
        &self,
        namespace: impl Into<String>,
        payload: serde_json::Value,
    ) -> Task<Result<serde_json::Value>> {
        let request = self.request(HostCommand::Extension {
            namespace: namespace.into(),
            payload,
        });
        self.executor.spawn(request)
    }
}

/// The connection to use now: the current one, or, once it closed after the writer said it
/// moves, a new one to the same socket (subscriptions carried over).
fn current_connection(inner: &Inner) -> Result<Arc<Connection>> {
    let mut current = inner.connection.lock();
    let closed = current.state.closed.load(Ordering::SeqCst);
    if !closed || !inner.shared.moving.load(Ordering::SeqCst) {
        return Ok(current.clone());
    }
    let connection = Arc::new(open_connection(&inner.socket, &inner.options, &inner.shared)?);
    if !inner.shared.subscribers.lock().is_empty() {
        subscribe_once(&connection)?;
    }
    inner.shared.moving.store(false, Ordering::SeqCst);
    *current = connection.clone();
    Ok(connection)
}

/// Whether `error` means the writer is moving, so the request is worth trying again.
fn is_moving(error: &anyhow::Error, shared: &ClientShared) -> bool {
    error.to_string() == MOVING_REFUSAL || shared.moving.load(Ordering::SeqCst)
}

async fn ask(
    connection: &Connection,
    inner: &Inner,
    command: HostCommand,
    executor: &BackgroundExecutor,
    timed: bool,
) -> Result<serde_json::Value> {
    let log = &inner.options.log;
    let (id, answer) = send_request(connection, command, log)?;
    let mut answer = answer.fuse();
    let answered = if timed {
        let timeout = executor.timer(inner.options.answer_timeout).fuse();
        futures::pin_mut!(timeout);
        futures::select! {
            result = answer => result,
            _ = timeout => {
                connection.state.pending.lock().remove(&id);
                connection.state.not_answering.store(true, Ordering::SeqCst);
                return Err(anyhow!(not_answering(log)));
            }
        }
    } else {
        answer.await
    };
    match answered {
        Ok(HostResult::Ok(value)) => Ok(value),
        Ok(HostResult::Refused(text)) => {
            if text == MOVING_REFUSAL {
                // A client that subscribed to nothing hears of the move only here.
                inner.shared.moving.store(true, Ordering::SeqCst);
            }
            Err(anyhow!(text))
        }
        Err(_) => Err(anyhow!(closed_message(log))),
    }
}

fn send_request(
    connection: &Connection,
    command: HostCommand,
    log: &Path,
) -> Result<(u64, oneshot::Receiver<HostResult>)> {
    let state = &connection.state;
    if state.not_answering.load(Ordering::SeqCst) {
        bail!(not_answering(log));
    }
    if state.closed.load(Ordering::SeqCst) {
        bail!(closed_message(log));
    }
    let id = connection.next_id.fetch_add(1, Ordering::SeqCst);
    let (sender, receiver) = oneshot::channel();
    state.pending.lock().insert(id, sender);
    let line = encode_line(&ClientMessage::Request { id, command })?;
    let written = {
        let mut writer = connection.writer.lock();
        writer.write_all(&line).and_then(|()| writer.flush())
    };
    if let Err(error) = written {
        state.pending.lock().remove(&id);
        if error.kind() == std::io::ErrorKind::WouldBlock
            || error.kind() == std::io::ErrorKind::TimedOut
        {
            state.not_answering.store(true, Ordering::SeqCst);
            bail!(not_answering(log));
        }
        return Err(anyhow!(error).context("sending a request to the engine host"));
    }
    Ok((id, receiver))
}

fn subscribe_once(connection: &Connection) -> Result<()> {
    if !connection.subscribed.swap(true, Ordering::SeqCst) {
        let line = encode_line(&ClientMessage::Subscribe { topics: Vec::new() })?;
        let mut writer = connection.writer.lock();
        writer.write_all(&line)?;
        writer.flush()?;
    }
    Ok(())
}

fn open_connection(
    socket: &Path,
    options: &ClientOptions,
    shared: &Arc<ClientShared>,
) -> Result<Connection> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    stream.set_read_timeout(Some(options.answer_timeout))?;
    stream.set_write_timeout(Some(options.answer_timeout))?;
    let mut writer = stream.try_clone()?;
    writer.write_all(&encode_line(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        bongos_version: options.bongos_version.clone(),
        client: options.client,
    })?)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let line = match read_line(&mut reader) {
        Ok(Some(line)) => line,
        Ok(None) => bail!(closed_message(&options.log)),
        Err(error) if is_timeout(&error) => bail!(not_answering(&options.log)),
        Err(error) => return Err(error),
    };
    let welcomed = match serde_json::from_str::<ServerMessage>(&line)
        .context("reading the engine host's answer")?
    {
        ServerMessage::Welcome {
            protocol: _,
            bongos_version,
            writer_pid,
            mode,
            generation,
        } => Welcomed {
            bongos_version,
            writer_pid,
            mode,
            generation,
        },
        ServerMessage::Refused { text } => bail!(text),
        other => bail!("the engine host answered {other:?} to hello"),
    };
    stream.set_read_timeout(None)?;
    let state = Arc::new(ConnectionState::default());
    let thread_state = state.clone();
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("engine-host-client".to_string())
        .spawn(move || read_messages(reader, thread_state, thread_shared))
        .context("starting the client's thread")?;
    Ok(Connection {
        stream,
        writer: Mutex::new(writer),
        next_id: AtomicU64::new(1),
        state,
        subscribed: AtomicBool::new(false),
        welcomed,
    })
}

impl StateSink for HostClient {
    fn apply(&self, ops: Vec<StateOp>, origin: Author) -> Task<Result<()>> {
        let request = self.request(HostCommand::WorkWrite {
            state_ops: ops,
            origin,
        });
        self.executor.spawn(async move {
            request.await?;
            Ok(())
        })
    }

    fn repo(&self, command: RepoCommand) -> Task<Result<RepoReply>> {
        let request = self.request(HostCommand::Repo(command));
        self.executor.spawn(async move {
            serde_json::from_value(request.await?).context("reading the engine host's answer")
        })
    }
}

fn is_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    })
}

fn read_messages(
    mut reader: BufReader<UnixStream>,
    state: Arc<ConnectionState>,
    shared: Arc<ClientShared>,
) {
    loop {
        let line = match read_line(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                log::info!("the engine host connection ended: {error:#}");
                break;
            }
        };
        // The writer is heard from: it answers again.
        state.not_answering.store(false, Ordering::SeqCst);
        match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Reply { id, result }) => {
                if let Some(sender) = state.pending.lock().remove(&id) {
                    sender.send(result).ok();
                }
            }
            Ok(ServerMessage::Event { event }) => {
                if matches!(event, HostEvent::WriterMoving { .. }) {
                    shared.moving.store(true, Ordering::SeqCst);
                }
                shared.subscribers.lock().retain(|(topics, sender)| {
                    let wanted = topics
                        .as_ref()
                        .is_none_or(|topics| topics.contains(&event.topic()));
                    !wanted || sender.unbounded_send(event.clone()).is_ok()
                });
            }
            Ok(other) => log::warn!("unexpected message from the engine host: {other:?}"),
            Err(error) => log::warn!("unreadable message from the engine host: {error}"),
        }
    }
    state.closed.store(true, Ordering::SeqCst);
    // Every waiting request hears that the connection is gone.
    state.pending.lock().clear();
    if !shared.moving.load(Ordering::SeqCst) {
        // The writer is gone for good: the subscriptions end. Across a move they carry over.
        shared.subscribers.lock().clear();
    }
}
