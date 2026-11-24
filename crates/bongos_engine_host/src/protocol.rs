//! The engine host's protocol, version 1: serde JSON, one object per line, over the project's
//! Unix socket. A client says `Hello` first and the server answers `Welcome` (or `Refused` when
//! the protocol or the bongos version differ); then the client sends `Request`s, each answered
//! by a `Reply` with its id, and may `Subscribe` to the stream of `HostEvent`s.

use bongos_project_repo::{ClientKind, RepoCommand, StateOp};
use bongos_project_repo_model::Author;
use serde::{Deserialize, Serialize};

/// The protocol this build speaks.
pub const PROTOCOL_VERSION: u32 = bongos_version::ENGINE_HOST_PROTOCOL;

/// The longest line either side reads; a longer one ends the connection.
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// What the server answers to a client whose protocol or bongos version differs from its own.
pub fn version_refusal(server_version: &str) -> String {
    format!(
        "the engine host runs bongos {server_version}; stop it with bongos host stop when its jobs end"
    )
}

/// Which kind of process holds the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostMode {
    /// A window that opened the project first.
    Window,
    /// A separate host process.
    Host,
}

/// A line from a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "message", rename_all = "kebab-case")]
pub enum ClientMessage {
    Hello {
        protocol: u32,
        bongos_version: String,
        client: ClientKind,
    },
    Request {
        id: u64,
        command: HostCommand,
    },
    /// From now on the server sends the events of `topics`; an empty list is every topic.
    Subscribe {
        topics: Vec<HostTopic>,
    },
}

/// A line from the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "message", rename_all = "kebab-case")]
pub enum ServerMessage {
    Welcome {
        protocol: u32,
        bongos_version: String,
        writer_pid: u32,
        mode: HostMode,
        generation: u64,
    },
    /// The answer to a `Hello` the server does not take; the connection ends after it.
    Refused {
        text: String,
    },
    Reply {
        id: u64,
        result: HostResult,
    },
    Event {
        event: HostEvent,
    },
}

/// The outcome of one request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostResult {
    Ok(serde_json::Value),
    Refused(String),
}

/// A request to the process that holds the writer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostCommand {
    /// State ops applied in one transaction through the host boundary. `origin` only labels
    /// history and is taken as received: at most a claimed person.
    WorkWrite {
        state_ops: Vec<StateOp>,
        origin: Author,
    },
    /// A repository command through the host boundary, which trusts its person claim only from
    /// a window of the same executable.
    Repo(RepoCommand),
    /// A command of the workflow engine; `payload` is the command's own arguments.
    Workflow {
        verb: WorkflowVerb,
        payload: serde_json::Value,
    },
    /// A `lanes:` command; `payload` is the command's own arguments.
    Lanes {
        verb: LanesVerb,
        payload: serde_json::Value,
    },
    /// A command another feature registered with `HostCommands::register`.
    Extension {
        namespace: String,
        payload: serde_json::Value,
    },
    Host(HostCtl),
}

/// The workflow engine's commands, defined with the engine.
pub use bongos_workflow::WorkflowVerb;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LanesVerb {
    Start,
    Stop,
    Pause,
    Resume,
    Adopt,
    DryRun,
    Pick,
    Status,
    AcceptGraph,
}

/// Control of the host itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostCtl {
    Status,
    Stop { at_job_boundary: bool },
    Handover,
}

/// The kinds of event a client can subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostTopic {
    State,
    Runs,
    Lanes,
    Notices,
    Writer,
}

/// Something that changed in the writer; a client re-reads the store when it hears of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum HostEvent {
    StateChanged {
        generation: u64,
        task_ids: Vec<String>,
        namespaces: Vec<String>,
    },
    RunChanged {
        run: String,
        subject: String,
        lifecycle: String,
    },
    LanesChanged,
    Notice {
        text: String,
    },
    WriterMoving {
        to_pid: u32,
    },
    WriterStopping {
        reason: String,
    },
}

impl HostEvent {
    pub fn topic(&self) -> HostTopic {
        match self {
            HostEvent::StateChanged { .. } => HostTopic::State,
            HostEvent::RunChanged { .. } => HostTopic::Runs,
            HostEvent::LanesChanged => HostTopic::Lanes,
            HostEvent::Notice { .. } => HostTopic::Notices,
            HostEvent::WriterMoving { .. } | HostEvent::WriterStopping { .. } => HostTopic::Writer,
        }
    }
}

/// `message` as one line, newline included.
pub fn encode_line<T: Serialize>(message: &T) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}
