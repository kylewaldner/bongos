use bongos_lanes_model::{Author, LaneId, LaneState, TaskId};
use bongos_project_repo::{Location, StateStore};
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::dispatcher::{DispatcherRecord, put_dispatcher};
use crate::lane_control::{DispatcherPhase, RunEnd};
use crate::store::write_state;
use crate::test_support::{lanes_state, writer};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

#[test]
fn the_arguments_are_json_and_a_lane() {
    assert_eq!(parse_args(&[]).unwrap(), ReadArgs::default());
    let parsed = parse_args(&args(&["--json", "--lane", "build-2"])).unwrap();
    assert!(parsed.json);
    assert_eq!(parsed.lane, Some(LaneId::new("build", 2)));
    for bad in [&["--lane"][..], &["--lane", "nolane"], &["--bogus"]] {
        assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn a_reader_shows_the_lanes_the_writer_recorded() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| write_state(tx, &lanes_state()))
        .unwrap();
    let folder = root.path().join("project");
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state: folder.join("state"),
        folder,
        remote: None,
        converted_from: None,
    };
    let reader = StateStore::open_reader(&location).unwrap();

    let status = reader.read(status_from_read).unwrap();
    assert_eq!(status.phase, None);
    let ids: Vec<String> = status
        .lanes
        .iter()
        .map(|lane| lane.id.to_string())
        .collect();
    assert_eq!(ids, ["build-1", "docs-1"]);
    let build = &status.lanes[0];
    assert_eq!(build.state, LaneState::Running);
    assert_eq!(build.task, Some(TaskId::new("feature")));
    assert_eq!(build.jobs_done, 2);

    writer
        .transact(|tx| {
            put_dispatcher(
                tx,
                &DispatcherRecord {
                    run_id: "run-1".to_string(),
                    host: "host-1".to_string(),
                    started: "2026-03-01T10:00:00Z".to_string(),
                    by: Author::System,
                    bound: None,
                    unattended: None,
                    definition_commit: None,
                    table_commit: Some("abc".to_string()),
                    table_wait: None,
                    stopped_by: None,
                    ended: Some(RunEnd::Cancelled),
                },
            )
        })
        .unwrap();
    let status = reader.read(status_from_read).unwrap();
    assert_eq!(
        status.phase,
        Some(DispatcherPhase::Ended(RunEnd::Cancelled))
    );
    assert_eq!(status.table_commit.as_deref(), Some("abc"));
}

#[test]
fn the_status_text_names_each_lane_and_a_split_master() {
    let doc = json!({
        "header": {
            "dispatcher": "running", "lanes": 2, "running": 1, "idle": 1, "waiting": 0,
            "paused": 0, "stopped": 0, "halted": 0,
        },
        "master_split": { "change_id": "c1" },
        "lanes": [
            { "id": "build-1", "state": "running", "task": "t1", "goal": "Ship", "run": "r1",
              "workflow": "build", "run_lifecycle": "running", "reason": null, "jobs_done": 3 },
            { "id": "build-2", "state": "idle", "task": null, "jobs_done": 0 },
        ],
    });
    let text = status_text(&doc);
    assert!(text.starts_with("lanes: 2 (dispatcher running): 1 running, 1 idle"));
    assert!(text.contains("master is split"));
    assert!(text.contains("build-1  running  t1 (Ship)  run r1 of build (running)  3 jobs done"));
    assert!(text.contains("build-2  idle  0 jobs done"));
}

/// The picker's inputs over `tasks` (all top-level) with one build lane.
fn loaded(tasks: Vec<bongos_lanes_model::PickTask>) -> Loaded {
    let top = tasks.iter().map(|task| task.id.clone()).collect();
    Loaded {
        graph: bongos_lanes_model::PickGraph::new(top, tasks),
        table: parse(&format!(
            "schema = 1\n{}",
            crate::dispatcher_tests::builds(1)
        ))
        .unwrap(),
        options: PickOptions::default(),
    }
}

#[test]
fn a_pick_answers_for_one_lane_and_skips_a_task_with_an_active_run() {
    let mut busy = crate::dispatcher_tests::leaf("a");
    busy.active_run = Some("01890a5d-ac96-774b-bcce-b302099a8057".to_string());
    let free = crate::dispatcher_tests::leaf("b");
    let lane = LaneId::new("build", 1);
    let state = LanesState::default();

    let (text, doc) = pick_answer(
        Pick::One,
        Some(&lane),
        &loaded(vec![busy.clone(), free]),
        &state,
    )
    .unwrap();
    assert!(text.starts_with("build-1: would start b with w"), "{text}");
    assert_eq!(doc["lane"], "build-1");
    assert!(doc["outcome"].is_object(), "{doc}");

    let (text, _) = pick_answer(Pick::One, Some(&lane), &loaded(vec![busy]), &state).unwrap();
    assert!(
        text.starts_with("build-1: nothing to do (a waits: "),
        "{text}"
    );
    assert!(text.contains("is active"), "{text}");

    assert!(pick_answer(Pick::One, None, &loaded(Vec::new()), &state).is_err());
    let (text, doc) = pick_answer(Pick::DryRun, None, &loaded(Vec::new()), &state).unwrap();
    assert!(text.starts_with("build-1: nothing to do"), "{text}");
    assert!(doc.get("per_lane").is_some(), "{doc}");
}
