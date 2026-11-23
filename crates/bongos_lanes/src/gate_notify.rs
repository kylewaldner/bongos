//! One desktop notification per gate a lane's run waits at. The engine host observes the
//! workflow engine; when a run of a lane waits on a person it notifies once for each open gate,
//! unless `lanes.desktop_notifications` is off or a window client is attached to the host (the
//! window shows the gate itself). A gate that opened while a client was connected is notified
//! once no client is any more, so a short command-line request does not swallow it.
//! [`GateNotifier`] records each notified gate in the store, so repeated events and a restarted
//! host stay quiet.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bongos_engine_host::{EngineHostProcess, WriterHosts};
use bongos_lanes_model::LanesState;
use bongos_project_repo::StateWriter;
use bongos_workflow::{RunStatus, WorkflowEngine, WorkflowEvent};
use bongos_workflow_model::record::NodeKey;
use futures::StreamExt as _;
use gpui::App;
use settings::Settings as _;

use crate::LanesSettings;
use crate::notify::GateNotifier;
use crate::store::{read_state, read_state_in};

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
    let pending = writer.transact(|tx| {
        let state = read_state_in(tx)?;
        let Some(lane) = lane_of_run(&state, run_id) else {
            return Ok(Vec::new());
        };
        let mut pending = Vec::new();
        for (key, question) in gates {
            let title = format!("{lane} needs you");
            pending.extend(notifier.notify_gate(
                tx,
                run_id,
                &key.to_string(),
                &title,
                question,
                now,
            )?);
        }
        Ok(pending)
    })?;
    for notification in &pending {
        notifier.send(notification);
    }
    Ok(pending.len())
}

fn lane_of_run(state: &LanesState, run_id: &str) -> Option<String> {
    state
        .lanes
        .values()
        .find(|lane| lane.job.as_ref().is_some_and(|job| job.run_id == run_id))
        .map(|lane| lane.id().to_string())
}

/// Starts notifying the gates of lane runs, through `notifier`, in the process that is the engine
/// host (a window shows its gates itself). A gate that opens while a client is connected is not
/// notified then: once no client is connected any more (a short command-line request ended, or the
/// last window went), every lane run still waiting on a person is notified for the gates not
/// notified yet.
pub fn observe_gates(notifier: GateNotifier, cx: &mut App) {
    let watching = Rc::new(Cell::new(false));
    WorkflowEngine::observe(cx, move |event, cx| {
        let WorkflowEvent::Changed { run, state, .. } = event;
        if *state != RunStatus::WaitingOnYou || !EngineHostProcess::is_running(cx) {
            return;
        }
        if client_attached(cx) {
            notify_after_clients_go(notifier.clone(), watching.clone(), cx);
            return;
        }
        let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() else {
            return;
        };
        let gates = match engine.lifecycle().state(run) {
            Some(state) => state.open_gates,
            None => return,
        };
        notify_run(&notifier, run, &gates, cx);
    });
}

/// Whether a client is connected to this process's socket.
fn client_attached(cx: &App) -> bool {
    cx.try_global::<WriterHosts>().is_some_and(|hosts| {
        hosts
            .hosts()
            .iter()
            .filter_map(|host| host.server())
            .any(|server| server.connection_count() > 0)
    })
}

/// Once no client is connected, notifies the open gates of every lane run waiting on a person.
/// One watch at a time: `watching` is set while one listens.
fn notify_after_clients_go(notifier: GateNotifier, watching: Rc<Cell<bool>>, cx: &mut App) {
    if watching.get() {
        return;
    }
    let Some(hosts) = cx.try_global::<WriterHosts>() else {
        return;
    };
    let changes: Vec<_> = hosts
        .hosts()
        .iter()
        .filter_map(|host| host.server())
        .map(|server| server.watch_connections())
        .collect();
    if changes.is_empty() {
        return;
    }
    watching.set(true);
    let mut changes = futures::stream::select_all(changes);
    cx.spawn(async move |cx| {
        // A client may have gone between the check and the watch.
        let mut gone = !cx.update(|cx| client_attached(cx));
        while !gone && changes.next().await.is_some() {
            gone = !cx.update(|cx| client_attached(cx));
        }
        watching.set(false);
        if gone {
            cx.update(|cx| notify_waiting_lane_runs(&notifier, cx));
        }
    })
    .detach();
}

/// Notifies the open gates of every lane run waiting on a person that were not notified yet.
fn notify_waiting_lane_runs(notifier: &GateNotifier, cx: &mut App) {
    if !EngineHostProcess::is_running(cx) {
        return;
    }
    let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() else {
        return;
    };
    let Some(writer) = sole_writer(cx) else {
        return;
    };
    let runs: Vec<String> = match writer.read(read_state) {
        Ok(state) => state
            .lanes
            .values()
            .filter_map(|lane| lane.job.as_ref().map(|job| job.run_id.clone()))
            .collect(),
        Err(error) => {
            log::warn!("lanes: reading the lanes for the gate notifications: {error:#}");
            return;
        }
    };
    for run in runs {
        let Some(state) = engine.lifecycle().state(&run) else {
            continue;
        };
        if RunStatus::of(&state) == RunStatus::WaitingOnYou {
            notify_run(notifier, &run, &state.open_gates, cx);
        }
    }
}

/// The writer of the only project repository open here.
fn sole_writer(cx: &App) -> Option<Arc<StateWriter>> {
    let repos = bongos_work::OpenProjectRepos::listed(cx);
    let [project] = repos.as_slice() else {
        return None;
    };
    project
        .repo()
        .state()
        .and_then(|state| state.writer())
        .cloned()
}

/// Notifies `gates` of the lane run `run`, with no client connected.
fn notify_run(notifier: &GateNotifier, run: &str, gates: &BTreeMap<NodeKey, String>, cx: &App) {
    let Some(writer) = sole_writer(cx) else {
        return;
    };
    let settings = LanesSettings::get_global(cx);
    let enabled = settings.enabled && settings.desktop_notifications;
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(error) = notify_open_gates(
        notifier,
        &writer,
        run,
        gates,
        enabled,
        client_attached(cx),
        &now,
    ) {
        log::warn!("lanes: the gate notification of run {run} failed: {error:#}");
    }
}
