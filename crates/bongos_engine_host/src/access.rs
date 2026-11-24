//! The way a window reaches a project's engines: [`EngineAccess::Local`] in the process that
//! holds the writer, where a command runs here, and [`EngineAccess::Remote`] in a client, where
//! it travels to the writer through [`HostClient`].

use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bongos_checks::host::{CHECKS_NAMESPACE, ChecksForwarderRegistry};
use bongos_project_repo::{ProjectRepo, StateSink};
use gpui::{App, AppContext as _, Task};

use crate::client::HostClient;
use crate::commands::{HostCommands, dispatch, local_identity};
use crate::protocol::HostCommand;

#[derive(Clone)]
pub enum EngineAccess {
    /// This process holds the project's writer.
    Local(ProjectRepo),
    /// Another process holds it.
    Remote(HostClient),
}

impl EngineAccess {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// The sink this process's writes go to when it is a client; `None` in the writer.
    pub fn sink(&self) -> Option<Arc<dyn StateSink>> {
        match self {
            Self::Local(_) => None,
            Self::Remote(client) => Some(Arc::new(client.clone())),
        }
    }

    /// Sends `command` to the writer, or runs it here when this process holds it. A repository
    /// command is refused here: the writer's own window calls the repository itself, with the
    /// person's marker.
    pub fn request(&self, command: HostCommand, cx: &mut App) -> Task<Result<serde_json::Value>> {
        match self {
            Self::Remote(client) => cx.background_spawn(client.request(command)),
            Self::Local(repo) => {
                let command = match command {
                    HostCommand::Repo(_) => {
                        return Task::ready(Err(anyhow!(
                            "the writer's window calls the repository directly"
                        )));
                    }
                    HostCommand::WorkWrite { state_ops, origin } => {
                        let repo = repo.clone();
                        return cx.background_spawn(async move {
                            repo.apply_state(state_ops, origin).await?;
                            Ok(serde_json::Value::Null)
                        });
                    }
                    command => command,
                };
                let peer = match local_identity() {
                    Ok(peer) => peer,
                    Err(error) => return Task::ready(Err(error)),
                };
                dispatch(repo, command, &peer, serde_json::Value::Null, cx)
            }
        }
    }

    /// Sends an extension command of `namespace` to the writer, or runs its handler here.
    pub fn extension(
        &self,
        namespace: &str,
        payload: serde_json::Value,
        cx: &mut App,
    ) -> Task<Result<serde_json::Value>> {
        match self {
            Self::Remote(client) => client.extension(namespace, payload),
            Self::Local(_) => {
                let Some(handler) = HostCommands::handler(namespace, cx) else {
                    return Task::ready(Err(anyhow!(
                        "this process serves no {namespace} commands"
                    )));
                };
                match local_identity() {
                    Ok(peer) => handler(payload, &peer, cx),
                    Err(error) => Task::ready(Err(error)),
                }
            }
        }
    }

    /// Points task checks' writing commands at the writer: a client registers a forwarder that
    /// sends them as `checks` extension commands, the writer clears it so they run here.
    pub fn hook_up_checks(&self, cx: &mut App) {
        match self {
            Self::Local(_) => ChecksForwarderRegistry::clear(cx),
            Self::Remote(client) => {
                let client = client.clone();
                ChecksForwarderRegistry::set(
                    Rc::new(move |payload, _cx: &mut App| {
                        client.extension(CHECKS_NAMESPACE, payload)
                    }),
                    cx,
                );
            }
        }
    }
}

/// The [`EngineAccess`] of every project open in this process, by project id.
#[derive(Default)]
pub struct EngineAccesses(std::collections::BTreeMap<String, EngineAccess>);

impl gpui::Global for EngineAccesses {}

impl EngineAccesses {
    pub fn set(project_id: impl Into<String>, access: EngineAccess, cx: &mut App) {
        cx.default_global::<Self>()
            .0
            .insert(project_id.into(), access);
    }

    pub fn get(project_id: &str, cx: &App) -> Option<EngineAccess> {
        cx.try_global::<Self>()?.0.get(project_id).cloned()
    }

    /// The access of the only project open, `None` with none or several.
    pub fn only(cx: &App) -> Option<EngineAccess> {
        let accesses = &cx.try_global::<Self>()?.0;
        match accesses.len() {
            1 => accesses.values().next().cloned(),
            _ => None,
        }
    }

    /// The access of every project, by id.
    pub fn all(cx: &App) -> Vec<(String, EngineAccess)> {
        cx.try_global::<Self>()
            .map(|accesses| {
                accesses
                    .0
                    .iter()
                    .map(|(id, access)| (id.clone(), access.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}
