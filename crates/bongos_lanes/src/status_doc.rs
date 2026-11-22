//! The `lanes-status 1` document: what `bongos lanes status --json` prints and what the engine
//! host answers a `status` request with besides its text. Every field of a lane is always
//! present (`null` when it has no value); `master_split` is present only while master is split,
//! so the schema stays compatible when a later version adds a field.

use bongos_lanes_model::{LaneId, LaneState, TaskId};
use bongos_project_repo::{SplitMaster, WAITING_MASTER_SPLIT};
use serde_json::{Value, json};

use crate::lane_control::{DispatcherPhase, LanesStatus};

/// The `schema` field of the document.
pub const STATUS_SCHEMA: &str = "lanes-status 1";

/// What the document needs besides the lanes' own status.
pub struct StatusFacts<'a> {
    pub project: &'a str,
    /// `ProjectRepo::split_master()`.
    pub split: Option<&'a SplitMaster>,
    /// The goal of a task.
    pub goal: &'a dyn Fn(&TaskId) -> Option<String>,
    /// The lifecycle of a run (`running`, `halted`, ...).
    pub run_lifecycle: &'a dyn Fn(&str) -> Option<String>,
    /// Whether the lane's job waits on a landing now.
    pub waits_on_landing: &'a dyn Fn(&LaneId) -> bool,
}

fn state_name(state: LaneState) -> &'static str {
    match state {
        LaneState::Running => "running",
        LaneState::Idle => "idle",
        LaneState::Paused => "paused",
        LaneState::Stopped => "stopped",
        LaneState::Halted => "halted",
        LaneState::Waiting => "waiting",
    }
}

fn phase_name(phase: Option<DispatcherPhase>) -> &'static str {
    match phase {
        None => "not started",
        Some(DispatcherPhase::Running) => "running",
        Some(DispatcherPhase::Pausing) => "pausing",
        Some(DispatcherPhase::Ended(_)) => "ended",
    }
}

/// The document for `status`.
pub fn status_doc(status: &LanesStatus, facts: &StatusFacts) -> Value {
    let count = |state: LaneState| {
        status
            .lanes
            .iter()
            .filter(|lane| lane.state == state)
            .count()
    };
    let lanes: Vec<Value> = status
        .lanes
        .iter()
        .map(|lane| {
            let split_wait = facts.split.is_some()
                && lane.state == LaneState::Running
                && (facts.waits_on_landing)(&lane.id);
            let reason = if split_wait {
                Some(WAITING_MASTER_SPLIT.to_string())
            } else {
                lane.reason.as_ref().map(|reason| reason.to_string())
            };
            json!({
                "id": lane.id.to_string(),
                "kind": lane.id.kind,
                "state": state_name(lane.state),
                "reason": reason,
                "task": lane.task.as_ref().map(|task| task.0.clone()),
                "goal": lane.task.as_ref().and_then(|task| (facts.goal)(task)),
                "run": lane.job.as_ref().map(|job| job.run_id.clone()),
                "workflow": lane.job.as_ref().map(|job| job.workflow.clone()),
                "run_lifecycle": lane.job.as_ref().and_then(|job| (facts.run_lifecycle)(&job.run_id)),
                "job_started": lane.job.as_ref().map(|job| job.started.clone()),
                "jobs_done": lane.jobs_done,
            })
        })
        .collect();
    let mut doc = json!({
        "schema": STATUS_SCHEMA,
        "project": facts.project,
        "header": {
            "dispatcher": phase_name(status.phase),
            "lanes": status.lanes.len(),
            "running": count(LaneState::Running),
            "idle": count(LaneState::Idle),
            "waiting": count(LaneState::Waiting),
            "paused": count(LaneState::Paused),
            "stopped": count(LaneState::Stopped),
            "halted": count(LaneState::Halted),
            "table_commit": status.table_commit,
            "table_wait": status.table_wait,
        },
        "lanes": lanes,
    });
    if let Some(split) = facts.split {
        doc["master_split"] = json!({
            "change_id": split.change_id,
            "main": split.main,
            "verified": split.verified,
        });
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane_control::LaneStatus;
    use bongos_lanes_model::JobRecord;
    use bongos_lanes_model::RunSubject;

    fn lane(index: u32, state: LaneState, task: Option<&str>, job: bool) -> LaneStatus {
        let id = LaneId::new("build", index);
        LaneStatus {
            id: id.clone(),
            state,
            reason: None,
            task: task.map(TaskId::new),
            pause_requested: false,
            stop_requested: false,
            asked: None,
            job: job.then(|| JobRecord {
                run_id: "run-1".to_string(),
                subject: RunSubject::Task(TaskId::new("t1")),
                workflow: "build".to_string(),
                started: "2026-01-01T00:00:00Z".to_string(),
                definition_commit: None,
                extension: None,
            }),
            jobs_done: 2,
        }
    }

    fn facts<'a>(split: Option<&'a SplitMaster>) -> StatusFacts<'a> {
        StatusFacts {
            project: "p1",
            split,
            goal: &|task| Some(format!("goal of {task}")),
            run_lifecycle: &|_| Some("running".to_string()),
            waits_on_landing: &|_| true,
        }
    }

    fn status() -> LanesStatus {
        LanesStatus {
            phase: Some(DispatcherPhase::Running),
            table_commit: Some("abc".to_string()),
            table_wait: None,
            lanes: vec![
                lane(1, LaneState::Running, Some("t1"), true),
                lane(2, LaneState::Idle, None, false),
            ],
        }
    }

    #[test]
    fn every_field_is_present_and_the_split_only_when_set() {
        let doc = status_doc(&status(), &facts(None));
        assert_eq!(doc["schema"], "lanes-status 1");
        assert_eq!(doc["header"]["running"], 1);
        assert_eq!(doc["header"]["idle"], 1);
        let first = doc["lanes"][0].as_object().unwrap();
        for key in [
            "id",
            "kind",
            "state",
            "reason",
            "task",
            "goal",
            "run",
            "workflow",
            "run_lifecycle",
            "job_started",
            "jobs_done",
        ] {
            assert!(first.contains_key(key), "missing {key}");
        }
        assert_eq!(doc["lanes"][1]["task"], Value::Null);
        assert!(doc.get("master_split").is_none());
    }

    #[test]
    fn a_split_master_adds_its_field_and_the_landing_wait_keeps_the_task() {
        let split = SplitMaster {
            change_id: "c1".to_string(),
            main: "m".to_string(),
            verified: "v".to_string(),
            old: "o".to_string(),
        };
        let doc = status_doc(&status(), &facts(Some(&split)));
        assert_eq!(doc["master_split"]["change_id"], "c1");
        assert_eq!(doc["lanes"][0]["state"], "running");
        assert_eq!(doc["lanes"][0]["task"], "t1");
        assert_eq!(doc["lanes"][0]["reason"], "waiting: master is split");
        assert_eq!(doc["lanes"][1]["reason"], Value::Null);
    }
}
