//! The commands the writer serves, and the one place a request becomes a write: state ops
//! through `HostBoundary::apply`, repository commands through `HostBoundary::accept`, and the
//! commands other features registered with [`HostCommands::register`]. The server never writes
//! state itself on a client's behalf.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use bongos_project_repo::{ClientKind, HostBoundary, PeerIdentity, ProjectRepo, peer_identity};
use gpui::{App, AppContext as _, Global, Task};

use crate::protocol::{HostCommand, HostCtl, LanesVerb, WorkflowVerb};

/// Serves one namespace of [`HostCommand::Extension`]: the payload, the peer that sent it, and
/// the app of the process that holds the writer.
pub type HostHandler =
    Rc<dyn Fn(serde_json::Value, &PeerIdentity, &mut App) -> Task<Result<serde_json::Value>>>;

/// Serves the verbs of [`HostCommand::Workflow`] or [`HostCommand::Lanes`].
pub type VerbHandler<V> =
    Rc<dyn Fn(V, serde_json::Value, &PeerIdentity, &mut App) -> Task<Result<serde_json::Value>>>;

/// The commands this process serves while it holds a project's writer.
#[derive(Default)]
pub struct HostCommands {
    extensions: HashMap<String, HostHandler>,
    workflow: Option<VerbHandler<WorkflowVerb>>,
    lanes: Option<VerbHandler<LanesVerb>>,
}

impl Global for HostCommands {}

impl HostCommands {
    /// Serves the extension commands of `namespace` with `handler`, replacing an earlier one.
    pub fn register(namespace: impl Into<String>, handler: HostHandler, cx: &mut App) {
        cx.default_global::<Self>()
            .extensions
            .insert(namespace.into(), handler);
    }

    /// Serves the workflow engine's commands with `handler`.
    pub fn serve_workflow(handler: VerbHandler<WorkflowVerb>, cx: &mut App) {
        cx.default_global::<Self>().workflow = Some(handler);
    }

    /// Serves the `lanes:` commands with `handler`.
    pub fn serve_lanes(handler: VerbHandler<LanesVerb>, cx: &mut App) {
        cx.default_global::<Self>().lanes = Some(handler);
    }

    pub fn handler(namespace: &str, cx: &App) -> Option<HostHandler> {
        cx.try_global::<Self>()
            .and_then(|commands| commands.extensions.get(namespace).cloned())
    }

    fn workflow(cx: &App) -> Option<VerbHandler<WorkflowVerb>> {
        cx.try_global::<Self>()
            .and_then(|commands| commands.workflow.clone())
    }

    fn lanes(cx: &App) -> Option<VerbHandler<LanesVerb>> {
        cx.try_global::<Self>()
            .and_then(|commands| commands.lanes.clone())
    }
}

/// The identity of this process as its own peer: a window running this executable. Read through
/// a socket pair, so it is what `peer_identity` reports for this process, not assumed.
pub fn local_identity() -> Result<PeerIdentity> {
    static LOCAL: OnceLock<Option<PeerIdentity>> = OnceLock::new();
    LOCAL
        .get_or_init(|| {
            let (ours, _theirs) = std::os::unix::net::UnixStream::pair().ok()?;
            peer_identity(&ours, ClientKind::Window).ok()
        })
        .clone()
        .ok_or_else(|| anyhow!("could not read this process's own identity"))
}

/// Runs `command` from `peer` in the process that holds `repo`'s writer. `status` answers
/// `Host(Status)`.
pub(crate) fn dispatch(
    repo: &ProjectRepo,
    command: HostCommand,
    peer: &PeerIdentity,
    status: serde_json::Value,
    cx: &mut App,
) -> Task<Result<serde_json::Value>> {
    match command {
        HostCommand::WorkWrite { state_ops, origin } => {
            let boundary = HostBoundary::new(repo.clone());
            let peer = peer.clone();
            cx.background_spawn(async move {
                boundary.apply(state_ops, origin, &peer)?;
                Ok(serde_json::Value::Null)
            })
        }
        HostCommand::Repo(command) => {
            let reply = HostBoundary::new(repo.clone()).accept(command, peer, cx);
            cx.background_spawn(async move { Ok(serde_json::to_value(reply.await?)?) })
        }
        HostCommand::Extension { namespace, payload } => {
            match HostCommands::handler(&namespace, cx) {
                Some(handler) => handler(payload, peer, cx),
                None => Task::ready(Err(anyhow!(
                    "this engine host serves no {namespace} commands"
                ))),
            }
        }
        HostCommand::Workflow { verb, payload } => match HostCommands::workflow(cx) {
            Some(handler) => handler(verb, payload, peer, cx),
            None => Task::ready(Err(anyhow!("this engine host serves no workflow commands"))),
        },
        HostCommand::Lanes { verb, payload } => match HostCommands::lanes(cx) {
            Some(handler) => handler(verb, payload, peer, cx),
            None => Task::ready(Err(anyhow!("this engine host serves no lanes commands"))),
        },
        HostCommand::Host(HostCtl::Status) => Task::ready(Ok(status)),
        HostCommand::Host(control) => {
            Task::ready(Err(anyhow!("this engine host does not take {control:?}")))
        }
    }
}
