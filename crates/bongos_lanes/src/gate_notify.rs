//! One desktop notification per gate a lane's run waits at. The engine host observes the
//! workflow engine; when a run of a lane waits on a person it notifies once for each open gate,
//! unless `lanes.desktop_notifications` is off or a window client is attached to the host (the
//! window shows the gate itself). [`GateNotifier`] records each notified gate in the store, so
//! repeated events and a restarted host stay quiet.

use std::collections::BTreeMap;

use anyhow::Result;
use bongos_engine_host::{EngineHostProcess, WriterHosts};
use bongos_lanes_model::LanesState;
use bongos_project_repo::StateWriter;
use bongos_workflow::{RunStatus, WorkflowEngine, WorkflowEvent};
use bongos_workflow_model::record::NodeKey;
use gpui::App;
use settings::Settings as _;

use crate::LanesSettings;
use crate::notify::GateNotifier;
use crate::store::read_state_in;

/// Notifies the gates in `gates` of the run `run_id` that no notification went out for yet, when
/// the run belongs to a lane in the state read inside the transaction. Returns how many
/// notifications went out. Nothing goes out, and nothing is recorded, while `enabled` is false or
/// a window client is attached.
pub fn notify_open_gates(
    notifier: &GateNotifier,
    writer: &StateWriter,
    run_id: &str,
    gates: &BTreeMap<NodeKey, String>,
    enabled: bool,
    client_attached: bool,
    now: &str,
) -> Result<usize> {
    if !enabled || client_attached || gates.is_empty() {
        return Ok(0);
    }
    writer.transact(|tx| {
        let state = read_state_in(tx)?;
        let Some(lane) = lane_of_run(&state, run_id) else {
            return Ok(0);
        };
        let mut sent = 0;
        for (key, question) in gates {
            let title = format!("{lane} needs you");
            if notifier.notify_gate(tx, run_id, &key.to_string(), &title, question, now)? {
                sent += 1;
            }
        }
        Ok(sent)
    })
}

fn lane_of_run(state: &LanesState, run_id: &str) -> Option<String> {
    state
        .lanes
        .values()
        .find(|lane| lane.job.as_ref().is_some_and(|job| job.run_id == run_id))
        .map(|lane| lane.id().to_string())
}

/// Starts notifying the gates of lane runs, through `notifier`, in the process that is the engine
/// host (a window shows its gates itself).
pub fn observe_gates(notifier: GateNotifier, cx: &mut App) {
    WorkflowEngine::observe(cx, move |event, cx| {
        let WorkflowEvent::Changed { run, state, .. } = event;
        if *state != RunStatus::WaitingOnYou || !EngineHostProcess::is_running(cx) {
            return;
        }
        let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() else {
            return;
        };
        let gates = match engine.lifecycle().state(run) {
            Some(state) => state.open_gates,
            None => return,
        };
        let repos = bongos_work::OpenProjectRepos::listed(cx);
        let [project] = repos.as_slice() else {
            return;
        };
        let Some(writer) = project.repo().state().and_then(|state| state.writer()) else {
            return;
        };
        let client_attached = cx.try_global::<WriterHosts>().is_some_and(|hosts| {
            hosts
                .hosts()
                .iter()
                .filter_map(|host| host.server())
                .any(|server| server.connection_count() > 0)
        });
        let enabled = LanesSettings::get_global(cx).enabled
            && LanesSettings::get_global(cx).desktop_notifications;
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(error) = notify_open_gates(
            &notifier,
            writer,
            run,
            &gates,
            enabled,
            client_attached,
            &now,
        ) {
            log::warn!("lanes: the gate notification of run {run} failed: {error:#}");
        }
    });
}
