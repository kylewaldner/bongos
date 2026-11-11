use serde_json::{Value, json};

use crate::definition::{Definition, parse};
use crate::failure::{Failure, FailureClass, FailureReason};
use crate::fake::{FakeRunner, Scripted};
use crate::record::{EndState, Event, NodeKey, RunState, replay};
use crate::scheduler::{Command, Limits, check_fresh_retry, plan, resume_set, retry_set, step};

fn definition(text: &str) -> Definition {
    parse(text).unwrap()
}

fn ok(value: Value) -> Scripted {
    Scripted::Result(value)
}

fn key(node: &str, pass: u32) -> NodeKey {
    NodeKey::new(node, pass)
}

fn item(node: &str, fan: &str, index: u32) -> NodeKey {
    NodeKey::new(node, 1).in_branch(fan, index)
}

fn runner(text: &str) -> FakeRunner {
    FakeRunner::new(definition(text), Limits::default())
}

fn assert_succeeded(state: &RunState) {
    assert_eq!(state.ended, Some((EndState::Succeeded, None)), "{state:#?}");
}

fn failure_of(state: &RunState) -> Failure {
    match &state.ended {
        Some((EndState::Failed, Some(failure))) => failure.clone(),
        other => panic!("expected a failed end with a failure, got {other:?}"),
    }
}

/// The log position of the first event matching `predicate`.
fn position(events: &[Event], predicate: impl Fn(&Event) -> bool) -> usize {
    events.iter().position(predicate).expect("event in the log")
}

const ROUTED: &str = r#"
schema = 1
name = "routed"
start = "triage"

[nodes.triage]
kind = "agent"
output = { mode = { type = "choice", values = ["one-shot", "planned", "unsure"] } }
routes = [
  { when = { mode = "one-shot" }, to = "implement" },
  { when = { mode = "planned" }, to = "plan" },
]

[nodes.implement]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[nodes.plan]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn only_the_routed_path_runs() {
    let mut run = runner(ROUTED);
    run.script("triage", [ok(json!({ "mode": "one-shot" }))])
        .script("implement", [ok(json!({ "summary": "done" }))]);
    let state = run.run();
    assert_succeeded(&state);
    assert_eq!(run.started, vec![key("triage", 1), key("implement", 1)]);
    let plan = plan(&run.definition, &state, &run.limits);
    assert_eq!(plan.skipped(&run.definition), vec!["plan"]);
}

#[test]
fn a_result_with_no_route_fails_the_run() {
    let mut run = runner(ROUTED);
    run.script("triage", [ok(json!({ "mode": "unsure" }))]);
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::NoRouteForResult);
    assert_eq!(failure.node, Some(key("triage", 1)));
    assert_eq!(failure.detail, "no route for mode = 'unsure'");
    assert_eq!(failure.class, FailureClass::Definition);
    assert_eq!(run.started, vec![key("triage", 1)], "never retried");
}

