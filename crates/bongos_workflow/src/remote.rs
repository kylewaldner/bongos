//! The workflow commands as they travel between processes. The engine runs only in the process
//! that holds a project's writer; a window that does not hold it sends the command there as a
//! [`WorkflowVerb`] with a [`WorkflowPayload`] (through the [`WorkflowRemote`] the app installs),
//! and the writer runs it with [`execute`], answering with the text to show.

use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use settings::Settings as _;
use std::rc::Rc;

use crate::WorkflowsSettings;
use crate::actions::OFF_MESSAGE;
use crate::engine::WorkflowEngine;
use crate::lifecycle::{Refusal, RunId, StartRequest};

/// The commands of the workflow engine a window sends to the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowVerb {
    Start,
    Pause,
    Halt,
    Resume,
    Cancel,
    Retry,
    Restart,
    Approve,
    Revise,
    ClearFinished,
}

/// What a command acts on and with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowPayload {
    /// The task at point in the sending window.
    pub task_id: Option<String>,
    /// The run asked about; else the writer takes the active run of the task, or the only one.
    pub run: Option<String>,
    /// The workflow `Start` runs; `None` is the one the task's todo names.
    pub workflow: Option<String>,
    /// `Retry` reads the definition again.
    pub fresh: bool,
    /// `Approve` with one field edited: its name and new value.
    pub edit: Option<(String, Value)>,
    /// What `Revise` asks to change.
    pub comment: Option<String>,
}

/// How this process reaches the writer of the projects it does not hold.
pub trait WorkflowRemote {
    /// Whether a command on `task_id` (or on the only run) is the business of another process's
    /// writer: this process does not run the engine for it.
    fn is_remote(&self, task_id: Option<&str>, cx: &App) -> bool;

    /// Sends the command to the writer; resolves with the text to show.
    fn send(
        &self,
        verb: WorkflowVerb,
        payload: WorkflowPayload,
        cx: &mut App,
    ) -> gpui::Task<anyhow::Result<String>>;
}

/// The installed [`WorkflowRemote`], if the app has one.
#[derive(Clone)]
pub struct InstalledWorkflowRemote(pub Rc<dyn WorkflowRemote>);

impl Global for InstalledWorkflowRemote {}

impl InstalledWorkflowRemote {
    pub fn install(remote: Rc<dyn WorkflowRemote>, cx: &mut App) {
        cx.set_global(Self(remote));
    }

    pub(crate) fn get(cx: &App) -> Option<Rc<dyn WorkflowRemote>> {
        cx.try_global::<Self>().map(|installed| installed.0.clone())
    }
}

/// The run `payload` asks about: the one it names, else the active run of its task, else the only
/// active run.
fn run_of(engine: &WorkflowEngine, payload: &WorkflowPayload) -> Result<RunId, String> {
    if let Some(run) = &payload.run {
        return Ok(run.clone());
    }
    if let Some(run) = payload
        .task_id
        .as_deref()
        .and_then(|task| engine.active_run_id(task))
    {
        return Ok(run);
    }
    match engine.active_runs().as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(crate::commands::NO_RUN.to_string()),
    }
}

fn refused(refusal: Refusal) -> String {
    refusal.to_string()
}

/// Runs `verb` in this process's engine, as the writer. Resolves with the text to show: what
/// was done, or the engine's refusal.
pub fn execute(
    verb: WorkflowVerb,
    payload: WorkflowPayload,
    cx: &mut App,
) -> Result<String, String> {
    if !WorkflowsSettings::get_global(cx).enabled {
        return Err(OFF_MESSAGE.to_string());
    }
    let engine = WorkflowEngine::ensure(cx).ok_or_else(|| OFF_MESSAGE.to_string())?;
    if verb == WorkflowVerb::Start {
        let task = payload
            .task_id
            .clone()
            .ok_or_else(|| crate::commands::NO_TASK.to_string())?;
        let request = StartRequest {
            workflow: payload.workflow,
            ..StartRequest::new(task)
        };
        return engine
            .start(request, cx)
            .map(|run| format!("Started the run {run}"))
            .map_err(refused);
    }
    if verb == WorkflowVerb::ClearFinished {
        let outcome = engine.clear_finished(cx);
        return if outcome.cleared.is_empty() && outcome.failed.is_empty() {
            Ok("No finished runs to clear".to_string())
        } else {
            let mut notes = vec![format!("Cleared {} finished runs", outcome.cleared.len())];
            notes.extend(
                outcome
                    .failed
                    .iter()
                    .map(|(run, error)| format!("Could not clear run {run}: {error}")),
            );
            Ok(notes.join(". "))
        };
    }
    let run = run_of(&engine, &payload)?;
    match verb {
        WorkflowVerb::Pause => engine
            .pause(&run, cx)
            .map(|()| "Pausing: running steps finish first".to_string())
            .map_err(refused),
        WorkflowVerb::Halt => engine
            .halt(&run, cx)
            .map(|()| "Halted".to_string())
            .map_err(refused),
        WorkflowVerb::Resume => engine
            .resume(&run, cx)
            .map(|()| "Workflow run resumed".to_string())
            .map_err(refused),
        WorkflowVerb::Cancel => engine
            .cancel(&run, cx)
            .map(|()| "Workflow run cancelled".to_string())
            .map_err(refused),
        WorkflowVerb::Retry => engine
            .retry(&run, None, payload.fresh, cx)
            .map(|()| "Retrying the failed step".to_string())
            .map_err(refused),
        WorkflowVerb::Restart => engine
            .restart(&run, cx)
            .map(|_| "Workflow run restarted".to_string())
            .map_err(refused),
        WorkflowVerb::Approve => {
            let key = engine
                .open_gates(&run)
                .into_iter()
                .next()
                .ok_or_else(|| "The run is not waiting at a gate".to_string())?;
            engine
                .approve(&run, &key, payload.edit, cx)
                .map(|()| "Approved".to_string())
                .map_err(refused)
        }
        WorkflowVerb::Revise => {
            let key = engine
                .open_gates(&run)
                .into_iter()
                .next()
                .ok_or_else(|| "The run is not waiting at a gate".to_string())?;
            engine
                .revise(&run, &key, payload.comment.unwrap_or_default(), cx)
                .map(|()| "Sent back for revision".to_string())
                .map_err(refused)
        }
        WorkflowVerb::Start | WorkflowVerb::ClearFinished => unreachable!("handled above"),
    }
}
