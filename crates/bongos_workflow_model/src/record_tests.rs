use std::io::{self, Read, Seek, Write};

use serde_json::json;

use crate::failure::{Failure, FailureReason};
use crate::inputs::InputValue;
use crate::record::*;

fn key(node: &str) -> NodeKey {
    NodeKey::new(node, 1)
}

fn started(key: &NodeKey, try_number: u32) -> Event {
    Event::NodeStarted {
        key: key.clone(),
        try_number,
        pid: 4000 + try_number,
        pid_start_time: 123_456,
    }
}

fn finished(key: &NodeKey, try_number: u32, result: serde_json::Value, cost: f64) -> Event {
    Event::NodeFinished {
        key: key.clone(),
        try_number,
        result: Some(result),
        failure: None,
        cost_usd: cost,
        turns: Some(3),
        elapsed_ms: 1500,
        session_id: Some("session-1".to_string()),
        confinement_refusals: Vec::new(),
    }
}

fn failed(key: &NodeKey, try_number: u32, reason: FailureReason) -> Event {
    Event::NodeFinished {
        key: key.clone(),
        try_number,
        result: None,
        failure: Some(Failure::new(reason, Some(key.clone()), "exit 2")),
        cost_usd: 0.25,
        turns: None,
        elapsed_ms: 10,
        session_id: None,
        confinement_refusals: Vec::new(),
    }
}

fn every_event() -> Vec<Event> {
    let plan = key("plan");
    let branch = NodeKey::new("write", 2)
        .in_branch("plan", 0)
        .in_branch("items", 3);
    vec![
        Event::Created,
        Event::Queued,
        started(&plan, 1),
        finished(&plan, 1, json!({ "items": ["a", "b"] }), 0.5),
        failed(&branch, 2, FailureReason::AgentExitedNonzero),
        Event::GateOpened {
            key: plan.clone(),
            question: "Approve the plan?".to_string(),
        },
        Event::GateClosed {
            key: plan.clone(),
            by: "person".to_string(),
            edit: Some(json!({ "items": ["a"] })),
            comment: Some("one is enough".to_string()),
        },
        Event::BranchMerged {
            join: "collect".to_string(),
            branch: vec![("plan".to_string(), 0)],
            sha: "abc123".to_string(),
        },
        Event::RouteTaken {
            from: plan.clone(),
            to: "write".to_string(),
            pass: 2,
        },
        Event::Paused,
        Event::Halted,
        Event::Resumed,
        Event::Interrupted {
            keys: vec![plan.clone(), branch],
        },
        Event::Held {
            key: plan.clone(),
            failure: Failure::new(
                FailureReason::CheckNoVerdict,
                Some(plan.clone()),
                "tests: the runner crashed",
            ),
        },
        Event::DefinitionReloaded { rev: 2 },
        Event::Ended {
            state: EndState::Failed,
            failure: Some(Failure::new(
                FailureReason::NoRouteForResult,
                Some(plan),
                "verdict = maybe",
            )),
        },
        Event::Cancelled,
    ]
}

#[test]
fn every_event_round_trips_as_one_json_line() {
    for event in every_event() {
        let line = event.to_line();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "{line}");
        let parsed: Event = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed, event);
    }
}

#[test]
fn events_are_tagged_by_name() {
    let value = serde_json::to_value(started(&key("plan"), 1)).unwrap();
    assert_eq!(
        value,
        json!({
            "event": "NodeStarted",
            "key": { "node": "plan", "pass": 1 },
            "try": 1,
            "pid": 4001,
            "pid_start_time": 123456
        })
    );
    assert_eq!(
        serde_json::to_value(Event::Created).unwrap(),
        json!({ "event": "Created" })
    );
}

#[test]
fn node_keys_have_a_stable_string_form() {
    let plain = key("triage");
    assert_eq!(plain.to_string(), "triage#1");
    assert_eq!(plain.branch_label(), "root");
    let nested = NodeKey::new("write", 2)
        .in_branch("plan", 0)
        .in_branch("items", 3);
    assert_eq!(nested.to_string(), "write[plan=0][items=3]#2");
    assert_eq!(nested.branch_label(), "plan-0.items-3");
    for key in [plain, nested] {
        assert_eq!(key.to_string().parse::<NodeKey>().unwrap(), key);
    }
    for bad in [
        "", "triage", "#1", "a[b]#1", "a[=1]#1", "a[b=x]#1", "a#x", "a[b=1",
    ] {
        assert!(bad.parse::<NodeKey>().is_err(), "{bad}");
    }
}

