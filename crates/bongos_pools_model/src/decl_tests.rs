use std::time::Duration;

use crate::decl::{
    Action, AllDone, ExhaustedAction, FieldType, GuardCheck, On, PoolKind, PoolScope, RunResult,
    lane_fields, parse_pools, parse_queues, parse_triggers,
};
use crate::schedule::{Schedule, TimeOfDay};

const POOLS: &str = r#"schema = 1
[[pool]]
name = "jobs"
kind = "rw"
scope = "project"
hold_while_waiting = false
max_hold = "4h"
releasable = false

[[pool]]
name = "compile"
kind = "slots"
size = 1
guard = { free_memory_gb = 4 }
settle_seconds = 5

[[pool]]
name = "disk"
kind = "slots"
size = 2
guard = { free_disk_gb = 20, path = "{project_folder}" }

[[pool]]
name = "swap"
kind = "slots"
size = 1
guard = { script = "pools/scripts/enough-swap" }

[[pool]]
name = "free-memory"
kind = "guard"
guard = { free_memory_gb = 8 }
"#;

#[test]
fn pool_examples_parse() {
    let parsed = parse_pools(POOLS).unwrap();
    assert_eq!(parsed.diagnostics, vec![]);
    let names: Vec<_> = parsed.decls.iter().map(|pool| pool.name.as_str()).collect();
    assert_eq!(names, ["jobs", "compile", "disk", "swap", "free-memory"]);
    let jobs = &parsed.decls[0];
    assert_eq!(jobs.kind, PoolKind::Rw);
    assert_eq!(jobs.scope, PoolScope::Project);
    assert_eq!(jobs.max_hold.as_deref(), Some("4h"));
    assert_eq!(jobs.line, 2);
    assert_eq!(jobs.key_line("max_hold"), 7);
    let compile = &parsed.decls[1];
    assert_eq!(compile.size, Some(1));
    assert_eq!(compile.settle_seconds, Some(5));
    assert_eq!(
        compile.guard.as_ref().map(|guard| &guard.check),
        Some(&GuardCheck::FreeMemoryGb(4.0))
    );
    assert_eq!(
        parsed.decls[2].guard.as_ref().map(|guard| &guard.check),
        Some(&GuardCheck::FreeDiskGb {
            gb: 20.0,
            path: Some("{project_folder}".to_string())
        })
    );
    assert_eq!(
        parsed.decls[3].guard.as_ref().map(|guard| &guard.check),
        Some(&GuardCheck::Script("pools/scripts/enough-swap".to_string()))
    );
    assert_eq!(parsed.decls[4].kind, PoolKind::Guard);
}

#[test]
fn blank_text_declares_nothing() {
    for text in ["", "  \n\n"] {
        assert!(parse_pools(text).unwrap().decls.is_empty());
        assert!(parse_queues(text).unwrap().decls.is_empty());
        assert!(parse_triggers(text).unwrap().decls.is_empty());
    }
}

#[test]
fn unknown_keys_are_errors_with_lines() {
    let parsed =
        parse_pools("schema = 1\nextra = 1\n[[pool]]\nname = \"a\"\nkind = \"rw\"\nsise = 2\n")
            .unwrap();
    let lines: Vec<_> = parsed.diagnostics.iter().map(|d| d.line).collect();
    assert_eq!(lines, vec![2, 6]);
    assert!(parsed.diagnostics[1].message.contains("sise"));
    let parsed =
        parse_pools("schema = 1\n[[pool]]\nname = \"a\"\nkind = \"guard\"\nguard = { free_memory_gb = 1, bogus = 2 }\n")
            .unwrap();
    assert_eq!(parsed.diagnostics[0].line, 5);
}

