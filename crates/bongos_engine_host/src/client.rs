//! A process that does not hold a project's writer reaches it through [`HostClient`]: its writes
//! become requests on the project's socket, and it hears of changes through the events. A writer
//! that does not answer a request in time leaves the client read-only, every write refused with
//! "the engine host is not answering; see <log>", until the writer is heard from again.

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

/// What a client says when the writer does not answer in time.
pub fn not_answering(log: &Path) -> String {
    format!("the engine host is not answering; see {}", log.display())
}

/// How a client connects.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub client: ClientKind,
    /// The version this process runs; the server refuses any other.
    pub bongos_version: String,
    pub answer_timeout: Duration,
    /// The writer's log, named when it does not answer.
    pub log: PathBuf,
}

impl ClientOptions {
    pub fn new(client: ClientKind, bongos_version: impl Into<String>, log: PathBuf) -> Self {
        Self {
            client,
            bongos_version: bongos_version.into(),
            answer_timeout: ANSWER_TIMEOUT,
            log,
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

/// What the reading thread shares with the client.
#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<u64, oneshot::Sender<HostResult>>>,
    subscribers: Mutex<Vec<(Option<Vec<HostTopic>>, mpsc::UnboundedSender<HostEvent>)>>,
    /// Set when a request timed out; cleared when the writer is heard from.
    not_answering: AtomicBool,
    closed: AtomicBool,
}

struct Inner {
    stream: UnixStream,
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    shared: Arc<Shared>,
    subscribed: AtomicBool,
    options: ClientOptions,
    welcomed: Welcomed,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Ends the reading thread.
        self.stream.shutdown(std::net::Shutdown::Both).ok();
    }
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
            Ok(None) => bail!(
                "the engine host closed the connection; see {}",
                options.log.display()
            ),
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
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("engine-host-client".to_string())
            .spawn(move || read_messages(reader, thread_shared))
            .context("starting the client's thread")?;
        Ok(Self {
            inner: Arc::new(Inner {
                stream,
                writer: Mutex::new(writer),
                next_id: AtomicU64::new(1),
                shared,
                subscribed: AtomicBool::new(false),
                options,
                welcomed,
            }),
            executor,
        })
    }

    pub fn welcomed(&self) -> &Welcomed {
        &self.inner.welcomed
    }

    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    /// Whether writes are refused because the writer did not answer in time.
    pub fn is_read_only(&self) -> bool {
        self.inner.shared.not_answering.load(Ordering::SeqCst)
    }

    /// Whether the writer ended the connection.
    pub fn is_closed(&self) -> bool {
        self.inner.shared.closed.load(Ordering::SeqCst)
    }

    /// Sends `command` and resolves with the writer's answer. The socket is written when the
    /// future is first polled, never on the caller's thread.
    pub fn request(
        &self,
        command: HostCommand,
    ) -> impl std::future::Future<Output = Result<serde_json::Value>> + Send + 'static {
        let inner = self.inner.clone();
        let executor = self.executor.clone();
        async move {
            let (id, answer) = Self::send_request(&inner, command)?;
            let timeout = executor.timer(inner.options.answer_timeout).fuse();
            futures::pin_mut!(timeout);
            let mut answer = answer.fuse();
            futures::select! {
                result = answer => match result {
                    Ok(HostResult::Ok(value)) => Ok(value),
                    Ok(HostResult::Refused(text)) => Err(anyhow!(text)),
                    Err(_) => Err(anyhow!(
                        "the engine host closed the connection; see {}",
                        inner.options.log.display()
                    )),
                },
                _ = timeout => {
                    inner.shared.pending.lock().remove(&id);
                    inner.shared.not_answering.store(true, Ordering::SeqCst);
                    Err(anyhow!(not_answering(&inner.options.log)))
                }
            }
        }
    }

    fn send_request(
        inner: &Inner,
        command: HostCommand,
    ) -> Result<(u64, oneshot::Receiver<HostResult>)> {
        if inner.shared.not_answering.load(Ordering::SeqCst) {
            bail!(not_answering(&inner.options.log));
        }
        if inner.shared.closed.load(Ordering::SeqCst) {
            bail!(
                "the engine host closed the connection; see {}",
                inner.options.log.display()
            );
        }
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        inner.shared.pending.lock().insert(id, sender);
        let line = encode_line(&ClientMessage::Request { id, command })?;
        let written = {
            let mut writer = inner.writer.lock();
            writer.write_all(&line).and_then(|()| writer.flush())
        };
        if let Err(error) = written {
            inner.shared.pending.lock().remove(&id);
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut
            {
                inner.shared.not_answering.store(true, Ordering::SeqCst);
                bail!(not_answering(&inner.options.log));
            }
            return Err(anyhow!(error).context("sending a request to the engine host"));
        }
        Ok((id, receiver))
    }

    /// The writer's events of `topics` (every topic when `None`) from now on.
    pub fn subscribe(
        &self,
        topics: Option<Vec<HostTopic>>,
    ) -> Result<mpsc::UnboundedReceiver<HostEvent>> {
        let (sender, receiver) = mpsc::unbounded();
        self.inner.shared.subscribers.lock().push((topics, sender));
        if !self.inner.subscribed.swap(true, Ordering::SeqCst) {
            let line = encode_line(&ClientMessage::Subscribe { topics: Vec::new() })?;
            let mut writer = self.inner.writer.lock();
            writer.write_all(&line)?;
            writer.flush()?;
        }
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

fn read_messages(mut reader: BufReader<UnixStream>, shared: Arc<Shared>) {
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
        shared.not_answering.store(false, Ordering::SeqCst);
        match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Reply { id, result }) => {
                if let Some(sender) = shared.pending.lock().remove(&id) {
                    sender.send(result).ok();
                }
            }
            Ok(ServerMessage::Event { event }) => {
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
    shared.closed.store(true, Ordering::SeqCst);
    // Every waiting request hears that the connection is gone.
    shared.pending.lock().clear();
    shared.subscribers.lock().clear();
}
