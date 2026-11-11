use std::path::PathBuf;

use serde_json::{Value, json};

use crate::inputs::{Input, InputValue};
use crate::record::{Event, NodeKey, TodoRef, replay};
use crate::state_doc::{Paths, RunIdentity, StateDoc, build};

const TASK_DIR: &str = "/root/projects/p/tasks/leaf";
const RUN_DIR: &str = "/root/projects/p/tasks/leaf/runs/01928c4e-7b3a-7c2d-9e4f-0123456789ab";

fn finished(key: NodeKey, try_number: u32, result: Option<Value>) -> Event {
    Event::NodeFinished {
        key,
        try_number,
        failure: result.is_none().then(|| {
            crate::failure::Failure::new(
                crate::failure::FailureReason::AgentExitedNonzero,
                None,
                "",
            )
        }),
        result,
        cost_usd: 0.5,
        turns: None,
        elapsed_ms: 0,
        session_id: None,
    }
}

fn doc() -> StateDoc {
    let triage = NodeKey::new("triage", 1);
    let state = replay(&[
        Event::Created,
        finished(triage.clone(), 1, Some(json!({ "mode": "planned" }))),
        finished(
            NodeKey::new("write", 1).in_branch("plan", 0),
            1,
            Some(json!({ "n": 0 })),
        ),
        finished(NodeKey::new("write", 1).in_branch("plan", 1), 1, None),
        finished(
            NodeKey::new("write", 1).in_branch("plan", 1),
            2,
            Some(json!({ "n": 1 })),
        ),
        Event::RouteTaken {
            from: triage,
            to: "write".to_string(),
            pass: 1,
        },
    ]);
    let inputs = vec![
        Input {
            name: "goal".to_string(),
            value: Some(InputValue::Text("Ship it".to_string())),
            source: "task.goal".to_string(),
            missing: false,
        },
        Input {
            name: "pr".to_string(),
            value: None,
            source: "task.pr".to_string(),
            missing: false,
        },
    ];
    let settings: toml::Table = toml::from_str("command = \"cargo test\"\nretries = 2\n").unwrap();
    build(
        &inputs,
        &state,
        RunIdentity {
            id: "01928c4e-7b3a-7c2d-9e4f-0123456789ab".to_string(),
            workflow: "pr-ship".to_string(),
            todo: TodoRef {
                index: 0,
                name: "implement".to_string(),
            },
            task_id: "task-1".to_string(),
            task_dir: PathBuf::from(TASK_DIR),
            branch: "feature/parser".to_string(),
            run_branch: "run/feature/parser/1".to_string(),
        },
        &settings,
        Paths {
            worktree: PathBuf::from("/worktrees/run-1"),
            scratch: PathBuf::from("/scratch/node"),
        },
    )
}

#[test]
fn the_state_document_has_every_field() {
    let value: Value = serde_json::from_str(&doc().to_json()).unwrap();
    assert_eq!(
        value,
        json!({
            "inputs": { "goal": "Ship it", "pr": null },
            "results": {
                "triage": { "mode": "planned" },
                "write": [
                    { "branch": "plan-0", "pass": 1, "result": { "n": 0 } },
                    { "branch": "plan-1", "pass": 1, "result": { "n": 1 } }
                ]
            },
            "run": {
                "id": "01928c4e-7b3a-7c2d-9e4f-0123456789ab",
                "workflow": "pr-ship",
                "todo": { "index": 0, "name": "implement" },
                "task_id": "task-1",
                "task_dir": TASK_DIR,
                "branch": "feature/parser",
                "run_branch": "run/feature/parser/1",
                "passes": { "write": 1 },
                "cost_usd": 2.0
            },
            "settings": { "command": "cargo test", "retries": 2 },
            "paths": { "worktree": "/worktrees/run-1", "scratch": "/scratch/node" }
        })
    );
}

#[test]
fn the_state_document_never_names_the_run_record() {
    let text = doc().to_json();
    assert!(!text.contains(RUN_DIR));
    assert!(!text.contains("/runs/"));
    assert!(!text.contains("events.jsonl"));
}

#[test]
fn write_readonly_leaves_a_read_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let doc = doc();
    doc.write_readonly(&path).unwrap();
    // A second write replaces the read-only file.
    doc.write_readonly(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), doc.to_json());
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    assert!(permissions.readonly());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(permissions.mode() & 0o777, 0o444);
    }
}