#[test]
fn a_default_route_takes_what_no_when_matches() {
    let text = ROUTED.replace(
        r#"{ when = { mode = "planned" }, to = "plan" },"#,
        r#"{ when = { mode = "planned" }, to = "plan" }, { default = true, to = "plan" },"#,
    );
    let mut run = runner(&text);
    run.script("triage", [ok(json!({ "mode": "unsure" }))])
        .script("plan", [ok(json!({ "summary": "planned" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.started, vec![key("triage", 1), key("plan", 1)]);
}

const FIXED: &str = r#"
schema = 1
name = "fixed"
start = "split"

[nodes.split]
kind = "agent"
output = { ready = { type = "text" } }
routes = [{ to = ["left", "right"] }]

[nodes.left]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.right]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "program"
join = { wait = "all" }
output = { status = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_fixed_fan_out_runs_each_listed_node_in_its_branch() {
    let mut run = runner(FIXED);
    run.script("split", [ok(json!({ "ready": "yes" }))])
        .script("left", [ok(json!({ "summary": "l" }))])
        .script("right", [ok(json!({ "summary": "r" }))])
        .script("gather", [ok(json!({ "status": "joined" }))]);
    assert_succeeded(&run.run());
    assert_eq!(
        run.started,
        vec![
            key("split", 1),
            item("left", "split", 0),
            item("right", "split", 1),
            key("gather", 1),
        ]
    );
    let merged: Vec<_> = run
        .events
        .iter()
        .filter_map(|event| match event {
            Event::BranchMerged { join, branch, .. } => Some((join.clone(), branch.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        merged,
        vec![
            ("gather".to_string(), vec![("split".to_string(), 0)]),
            ("gather".to_string(), vec![("split".to_string(), 1)]),
        ]
    );
}

fn per_item(join: &str, width: Option<u32>) -> String {
    let width = width.map_or(String::new(), |width| format!(", width = {width}"));
    format!(
        r#"
schema = 1
name = "per item"
start = "plan"

[nodes.plan]
kind = "agent"
output = {{ items = {{ type = "list", of = "text" }} }}
routes = [{{ fan_out = {{ over = "items", to = "work"{width} }} }}]

[nodes.work]
kind = "agent"
output = {{ summary = {{ type = "text" }} }}
routes = [{{ to = "gather" }}]

[nodes.gather]
kind = "program"
join = {{ wait = "{join}" }}
output = {{ status = {{ type = "text" }} }}
routes = [{{ to = "done" }}]

[ends.done]
kind = "success"
"#
    )
}

fn items(count: usize) -> Scripted {
    let items: Vec<String> = (0..count).map(|index| format!("item {index}")).collect();
    ok(json!({ "items": items }))
}

fn fan_runner(join: &str, count: usize) -> FakeRunner {
    let mut run = runner(&per_item(join, None));
    run.script("plan", [items(count)])
        .script("work", [ok(json!({ "summary": "did it" }))])
        .script("gather", [ok(json!({ "status": "joined" }))]);
    run
}

#[test]
fn a_per_item_fan_out_runs_one_branch_per_entry() {
    let mut run = fan_runner("all", 3);
    assert_succeeded(&run.run());
    for index in 0..3 {
        assert_eq!(run.tries(&item("work", "plan", index)), 1);
    }
    assert_eq!(run.starts_of("work"), 3);
    assert_eq!(run.tries(&key("gather", 1)), 1);
}

#[test]
fn a_per_item_fan_out_over_an_empty_list_continues_at_its_join() {
    let mut run = fan_runner("all", 0);
    assert_succeeded(&run.run());
    assert_eq!(run.started, vec![key("plan", 1), key("gather", 1)]);
}

#[test]
fn nine_items_over_a_width_of_eight_fail_branch_limit_exceeded() {
    let mut run = fan_runner("all", 9);
    let failure = failure_of(&run.run());
    assert_eq!(failure.reason, FailureReason::BranchLimitExceeded);
    assert_eq!(
        failure.to_string().split(':').next(),
        Some("branch limit exceeded at plan#1")
    );
    assert_eq!(failure.class, FailureClass::Definition);
    assert_eq!(run.starts_of("work"), 0);

    // Eight is within the cap.
    let mut run = fan_runner("all", 8);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("work"), 8);

    // A route's own width is the cap for that route.
    let mut run = runner(&per_item("all", Some(2)));
    run.script("plan", [items(3)]);
    assert_eq!(
        failure_of(&run.run()).reason,
        FailureReason::BranchLimitExceeded
    );
}

#[test]
fn five_items_run_three_at_a_time() {
    let mut run = fan_runner("all", 5);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("work"), 5);
    assert_eq!(run.max_branches, 3);
    assert_eq!(run.max_running, 3);
}

#[test]
fn the_session_limit_bounds_starts_across_runs() {
    let limits = Limits {
        max_concurrent_sessions: Some(2),
        ..Limits::default()
    };
    let mut run = FakeRunner::new(definition(&per_item("all", None)), limits);
    run.script("plan", [items(5)])
        .script("work", [ok(json!({ "summary": "s" }))])
        .script("gather", [ok(json!({ "status": "j" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.max_running, 2);

    // Another run holds one of the two slots.
    let mut run = FakeRunner::new(definition(&per_item("all", None)), limits);
    run.other_sessions = 1;
    run.script("plan", [items(3)])
        .script("work", [ok(json!({ "summary": "s" }))])
        .script("gather", [ok(json!({ "status": "j" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.max_running, 1);
}

#[test]
fn run_and_session_limits_follow_the_settings() {
    let limits = Limits::default();
    assert_eq!(limits.session_limit(), 6, "null is twice the run limit");
    assert!(limits.admits_run(2));
    assert!(!limits.admits_run(3));
    assert_eq!(limits.free_sessions(4), 2);
    assert_eq!(limits.free_sessions(9), 0);
}

#[test]
fn join_all_waits_for_every_branch() {
    let mut run = fan_runner("all", 3);
    run.script_key(
        item("work", "plan", 2),
        [
            Scripted::Fail(FailureReason::AgentExitedNonzero),
            ok(json!({ "summary": "late" })),
        ],
    );
    assert_succeeded(&run.run());
    let gather = position(
        &run.events,
        |event| matches!(event, Event::NodeStarted { key, .. } if key.node == "gather"),
    );
    let last_branch = run
        .events
        .iter()
        .rposition(|event| matches!(event, Event::NodeFinished { key, .. } if key.node == "work"))
        .unwrap();
    assert!(
        gather > last_branch,
        "the join starts after the last branch"
    );
    assert_eq!(run.tries(&item("work", "plan", 2)), 2, "retried once");
}

#[test]
fn join_any_merges_the_first_and_cancels_the_rest() {
    let mut run = fan_runner("any", 5);
    let state = run.run();
    assert_succeeded(&state);
    let merged: Vec<_> = state
        .merged
        .iter()
        .map(|merged| merged.branch.clone())
        .collect();
    assert_eq!(merged, vec![vec![("plan".to_string(), 0)]]);
    let cancels: Vec<_> = run
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::CancelBranch { join, branch } => Some((join.as_str(), branch[0].1)),
            _ => None,
        })
        .collect();
    assert_eq!(cancels, vec![("gather", 1), ("gather", 2)]);
    assert_eq!(run.starts_of("work"), 3, "branches 3 and 4 never start");
    assert_eq!(run.tries(&key("gather", 1)), 1);
}

/// How many merges the scheduler asked for.
fn merges_asked(run: &FakeRunner) -> usize {
    run.commands
        .iter()
        .filter(|command| matches!(command, Command::MergeBranch { .. }))
        .count()
}

#[test]
fn a_merge_conflict_ends_the_run_failed() {
    let mut run = fan_runner("all", 3);
    run.conflicting_joins.insert("gather".to_string());
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::MergeConflict);
    assert_eq!(failure.node, Some(key("gather", 1)));
    assert!(state.merged.is_empty());
    assert_eq!(merges_asked(&run), 1, "the conflict is not merged again");
    assert_eq!(run.tries(&key("gather", 1)), 1, "only the conflicted try");
}

#[test]
fn retry_set_excludes_a_join_whose_merge_conflicted() {
    let mut run = fan_runner("all", 3);
    run.conflicting_joins.insert("gather".to_string());
    let state = run.run();
    assert_eq!(failure_of(&state).reason, FailureReason::MergeConflict);
    assert_eq!(
        retry_set(&run.definition, &state, &run.limits),
        Vec::new(),
        "a conflict is terminal for the run"
    );
}

#[test]
fn a_merge_conflict_on_an_any_join_ends_the_run_failed() {
    let mut run = fan_runner("any", 5);
    run.conflicting_joins.insert("gather".to_string());
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::MergeConflict);
    assert_eq!(failure.node, Some(key("gather", 1)));
    assert!(state.merged.is_empty());
    assert_eq!(merges_asked(&run), 1, "the conflict is not merged again");
    assert_eq!(run.tries(&key("gather", 1)), 1, "the join never runs");
    assert!(state.running.is_empty(), "running branches finish first");
    assert_eq!(retry_set(&run.definition, &state, &run.limits), Vec::new());
}

const LOOP: &str = r#"
schema = 1
name = "loop"
start = "test"

[nodes.test]
kind = "program"
output = { status = { type = "choice", values = ["pass", "fail"] } }
routes = [
  { when = { status = "pass" }, to = "done" },
  { when = { status = "fail" }, to = "fix", max_passes = 3 },
]

[nodes.fix]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "test" }]

[nodes.escalate]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_loop_stops_at_its_bound_with_each_pass_recorded() {
    let mut run = runner(LOOP);
    run.script("test", [ok(json!({ "status": "fail" }))])
        .script("fix", [ok(json!({ "summary": "tried" }))]);
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::LoopBoundReached);
    assert_eq!(failure.node, Some(key("test", 4)));
    assert!(
        failure.detail.contains("bound of 3 passes"),
        "{}",
        failure.detail
    );
    assert_eq!(
        run.started,
        vec![
            key("test", 1),
            key("fix", 1),
            key("test", 2),
            key("fix", 2),
            key("test", 3),
            key("fix", 3),
            key("test", 4),
        ]
    );
    assert_eq!(state.passes.get("fix"), Some(&3));
    assert_eq!(state.passes.get("test"), Some(&4));
}

#[test]
fn a_loop_at_its_bound_follows_on_bound() {
    let text = LOOP.replace(
        "max_passes = 3 }",
        r#"max_passes = 2, on_bound = "escalate" }"#,
    );
    let mut run = runner(&text);
    run.script("test", [ok(json!({ "status": "fail" }))])
        .script("fix", [ok(json!({ "summary": "tried" }))])
        .script("escalate", [ok(json!({ "summary": "asked" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("fix"), 2);
    assert_eq!(run.starts_of("test"), 3);
    assert_eq!(run.started.last(), Some(&key("escalate", 1)));
}

#[test]
fn a_loop_that_passes_ends_early() {
    let mut run = runner(LOOP);
    run.script(
        "test",
        [
            ok(json!({ "status": "fail" })),
            ok(json!({ "status": "pass" })),
        ],
    )
    .script("fix", [ok(json!({ "summary": "fixed" }))]);
    assert_succeeded(&run.run());
    assert_eq!(
        run.started,
        vec![key("test", 1), key("fix", 1), key("test", 2)]
    );
}

#[test]
fn a_failing_branch_lets_running_siblings_finish_and_starts_nothing() {
    let text = per_item("all", None).replace(
        "[nodes.work]\nkind = \"agent\"",
        "[nodes.work]\nkind = \"agent\"\nretries = 0",
    );
    let mut run = runner(&text);
    run.script("plan", [items(5)])
        .script("work", [ok(json!({ "summary": "fine" }))])
        .script("gather", [ok(json!({ "status": "joined" }))])
        .script_key(
            item("work", "plan", 0),
            [Scripted::Fail(FailureReason::AgentExitedNonzero)],
        );
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::AgentExitedNonzero);
    assert_eq!(failure.node, Some(item("work", "plan", 0)));
    assert_eq!(run.starts_of("work"), 3, "branches 3 and 4 never start");
    assert_eq!(run.starts_of("gather"), 0);
    for index in [1, 2] {
        let finish = state.last_finish(&item("work", "plan", index)).unwrap();
        assert!(finish.succeeded(), "sibling {index} finished");
    }
    // The run ended only once nothing ran.
    let ended = position(&run.events, |event| matches!(event, Event::Ended { .. }));
    assert!(replay(&run.events[..ended]).running.is_empty());
}

#[test]
fn retries_run_new_tries_of_the_same_key_then_fail() {
    let mut run = runner(ROUTED);
    run.script("triage", [ok(json!({ "mode": "planned" }))])
        .script("plan", [Scripted::Fail(FailureReason::TurnCapHit)]);
    let state = run.run();
    assert_eq!(
        run.tries(&key("plan", 1)),
        3,
        "the first try and two retries"
    );
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::TurnCapHit);
    assert_eq!(
        failure.class,
        FailureClass::Definition,
        "every retry hit the cap"
    );
}

const PROMISE: &str = r#"
schema = 1
name = "promise"
start = "review"

[nodes.review]
kind = "agent"
gate = { when = "after", question = "Ship this?" }
output = { verdict = { type = "text" }, pr = { type = "text", outcome = "pr", optional = true } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
outcomes = ["pr"]
"#;

#[test]
fn success_needs_every_promised_outcome() {
    let mut run = runner(PROMISE);
    run.script("review", [ok(json!({ "verdict": "fine" }))]);
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::OutcomePromisedNotDelivered);
    assert!(failure.detail.contains("promises pr"), "{}", failure.detail);
    assert_eq!(
        retry_set(&run.definition, &state, &run.limits),
        vec![key("review", 1)],
        "no branch failed, so the retry falls back to the key the end names"
    );

    let mut run = runner(PROMISE);
    run.script(
        "review",
        [ok(
            json!({ "verdict": "fine", "pr": "https://example.invalid/pr/1" }),
        )],
    );
    assert_succeeded(&run.run());
    assert!(
        run.commands.contains(&Command::OpenGate(key("review", 1))),
        "the after gate opened before routing"
    );
}

#[test]
fn a_failure_end_fails_the_run() {
    let text = ROUTED.replace(
        "[ends.done]",
        "[ends.stuck]\nkind = \"failure\"\nreason = \"needs a hand\"\n\n[ends.done]",
    );
    let text = text.replace(
        r#"{ when = { mode = "planned" }, to = "plan" },"#,
        r#"{ when = { mode = "planned" }, to = "plan" }, { when = { mode = "unsure" }, to = "stuck" },"#,
    );
    let mut run = runner(&text);
    run.script("triage", [ok(json!({ "mode": "unsure" }))]);
    let state = run.run();
    let failure = failure_of(&state);
    assert_eq!(failure.reason, FailureReason::FailureEndReached);
    assert_eq!(failure.node, Some(key("triage", 1)));
    assert_eq!(failure.detail, "stuck: needs a hand");
    assert_eq!(
        retry_set(&run.definition, &state, &run.limits),
        vec![key("triage", 1)]
    );
    let plan = plan(&run.definition, &state, &run.limits);
    assert_eq!(
        plan.failure().and_then(|failed| failed.key().cloned()),
        Some(key("triage", 1))
    );
}

#[test]
fn a_revised_gate_reruns_the_node() {
    let review = key("review", 1);
    let events = vec![
        Event::Created,
        Event::NodeStarted {
            key: review.clone(),
            try_number: 1,
            pid: 1,
            pid_start_time: 0,
        },
        Event::NodeFinished {
            key: review.clone(),
            try_number: 1,
            result: Some(json!({ "verdict": "meh" })),
            failure: None,
            cost_usd: 0.0,
            turns: None,
            elapsed_ms: 0,
            session_id: None,
        },
        Event::GateOpened {
            key: review.clone(),
            question: "Ship this?".to_string(),
        },
        Event::GateClosed {
            key: review.clone(),
            by: "kyle".to_string(),
            edit: None,
            comment: Some("add a test".to_string()),
        },
    ];
    let definition = definition(PROMISE);
    let limits = Limits::default();
    assert_eq!(
        step(&definition, &replay(&events), &limits, 6),
        vec![Command::Start(review.clone())]
    );
    // While the gate is open nothing happens and no slot is held.
    let open = replay(&events[..4]);
    assert_eq!(step(&definition, &open, &limits, 6), Vec::new());
    assert_eq!(plan(&definition, &open, &limits).open_gates, vec![review]);
}

fn finished(key: &NodeKey, result: Value) -> Event {
    Event::NodeFinished {
        key: key.clone(),
        try_number: 1,
        result: Some(result),
        failure: None,
        cost_usd: 0.0,
        turns: None,
        elapsed_ms: 0,
        session_id: None,
    }
}

fn started(key: &NodeKey) -> Event {
    Event::NodeStarted {
        key: key.clone(),
        try_number: 1,
        pid: 1,
        pid_start_time: 0,
    }
}

#[test]
fn resume_reruns_only_the_in_flight_keys() {
    let plan_key = key("plan", 1);
    let [first, second, third] = [0, 1, 2].map(|index| item("work", "plan", index));
    let events = vec![
        Event::Created,
        started(&plan_key),
        finished(&plan_key, json!({ "items": ["a", "b", "c"] })),
        started(&first),
        started(&second),
        started(&third),
        finished(&first, json!({ "summary": "done" })),
        Event::Interrupted {
            keys: vec![second.clone(), third.clone()],
        },
    ];
    let definition = definition(&per_item("all", None));
    let state = replay(&events);
    assert_eq!(
        step(&definition, &state, &Limits::default(), 6),
        Vec::new(),
        "an interrupted run waits for Kyle"
    );
    assert_eq!(resume_set(&state), vec![second.clone(), third.clone()]);

    let mut run = FakeRunner::with_events(definition, Limits::default(), events);
    run.script("work", [ok(json!({ "summary": "resumed" }))])
        .script("gather", [ok(json!({ "status": "joined" }))]);
    run.resume();
    assert_succeeded(&run.run());
    assert_eq!(run.started, vec![second, third.clone(), key("gather", 1)]);
    let retried = run.events.iter().any(
        |event| matches!(event, Event::NodeStarted { key, try_number: 2, .. } if *key == third),
    );
    assert!(retried, "a resumed key starts a new try");
}

#[test]
fn retry_step_reruns_only_the_failed_key_and_keeps_finished_siblings() {
    let text = per_item("all", None).replace(
        "[nodes.work]\nkind = \"agent\"",
        "[nodes.work]\nkind = \"agent\"\nretries = 1",
    );
    let mut run = runner(&text);
    let failing = item("work", "plan", 1);
    run.script("plan", [items(3)])
        .script("work", [ok(json!({ "summary": "fine" }))])
        .script("gather", [ok(json!({ "status": "joined" }))])
        .script_key(
            failing.clone(),
            [
                Scripted::Fail(FailureReason::AgentExitedNonzero),
                Scripted::Fail(FailureReason::AgentExitedNonzero),
                Scripted::Fail(FailureReason::AgentExitedNonzero),
                ok(json!({ "summary": "fixed" })),
            ],
        );
    let state = run.run();
    assert_eq!(failure_of(&state).node, Some(failing.clone()));
    assert_eq!(run.tries(&failing), 2, "the first try and one retry");
    assert_eq!(
        retry_set(&run.definition, &state, &run.limits),
        vec![failing.clone()]
    );

    run.retry_step(failing.clone());
    assert_succeeded(&run.run());
    // The retry step gave the key its retry again: one more failure, then success.
    assert_eq!(run.tries(&failing), 4);
    assert_eq!(run.tries(&key("plan", 1)), 1);
    assert_eq!(run.tries(&item("work", "plan", 0)), 1);
    assert_eq!(run.tries(&item("work", "plan", 2)), 1);
    assert_eq!(run.tries(&key("gather", 1)), 1);
}

#[test]
fn fresh_definition_retry_is_refused_when_a_completed_node_is_gone() {
    let old = definition(&per_item("all", None));
    let plan_key = key("plan", 1);
    let state = replay(&[
        Event::Created,
        started(&plan_key),
        finished(&plan_key, json!({ "items": ["a"] })),
    ]);
    let renamed = per_item("all", None).replace("[nodes.plan]", "[nodes.design]");
    let renamed = renamed.replace("start = \"plan\"", "start = \"design\"");
    let refusal = check_fresh_retry(&old, &definition(&renamed), &state).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "the new definition drops node 'plan', which this run completed; restart instead"
    );

    let retyped = per_item("all", None).replace(r#"of = "text""#, r#"of = "path""#);
    let refusal = check_fresh_retry(&old, &definition(&retyped), &state).unwrap_err();
    assert!(
        refusal.0.contains("changes output field 'items' of 'plan'"),
        "{refusal}"
    );

    let added = per_item("all", None).replace(
        r#"output = { items = { type = "list", of = "text" } }"#,
        r#"output = { items = { type = "list", of = "text" }, note = { type = "text" } }"#,
    );
    let refusal = check_fresh_retry(&old, &definition(&added), &state).unwrap_err();
    assert!(
        refusal.0.contains("adds required output field 'note'"),
        "{refusal}"
    );

    // A change to a node the run has not completed is fine.
    let reworded = per_item("all", None).replace(
        r#"output = { summary = { type = "text" } }"#,
        r#"output = { summary = { type = "text" }, extra = { type = "text" } }"#,
    );
    assert_ne!(reworded, per_item("all", None));
    assert_eq!(
        check_fresh_retry(&old, &definition(&reworded), &state),
        Ok(())
    );
}

#[test]
fn a_polling_node_sleeps_without_a_slot_until_it_reports_another_value() {
    let text = r#"
schema = 1
name = "poll"
start = "wait"

[nodes.wait]
kind = "program"
polls = { value = "waiting" }
output = { status = { type = "choice", values = ["waiting", "satisfied"] } }
routes = [{ when = { status = "satisfied" }, to = "done" }]

[ends.done]
kind = "success"
"#;
    let mut run = runner(text);
    run.script(
        "wait",
        [
            ok(json!({ "status": "waiting" })),
            ok(json!({ "status": "waiting" })),
            ok(json!({ "status": "satisfied" })),
        ],
    );
    assert_succeeded(&run.run());
    assert_eq!(run.tries(&key("wait", 1)), 3);

    let wait = key("wait", 1);
    let sleeping = replay(&[
        Event::Created,
        started(&wait),
        finished(&wait, json!({ "status": "waiting" })),
    ]);
    let definition = definition(text);
    assert_eq!(
        step(&definition, &sleeping, &Limits::default(), 6),
        Vec::new()
    );
    assert_eq!(
        plan(&definition, &sleeping, &Limits::default()).sleeping,
        vec![wait]
    );
}