/// Counts every call so a test can see exactly how `append` writes.
#[derive(Default)]
struct Recorder {
    bytes: Vec<u8>,
    write_alls: usize,
    syncs: usize,
    synced_len: usize,
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.write_alls += 1;
        self.bytes.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SyncWrite for Recorder {
    fn sync_data(&mut self) -> io::Result<()> {
        self.syncs += 1;
        self.synced_len = self.bytes.len();
        Ok(())
    }
}

#[test]
fn append_is_one_line_one_write_and_one_sync() {
    let mut recorder = Recorder::default();
    let event = finished(&key("plan"), 1, json!({ "ok": true }), 0.1);
    append(&mut recorder, &event).unwrap();
    assert_eq!(recorder.write_alls, 1);
    assert_eq!(recorder.syncs, 1);
    assert_eq!(
        recorder.synced_len,
        recorder.bytes.len(),
        "synced after the write"
    );
    let text = String::from_utf8(recorder.bytes).unwrap();
    assert_eq!(text, event.to_line());
    assert_eq!(text.lines().count(), 1);
}

#[test]
fn append_to_a_real_file_reads_back() {
    let mut file = tempfile::tempfile().unwrap();
    for event in every_event() {
        append(&mut file, &event).unwrap();
    }
    file.rewind().unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    let parsed = parse_log(&text).unwrap();
    assert!(!parsed.dropped_tail);
    assert_eq!(parsed.valid_len, text.len());
    assert_eq!(parsed.events, every_event());
}

fn log(events: &[Event]) -> String {
    events.iter().map(Event::to_line).collect()
}

fn parsed(events: Vec<Event>, dropped_tail: bool, valid_len: usize) -> ParsedLog {
    ParsedLog {
        events,
        dropped_tail,
        valid_len,
    }
}

#[test]
fn a_truncated_last_line_is_dropped_and_reported() {
    let events = vec![Event::Created, Event::Queued];
    let full = log(&events);
    let tail = finished(&key("plan"), 1, json!({}), 0.0).to_line();
    let text = format!("{full}{}", &tail[..tail.len() / 2]);
    assert_eq!(
        parse_log(&text).unwrap(),
        parsed(events.clone(), true, full.len())
    );
    // A complete log drops nothing.
    assert_eq!(
        parse_log(&full).unwrap(),
        parsed(events.clone(), false, full.len())
    );
    assert_eq!(parse_log("").unwrap(), parsed(Vec::new(), false, 0));
    // A last line with complete JSON but no newline is still an unfinished write: dropped.
    let unterminated = full.trim_end();
    assert_eq!(
        parse_log(unterminated).unwrap(),
        parsed(vec![Event::Created], true, Event::Created.to_line().len())
    );
    // A crash inside a multi-byte character leaves bytes that are not UTF-8; still just a tail.
    let mut bytes = full.clone().into_bytes();
    bytes.extend_from_slice(&"{\"event\":\"é".as_bytes()[..11]);
    assert_eq!(parse_log(&bytes).unwrap(), parsed(events, true, full.len()));
}

#[test]
fn a_bad_middle_line_is_an_error() {
    let text = format!(
        "{}{{\"event\": \"Nope\"}}\n{}",
        Event::Created.to_line(),
        Event::Queued.to_line()
    );
    let error = parse_log(&text).unwrap_err();
    assert_eq!(error.line, 2);
}

#[test]
fn a_bad_terminated_last_line_is_an_error() {
    let text = format!("{}{{\"event\": \"Nope\"}}\n", Event::Created.to_line());
    let error = parse_log(&text).unwrap_err();
    assert_eq!(error.line, 2);
    // Half a line followed by a newline is damage too, not a crash tail.
    let tail = Event::Queued.to_line();
    let text = format!("{}{}\n", Event::Created.to_line(), &tail[..tail.len() / 2]);
    assert_eq!(parse_log(&text).unwrap_err().line, 2);
}

#[test]
fn repair_after_a_crash_mid_line_lets_appends_continue() {
    let mut file = tempfile::tempfile().unwrap();
    append(&mut file, &Event::Created).unwrap();
    append(&mut file, &Event::Queued).unwrap();
    let tail = finished(&key("plan"), 1, json!({}), 0.0).to_line();
    file.write_all(&tail.as_bytes()[..tail.len() / 2]).unwrap();

    let repaired = repair(&mut file).unwrap();
    assert_eq!(repaired.events, vec![Event::Created, Event::Queued]);
    assert!(repaired.dropped_tail);
    assert_eq!(file.metadata().unwrap().len(), repaired.valid_len as u64);

    let plan = key("plan");
    append(&mut file, &started(&plan, 1)).unwrap();
    file.rewind().unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(
        parse_log(&bytes).unwrap(),
        parsed(
            vec![Event::Created, Event::Queued, started(&plan, 1)],
            false,
            bytes.len()
        )
    );
    // A second repair of a clean log changes nothing.
    let again = repair(&mut file).unwrap();
    assert!(!again.dropped_tail);
    assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
}

#[test]
fn repair_leaves_a_damaged_log_alone() {
    let mut file = tempfile::tempfile().unwrap();
    let text = format!("{{\"event\": \"Nope\"}}\n{}half", Event::Created.to_line());
    file.write_all(text.as_bytes()).unwrap();
    assert!(matches!(repair(&mut file), Err(RepairError::Log(_))));
    assert_eq!(file.metadata().unwrap().len(), text.len() as u64);
}

#[test]
fn replay_of_a_successful_run() {
    let triage = key("triage");
    let write = key("write");
    let state = replay(&[
        Event::Created,
        Event::Queued,
        started(&triage, 1),
        finished(&triage, 1, json!({ "mode": "planned" }), 0.5),
        Event::RouteTaken {
            from: triage.clone(),
            to: "write".to_string(),
            pass: 1,
        },
        started(&write, 1),
        finished(&write, 1, json!({ "summary": "done" }), 1.25),
        Event::Ended {
            state: EndState::Succeeded,
            failure: None,
        },
    ]);
    assert_eq!(state.lifecycle(), Lifecycle::Ended(EndState::Succeeded));
    assert!(state.running.is_empty());
    assert_eq!(state.finished.len(), 2);
    assert!(state.finished.iter().all(|f| f.had_start && f.succeeded()));
    assert_eq!(state.cost_usd, 1.75);
    assert_eq!(state.routes.len(), 1);
    assert_eq!(state.passes.get("write"), Some(&1));
    assert_eq!(state.ignored_after_end, 0);
}

#[test]
fn replay_tracks_lifecycle_before_any_node() {
    assert_eq!(replay(&[]).lifecycle(), Lifecycle::New);
    assert_eq!(replay(&[Event::Created]).lifecycle(), Lifecycle::Created);
    assert_eq!(
        replay(&[Event::Created, Event::Queued]).lifecycle(),
        Lifecycle::Queued
    );
}

#[test]
fn replay_of_a_failed_run_with_a_retry() {
    let build = key("build");
    let state = replay(&[
        Event::Created,
        started(&build, 1),
        failed(&build, 1, FailureReason::AgentExitedNonzero),
        started(&build, 2),
        failed(&build, 2, FailureReason::AgentExitedNonzero),
        Event::Ended {
            state: EndState::Failed,
            failure: Some(Failure::new(
                FailureReason::AgentExitedNonzero,
                Some(build.clone()),
                "",
            )),
        },
    ]);
    assert_eq!(state.lifecycle(), Lifecycle::Ended(EndState::Failed));
    assert_eq!(state.finished.len(), 2);
    assert_eq!(state.last_finish(&build).unwrap().try_number, 2);
    assert!(!state.last_finish(&build).unwrap().succeeded());
    assert_eq!(state.cost_usd, 0.5);
    let (_, failure) = state.ended.as_ref().unwrap();
    assert_eq!(
        failure.as_ref().unwrap().reason,
        FailureReason::AgentExitedNonzero
    );
}

#[test]
fn replay_running_node_keeps_its_try_and_pid() {
    let plan = key("plan");
    let state = replay(&[Event::Created, started(&plan, 2)]);
    assert_eq!(state.lifecycle(), Lifecycle::Running);
    assert_eq!(
        state.running.get(&plan),
        Some(&RunningNode {
            try_number: 2,
            pid: 4002,
            pid_start_time: 123_456
        })
    );
}

#[test]
fn replay_of_gates() {
    let plan = key("plan");
    let opened = replay(&[
        Event::Created,
        Event::GateOpened {
            key: plan.clone(),
            question: "Go?".to_string(),
        },
    ]);
    assert_eq!(
        opened.open_gates.get(&plan).map(String::as_str),
        Some("Go?")
    );
    let closed = replay(&[
        Event::Created,
        Event::GateOpened {
            key: plan.clone(),
            question: "Go?".to_string(),
        },
        Event::GateClosed {
            key: plan,
            by: "person".to_string(),
            edit: None,
            comment: Some("fine".to_string()),
        },
    ]);
    assert!(closed.open_gates.is_empty());
    assert_eq!(closed.closed_gates.len(), 1);
    assert_eq!(closed.closed_gates[0].comment.as_deref(), Some("fine"));
}

#[test]
fn replay_of_fan_out_branches_and_merge() {
    let plan = key("plan");
    let first = NodeKey::new("write", 1).in_branch("plan", 0);
    let second = NodeKey::new("write", 1).in_branch("plan", 1);
    let state = replay(&[
        Event::Created,
        started(&plan, 1),
        finished(&plan, 1, json!({ "items": ["a", "b"] }), 0.0),
        started(&first, 1),
        started(&second, 1),
        finished(&first, 1, json!({ "ok": 1 }), 0.1),
        Event::BranchMerged {
            join: "collect".to_string(),
            branch: first.branch.clone(),
            sha: "aaa".to_string(),
        },
    ]);
    assert_eq!(state.running.keys().collect::<Vec<_>>(), vec![&second]);
    assert!(state.last_finish(&first).unwrap().succeeded());
    assert_eq!(
        state.merged,
        vec![MergedBranch {
            join: "collect".to_string(),
            branch: vec![("plan".to_string(), 0)],
            sha: "aaa".to_string(),
        }]
    );
}

#[test]
fn replay_of_loops_counts_passes() {
    let review = key("review");
    let state = replay(&[
        Event::Created,
        started(&review, 1),
        finished(&review, 1, json!({ "verdict": "changes" }), 0.0),
        Event::RouteTaken {
            from: review.clone(),
            to: "fix".to_string(),
            pass: 1,
        },
        Event::RouteTaken {
            from: key("fix"),
            to: "review".to_string(),
            pass: 2,
        },
        started(&NodeKey::new("review", 2), 1),
    ]);
    assert_eq!(state.passes.get("review"), Some(&2));
    assert_eq!(state.passes.get("fix"), Some(&1));
    assert_eq!(state.routes.len(), 2);
}

#[test]
fn replay_of_pause_halt_and_resume() {
    let paused = replay(&[Event::Created, Event::Paused]);
    assert_eq!(paused.lifecycle(), Lifecycle::Paused);
    let halted = replay(&[Event::Created, Event::Paused, Event::Halted]);
    assert_eq!(halted.lifecycle(), Lifecycle::Halted);
    let resumed = replay(&[Event::Created, Event::Halted, Event::Resumed]);
    assert!(!resumed.paused && !resumed.halted);
    assert_eq!(resumed.lifecycle(), Lifecycle::Created);
}

#[test]
fn replay_of_held_then_resumed() {
    let check = key("check");
    let other = key("other");
    let failure = Failure::new(
        FailureReason::CheckNoVerdict,
        Some(check.clone()),
        "tests: timed out",
    );
    let held = |key: &NodeKey| Event::Held {
        key: key.clone(),
        failure: failure.clone(),
    };
    let base = vec![
        Event::Created,
        started(&check, 1),
        finished(&check, 1, json!({ "verdict": "no_verdict" }), 0.0),
        held(&check),
    ];
    let state = replay(&base);
    assert_eq!(state.held.len(), 1);
    assert_eq!(state.held[&check], failure);
    assert_eq!(
        state.lifecycle(),
        Lifecycle::Running,
        "a hold is not an end"
    );
    assert!(state.ended.is_none());

    let line = held(&check).to_line();
    assert!(line.contains("\"event\":\"Held\""), "{line}");
    assert!(line.contains("check_no_verdict"), "{line}");

    let mut resumed = base.clone();
    resumed.push(Event::Resumed);
    assert!(
        replay(&resumed).held.is_empty(),
        "a resume clears every hold"
    );

    let mut rerun = base;
    rerun.push(held(&other));
    rerun.push(started(&check, 2));
    let state = replay(&rerun);
    assert_eq!(
        state.held.keys().collect::<Vec<_>>(),
        vec![&other],
        "a new try of a held key clears only its hold"
    );
}

#[test]
fn replay_of_interrupted_then_resumed() {
    let a = key("a");
    let b = key("b");
    let state = replay(&[
        Event::Created,
        started(&a, 1),
        started(&b, 1),
        Event::Interrupted {
            keys: vec![a.clone(), b.clone()],
        },
    ]);
    assert!(state.running.is_empty());
    assert!(state.finished.is_empty(), "interrupted is not finished");
    assert_eq!(state.interrupted.len(), 2);
    assert_eq!(state.interrupted[&a].as_ref().unwrap().pid, 4001);

    let resumed = replay(&[
        Event::Created,
        started(&a, 1),
        started(&b, 1),
        Event::Interrupted {
            keys: vec![a.clone(), b.clone()],
        },
        Event::Resumed,
        started(&a, 2),
    ]);
    assert_eq!(resumed.interrupted.keys().collect::<Vec<_>>(), vec![&b]);
    assert_eq!(resumed.running[&a].try_number, 2);
}

fn ended(end: EndState, failure: Option<Failure>) -> Event {
    Event::Ended {
        state: end,
        failure,
    }
}

fn run_failure(key: &NodeKey) -> Failure {
    Failure::new(
        FailureReason::AgentExitedNonzero,
        Some(key.clone()),
        "exit 2",
    )
}

#[test]
fn replay_of_a_cancelled_run_keeps_costs_and_ignores_the_rest() {
    let plan = key("plan");
    let write = key("write");
    let state = replay(&[
        Event::Created,
        started(&plan, 1),
        Event::Cancelled,
        // The cancelled try reports its cost: applied, lifecycle unchanged.
        finished(&plan, 1, json!({}), 9.0),
        ended(EndState::Succeeded, None),
        started(&write, 1),
        Event::Resumed,
    ]);
    assert_eq!(state.lifecycle(), Lifecycle::Cancelled);
    assert_eq!(state.finished.len(), 1);
    assert!(state.finished[0].had_start);
    assert!(state.running.is_empty());
    assert_eq!(state.cost_usd, 9.0);
    assert!(state.ended.is_none());
    assert_eq!(state.ignored_after_end, 3);
}

#[test]
fn replay_of_a_succeeded_run_keeps_late_costs_and_ignores_the_rest() {
    let plan = key("plan");
    let state = replay(&[
        Event::Created,
        started(&plan, 1),
        finished(&plan, 1, json!({}), 1.0),
        ended(EndState::Succeeded, None),
        finished(&plan, 2, json!({}), 0.5),
        Event::Cancelled,
        started(&plan, 3),
    ]);
    assert_eq!(state.lifecycle(), Lifecycle::Ended(EndState::Succeeded));
    assert_eq!(state.cost_usd, 1.5);
    assert!(!state.cancelled);
    assert!(state.running.is_empty());
    assert_eq!(state.ignored_after_end, 2);
}

#[test]
fn replay_of_a_failed_run_then_retry_step_then_success() {
    let plan = key("plan");
    let events = [
        Event::Created,
        started(&plan, 1),
        failed(&plan, 1, FailureReason::AgentExitedNonzero),
        ended(EndState::Failed, Some(run_failure(&plan))),
    ];
    let failed_state = replay(&events);
    assert_eq!(failed_state.lifecycle(), Lifecycle::Ended(EndState::Failed));
    assert!(!failed_state.is_over(), "a failed run stays active");

    // Retry step reruns the failed key in the same attempt: the run is live again.
    let mut retried = events.to_vec();
    retried.push(started(&plan, 2));
    let state = replay(&retried);
    assert_eq!(state.lifecycle(), Lifecycle::Running);
    assert!(state.ended.is_none());
    assert_eq!(state.running[&plan].try_number, 2);

    retried.push(finished(&plan, 2, json!({ "ok": true }), 1.0));
    retried.push(ended(EndState::Succeeded, None));
    let state = replay(&retried);
    assert_eq!(state.lifecycle(), Lifecycle::Ended(EndState::Succeeded));
    assert!(state.last_finish(&plan).unwrap().succeeded());
    assert_eq!(state.cost_usd, 1.25);
    assert_eq!(state.ignored_after_end, 0);

    // Retry step fresh reloads the definition first, which also makes the run live.
    let mut fresh = events.to_vec();
    fresh.push(Event::DefinitionReloaded { rev: 1 });
    let state = replay(&fresh);
    assert!(state.ended.is_none());
    assert_eq!(state.definition_rev, Some(1));
}

#[test]
fn replay_of_a_failed_run_then_cancelled_closes_it_out() {
    let plan = key("plan");
    let state = replay(&[
        Event::Created,
        started(&plan, 1),
        failed(&plan, 1, FailureReason::AgentExitedNonzero),
        ended(EndState::Failed, Some(run_failure(&plan))),
        Event::Cancelled,
        started(&plan, 2),
    ]);
    assert_eq!(state.lifecycle(), Lifecycle::Cancelled);
    assert!(state.is_over());
    assert!(state.running.is_empty());
    assert_eq!(state.ignored_after_end, 1);
}

#[test]
fn replay_records_the_definition_revision() {
    assert_eq!(replay(&[Event::Created]).definition_rev, None);
    let state = replay(&[
        Event::Created,
        Event::DefinitionReloaded { rev: 1 },
        Event::DefinitionReloaded { rev: 3 },
    ]);
    assert_eq!(state.definition_rev, Some(3));
}

#[test]
fn replay_never_invents_state() {
    let plan = key("plan");
    // A finish with no start is still recorded as finished, marked as such.
    let state = replay(&[Event::Created, finished(&plan, 1, json!({}), 0.5)]);
    assert_eq!(state.finished.len(), 1);
    assert!(!state.finished[0].had_start);
    assert_eq!(state.cost_usd, 0.5);
    // A finish of another try leaves the running try alone.
    let state = replay(&[
        Event::Created,
        started(&plan, 2),
        finished(&plan, 1, json!({}), 0.0),
    ]);
    assert_eq!(state.running[&plan].try_number, 2);
    assert!(!state.finished[0].had_start);
    // An interrupted key that was not running is recorded with no try.
    let state = replay(&[
        Event::Created,
        Event::Interrupted {
            keys: vec![plan.clone()],
        },
    ]);
    assert_eq!(state.interrupted.get(&plan), Some(&None));
}

const RUN_ID: &str = "01928c4e-7b3a-7c2d-9e4f-0123456789ab";

fn run_toml() -> RunToml {
    RunToml {
        schema: RUN_SCHEMA,
        id: RUN_ID.to_string(),
        workflow: "pr-ship".to_string(),
        layer: "shipped".to_string(),
        subject: RunSubject::Task {
            id: "task-1".to_string(),
        },
        task_id: "task-1".to_string(),
        created: "2026-11-11T10:00:00Z".to_string(),
        started_by: StartedBy::TreeRun,
        parent: None,
        lane: None,
        unattended: None,
        tree: None,
        definitions: None,
        env: Vec::new(),
        lane_dirs: Default::default(),
        todo: Some(TodoRef {
            index: 0,
            name: "implement".to_string(),
        }),
        inputs: vec![
            RecordedInput {
                name: "goal".to_string(),
                source: "task.goal".to_string(),
                value: Some(InputValue::Text("ship it".to_string())),
            },
            RecordedInput {
                name: "artifacts".to_string(),
                source: "task.artifacts".to_string(),
                value: Some(InputValue::List(vec!["/t/a.md".to_string()])),
            },
            RecordedInput {
                name: "ship".to_string(),
                source: "task.merge_is_ship".to_string(),
                value: Some(InputValue::Bool(true)),
            },
            RecordedInput {
                name: "pr".to_string(),
                source: "task.pr".to_string(),
                value: None,
            },
        ],
        attempts: vec![
            Attempt {
                n: 1,
                created: "2026-11-11T10:00:00Z".to_string(),
                definition_hash: "h1".to_string(),
                definition_commit: Some(COMMIT.to_string()),
                worktree: "/w/1".to_string(),
                branch: "run/1".to_string(),
                git_dir: Some("/repo/.git/worktrees/1".to_string()),
                loop_bound: Some(7),
            },
            Attempt {
                n: 2,
                created: "2026-11-11T11:00:00Z".to_string(),
                definition_hash: "h2".to_string(),
                definition_commit: None,
                worktree: "/w/2".to_string(),
                branch: "run/2".to_string(),
                git_dir: None,
                loop_bound: None,
            },
        ],
    }
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn run_toml_round_trips() {
    let run = run_toml();
    let text = run.to_toml();
    assert!(text.contains("started_by = \"tree-run\""), "{text}");
    assert_eq!(RunToml::parse(&text).unwrap(), run);
    assert_eq!(run.current_attempt().unwrap().n, 2);
}

#[test]
fn an_old_run_toml_started_by_kyle_loads_as_a_person() {
    let mut run = run_toml();
    run.started_by = StartedBy::Person;
    let old = run
        .to_toml()
        .replace("started_by = \"person\"", "started_by = \"kyle\"");
    assert!(old.contains("started_by = \"kyle\""), "{old}");
    assert_eq!(RunToml::parse(&old).unwrap(), run);
}

#[test]
fn a_new_run_toml_writes_started_by_person() {
    let mut run = run_toml();
    run.started_by = StartedBy::Person;
    let text = run.to_toml();
    assert!(text.contains("started_by = \"person\""), "{text}");
    assert!(!text.contains("kyle"), "{text}");
}

#[test]
fn run_toml_refuses_a_bad_id_or_schema() {
    let mut run = run_toml();
    run.id = "not-a-uuid".to_string();
    assert!(matches!(
        RunToml::parse(&run.to_toml()),
        Err(RunTomlError::Id(_))
    ));
    let mut run = run_toml();
    run.schema = 2;
    assert!(matches!(
        RunToml::parse(&run.to_toml()),
        Err(RunTomlError::Schema(2))
    ));
}

#[test]
fn uuid_v7_shape() {
    assert!(is_uuid_v7(RUN_ID));
    assert!(is_uuid_v7("01928C4E-7B3A-7C2D-BE4F-0123456789AB"));
    // Version 4, wrong variant, missing hyphen, wrong length.
    assert!(!is_uuid_v7("01928c4e-7b3a-4c2d-9e4f-0123456789ab"));
    assert!(!is_uuid_v7("01928c4e-7b3a-7c2d-1e4f-0123456789ab"));
    assert!(!is_uuid_v7("01928c4e7b3a-7c2d-9e4f-0123456789abc"));
    assert!(!is_uuid_v7("01928c4e-7b3a-7c2d-9e4f-0123456789a"));
}

#[test]
fn a_landing_run_writes_started_by_landing() {
    let mut run = run_toml();
    run.started_by = StartedBy::Landing;
    let text = run.to_toml();
    assert!(text.contains("started_by = \"landing\""), "{text}");
    assert_eq!(
        RunToml::parse(&text).unwrap().started_by,
        StartedBy::Landing
    );
}

#[test]
fn the_definition_commit_is_written_only_when_there_is_one() {
    let text = run_toml().to_toml();
    assert_eq!(text.matches("definition_commit").count(), 1, "{text}");
    assert!(
        text.contains(&format!("definition_commit = \"{COMMIT}\"")),
        "{text}"
    );
    // A record from before the field reads with none.
    let old = text.replace(&format!("definition_commit = \"{COMMIT}\"\n"), "");
    let parsed = RunToml::parse(&old).unwrap();
    assert!(
        parsed
            .attempts
            .iter()
            .all(|a| a.definition_commit.is_none())
    );
}

#[test]
fn the_git_directory_is_written_only_when_there_is_one() {
    let text = run_toml().to_toml();
    assert_eq!(text.matches("git_dir").count(), 1, "{text}");
    // A record from before the field reads with none, and writes none back.
    let old = text.replace("git_dir = \"/repo/.git/worktrees/1\"\n", "");
    let parsed = RunToml::parse(&old).unwrap();
    assert!(parsed.attempts.iter().all(|a| a.git_dir.is_none()));
    assert!(!parsed.to_toml().contains("git_dir"));
}

#[test]
fn run_toml_refuses_a_definition_commit_that_is_not_a_commit_id() {
    for bad in [
        "--output=/etc/passwd",
        "main",
        "0123",
        "ABCDEF0123456789ABCDEF0123456789ABCDEF01",
        "",
    ] {
        let mut run = run_toml();
        run.attempts[1].definition_commit = Some(bad.to_string());
        assert!(
            matches!(RunToml::parse(&run.to_toml()), Err(RunTomlError::Commit(2))),
            "{bad}"
        );
    }
    assert!(is_commit_id(COMMIT));
    assert!(is_commit_id(&"a".repeat(64)));
}

#[test]
fn the_loop_bound_is_written_only_when_there_is_one_and_old_records_load() {
    let text = run_toml().to_toml();
    assert_eq!(text.matches("loop_bound").count(), 1, "{text}");
    assert!(text.contains("loop_bound = 7"), "{text}");
    let old = text.replace("loop_bound = 7\n", "");
    let parsed = RunToml::parse(&old).unwrap();
    assert!(parsed.attempts.iter().all(|a| a.loop_bound.is_none()));
    assert_eq!(
        RunToml::parse(&text).unwrap().attempts[0].loop_bound,
        Some(7)
    );
}

#[test]
fn a_project_subject_run_toml_round_trips_with_no_todo_and_an_empty_task_id() {
    let mut run = run_toml();
    run.subject = RunSubject::Project {
        id: "project-1".to_string(),
    };
    run.task_id = String::new();
    run.todo = None;
    run.started_by = StartedBy::Lane;
    run.parent = Some(RUN_ID.to_string());
    run.lane = Some(LaneRef {
        id: "lane-1".to_string(),
        kind: "build".to_string(),
    });
    run.unattended = Some(UnattendedChoice {
        value: true,
        source: UnattendedSource::Lane,
    });
    run.tree = Some(RunTreeRecord::Repository {
        repo: "/repos/app".into(),
        base: "main".to_string(),
        merge_into: Some("integration".to_string()),
    });
    run.definitions = Some("/projects/app/repository/definitions".into());
    run.env = vec![
        crate::EnvEdit::Set {
            name: "CARGO_TARGET_DIR".to_string(),
            value: "/lanes/1/target".to_string(),
        },
        crate::EnvEdit::Unset {
            name: "RUSTC_WRAPPER".to_string(),
        },
    ];
    run.lane_dirs = [("target".to_string(), "/lanes/1/target".into())].into();
    let text = run.to_toml();
    assert!(!text.contains("[todo]"), "{text}");
    assert_eq!(RunToml::parse(&text).unwrap(), run);
    run.tree = Some(RunTreeRecord::Snapshot {
        path: "/snapshots/abc".into(),
    });
    let text = run.to_toml();
    assert!(text.contains("task_id = \"\""), "{text}");
    assert!(text.contains("kind = \"project\""), "{text}");
    assert_eq!(RunToml::parse(&text).unwrap(), run);
    assert_eq!(run.subject.task_id(), None);
    run.tree = Some(RunTreeRecord::Work);
    let text = run.to_toml();
    assert!(text.contains("kind = \"work\""), "{text}");
    assert_eq!(RunToml::parse(&text).unwrap(), run);
}

#[test]
fn an_old_run_toml_with_no_subject_reads_as_its_task() {
    let run = run_toml();
    let text = run.to_toml();
    let start = text.find("[subject]").unwrap();
    let end = start
        + text[start..]
            .find("\n\n")
            .map_or(text.len() - start, |at| at + 2);
    let old = format!("{}{}", &text[..start], &text[end..]);
    assert!(!old.contains("subject"), "{old}");
    let parsed = RunToml::parse(&old).unwrap();
    assert_eq!(
        parsed.subject,
        RunSubject::Task {
            id: "task-1".to_string()
        }
    );
    assert_eq!(parsed, run);
}

#[test]
fn host_and_lane_starts_round_trip() {
    for started_by in [StartedBy::Host, StartedBy::Lane] {
        let mut run = run_toml();
        run.started_by = started_by;
        assert_eq!(
            RunToml::parse(&run.to_toml()).unwrap().started_by,
            started_by
        );
    }
}
