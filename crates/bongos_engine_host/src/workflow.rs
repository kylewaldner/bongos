//! The workflow engine's commands over the socket: the writer serves them with the engine it
//! runs, and a window that attached to another's writer sends them there through
//! [`EngineAccess`].

use std::rc::Rc;

use anyhow::anyhow;
use bongos_workflow::{
    InstalledWorkflowRemote, WorkflowPayload, WorkflowRemote, WorkflowVerb, execute,
};
use gpui::{App, AppContext as _, Task};

use crate::access::{EngineAccess, EngineAccesses};
use crate::commands::HostCommands;
use crate::protocol::HostCommand;

/// Serves the workflow commands in the writer with the process's engine. Whether the peer may
/// attest a person decides the commands only a person may give (allowing a run without the
/// sandbox).
pub fn register_workflow(cx: &mut App) {
    HostCommands::serve_workflow(
        Rc::new(|verb, payload, peer, cx| {
            let person = peer.may_attest_person();
            let result = serde_json::from_value::<WorkflowPayload>(payload)
                .map_err(|error| format!("reading the workflow command: {error}"))
                .and_then(|payload| execute(verb, payload, person, cx));
            Task::ready(match result {
                Ok(message) => Ok(serde_json::Value::String(message)),
                Err(message) => Err(anyhow!(message)),
            })
        }),
        cx,
    );
}

/// The access of the project `task_id` belongs to, else of the only project open.
fn access_for(task_id: Option<&str>, cx: &App) -> Option<EngineAccess> {
    let Some(task_id) = task_id else {
        return EngineAccesses::only(cx);
    };
    let tree = bongos_work::WorkStore::global(cx)?.read(cx).snapshot()?;
    let task = tree.task_by_id(task_id)?;
    let repo = bongos_work::project_repo_of(task, cx)?;
    EngineAccesses::get(&repo.location().id, cx)
}

struct SocketWorkflow;

impl WorkflowRemote for SocketWorkflow {
    fn is_remote(&self, task_id: Option<&str>, cx: &App) -> bool {
        access_for(task_id, cx).is_some_and(|access| !access.is_local())
    }

    fn send(
        &self,
        verb: WorkflowVerb,
        payload: WorkflowPayload,
        cx: &mut App,
    ) -> Task<anyhow::Result<String>> {
        let Some(access) = access_for(payload.task_id.as_deref(), cx) else {
            return Task::ready(Err(anyhow!("no engine host serves this project")));
        };
        let payload = match serde_json::to_value(&payload) {
            Ok(payload) => payload,
            Err(error) => return Task::ready(Err(error.into())),
        };
        let sending = access.request(HostCommand::Workflow { verb, payload }, cx);
        cx.background_spawn(async move {
            let answer = sending.await?;
            Ok(answer.as_str().unwrap_or_default().to_string())
        })
    }
}

/// Makes the workflow commands travel to the writer when this process does not hold it.
pub fn install_workflow_remote(cx: &mut App) {
    InstalledWorkflowRemote::install(Rc::new(SocketWorkflow), cx);
}