#[test]
fn syntax_and_schema_errors_name_a_line() {
    let error = parse_pools("schema = 1\n[[pool]\n").unwrap_err();
    assert_eq!(error.line, 2);
    let error = parse_queues("schema = 2\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert!(error.message.contains("unsupported schema"));
}

#[test]
fn queue_example_parses() {
    let text = r#"schema = 1
[[queue]]
name = "fixes"
item = { test = { type = "text" }, task = { type = "task", optional = true }, note = { type = "text", optional = true } }
task_field = "task"
max_attempts = 3
on_exhausted = "notify"
unattended = true
"#;
    let parsed = parse_queues(text).unwrap();
    assert_eq!(parsed.diagnostics, vec![]);
    let queue = &parsed.decls[0];
    assert_eq!(queue.name, "fixes");
    assert_eq!(queue.item.len(), 3);
    let task = queue
        .item
        .iter()
        .find(|field| field.name == "task")
        .unwrap();
    assert_eq!(task.ty, FieldType::Task);
    assert!(task.optional);
    assert_eq!(queue.task_field.as_deref(), Some("task"));
    assert_eq!(queue.max_attempts, Some(3));
    assert_eq!(queue.on_exhausted, ExhaustedAction::Notify);
    assert_eq!(queue.unattended, Some(true));
    assert_eq!(queue.key_line("task_field"), 5);
}

const TRIGGERS: &str = r#"schema = 1
[[trigger]]
name = "nightly-check"
title = "nightly check"
on = { timer = { at = ["03:00"] } }
catch_up = false
action = { workflow = "release check", task = "0193abc", inputs = { mode = "literal:full" } }
unattended = true

[[trigger]]
name = "release-build"
on = { all_done = { work_kind = "feature" } }
action = { workflow = "build release", repo = "app", base = "main", merge_into = "release" }

[[trigger]]
name = "triage"
on = { queue = "reports", consume = true }
action = { workflow = "triage report", inputs = { report = "item.text" } }

[[trigger]]
name = "requeue-to-fixes"
on = { run_ended = { workflow = "release check", result = "failed" } }
action = { push = { queue = "fixes", fields = { test = "literal:see the release check run" } } }

[[trigger]]
name = "start-lanes"
on = { task_done = "0193def" }
action = { dispatcher = {} }

[[trigger]]
name = "weekly"
on = { timer = { every = "1d" } }
action = { dispatcher = {} }

[[trigger]]
name = "under"
on = { all_done = { under = "0193abc" } }
action = { dispatcher = {} }
"#;

#[test]
fn trigger_examples_parse() {
    let parsed = parse_triggers(TRIGGERS).unwrap();
    assert_eq!(parsed.diagnostics, vec![]);
    let decls = &parsed.decls;
    assert_eq!(decls.len(), 7);
    assert_eq!(decls[0].title.as_deref(), Some("nightly check"));
    assert_eq!(
        decls[0].on,
        On::Timer(Schedule::At {
            times: vec![TimeOfDay { hour: 3, minute: 0 }],
            days: None
        })
    );
    assert_eq!(decls[0].unattended, Some(true));
    match &decls[0].action {
        Action::Workflow {
            workflow,
            task,
            inputs,
            ..
        } => {
            assert_eq!(workflow, "release check");
            assert_eq!(task.as_deref(), Some("0193abc"));
            assert_eq!(inputs["mode"], "literal:full");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        decls[1].on,
        On::AllDone(AllDone::WorkKind("feature".to_string()))
    );
    assert!(matches!(
        &decls[1].action,
        Action::Workflow { repo: Some(repo), base: Some(base), merge_into: Some(into), .. }
            if repo == "app" && base == "main" && into == "release"
    ));
    assert_eq!(
        decls[2].on,
        On::Queue {
            queue: "reports".to_string(),
            consume: true
        }
    );
    assert_eq!(
        decls[3].on,
        On::RunEnded {
            workflow: Some("release check".to_string()),
            result: RunResult::Failed
        }
    );
    assert!(matches!(&decls[3].action, Action::Push { queue, fields }
        if queue == "fixes" && fields["test"] == "literal:see the release check run"));
    assert_eq!(decls[4].on, On::TaskDone("0193def".to_string()));
    assert_eq!(decls[4].action, Action::Dispatcher);
    assert_eq!(
        decls[5].on,
        On::Timer(Schedule::Every(Duration::from_secs(86_400)))
    );
    assert_eq!(
        decls[6].on,
        On::AllDone(AllDone::Under("0193abc".to_string()))
    );
}

#[test]
fn on_and_action_must_be_exactly_one_variant() {
    let text = "schema = 1\n[[trigger]]\nname = \"a\"\non = { task_done = \"x\", timer = { every = \"5m\" } }\naction = { dispatcher = {} }\n";
    let parsed = parse_triggers(text).unwrap();
    assert!(parsed.decls.is_empty());
    assert_eq!(parsed.diagnostics[0].line, 4);
    let text = "schema = 1\n[[trigger]]\nname = \"a\"\non = { task_done = \"x\" }\naction = { dispatcher = {}, push = { queue = \"q\" } }\n";
    let parsed = parse_triggers(text).unwrap();
    assert_eq!(parsed.diagnostics[0].line, 5);
}

const LANES: &str = r#"schema = 1
[[kind]]
name = "build"
pulls = [{ queue = "fixes", priority = "before_picker", workflow = "fix release", inputs = { failing = "item.test" }, unattended = true, repo = "app", base = "main", merge_into = "queue/fixes" }]
[kind.binding]
pools = ["jobs", { pool = "compile", count = 2 }]
[kind.binding.hooks]
before_job   = { run = "lanes/hooks/preflight", needs = ["disk-io"], timeout = "10m" }
during_job   = { run = "lanes/hooks/janitor", every = "15m" }

[[kind]]
name = "docs"
[[kind.pulls]]
queue = "reports"
workflow = "triage report"
[[kind.pulls]]
queue = "fixes"
"#;

#[test]
fn lane_fields_are_raw_located_values() {
    let kinds = lane_fields(LANES).unwrap();
    assert_eq!(kinds.len(), 2);
    let build = &kinds["build"];
    assert_eq!(build.pulls.len(), 1);
    assert_eq!(build.pulls[0].line, 4);
    assert_eq!(
        build.pulls[0].value["queue"],
        toml::Value::String("fixes".to_string())
    );
    assert_eq!(build.pools.len(), 2);
    assert_eq!(build.pools[0].line, 6);
    assert_eq!(
        build.pools[0].value,
        toml::Value::String("jobs".to_string())
    );
    let hooks: Vec<_> = build
        .hooks
        .iter()
        .map(|(name, hook)| (name.as_str(), hook.line))
        .collect();
    assert_eq!(hooks, [("before_job", 8), ("during_job", 9)]);
    let docs = &kinds["docs"];
    assert_eq!(docs.pulls.len(), 2);
    assert_eq!(docs.pulls[1].value["queue"].as_str(), Some("fixes"));
    assert!(docs.hooks.is_empty());
}
