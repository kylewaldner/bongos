//! Tests of the data the Lanes view and the dry-run buffer read: the dry-run lines
//! with nothing written, the rows, the summary, the `wait_text` registry and the gate
//! notification.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bongos_lanes_model::{
    Author, EdgeKind, JobRecord, LaneId, LaneRecord, LaneState, LanesState, OpenTodo, OwnerRecord,
    OwnerState, PickEdge, PickGraph, PickOptions, PickTask, RunSubject, TaskId, WaitReason,
    explain, parse,
};
use gpui::TestAppContext;
use serde_json::Value;
use tempfile::TempDir;

use crate::dry_run::{DryRunInputs, dry_run_buffer, dry_run_text, idle_reasons};
use crate::lane_control::{DispatcherPhase, LaneStatus, LanesStatus};
use crate::lanes_rows::{RowFacts, lanes_view_rows};
use crate::notify::{DesktopSink, GateNotifier};
use crate::pick_extension::PickExtension;
use crate::status_doc::{StatusFacts, status_doc_with};
use crate::store::{read_state, write_state};
use crate::summary::{
    HandoverNote, KindCount, SummaryInputs, SummarySection, SummarySections, summary_text,
};
use crate::test_support::{SINCE, writer};
use crate::wait_text::WaitTextFormatters;

fn leaf(id: &str, name: &str) -> PickTask {
    let mut task = PickTask::leaf(id);
    task.name = name.to_string();
    task.open_todo = Some(OpenTodo {
        index: 0,
        name: "implement".to_string(),
        workflow: Some("implement step".to_string()),
    });
    task
}

fn running_lane(index: u32, task: &str) -> (LaneRecord, OwnerRecord) {
    let id = LaneId::new("build", index);
    let mut record = LaneRecord::new(&id, LaneState::Running);
    record.job = Some(JobRecord {
        run_id: "run-1".to_string(),
        subject: RunSubject::Task(TaskId::new(task)),
        workflow: "implement step".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
        extension: None,
    });
    let owner = OwnerRecord {
        lane: Some(id.clone()),
        state: OwnerState::Active,
        since: SINCE.to_string(),
        by: Author::Lane(id),
        reason: None,
        prepared_commit: None,
        refreshed: false,
    };
    (record, owner)
}

/// Only `build-1` is busy, on "Usage events"; "Invoice export" waits for it; "Search index" is
/// free for `build-2`; `build-3` finds nothing.
struct Fixture {
    graph: PickGraph,
    state: LanesState,
    table: bongos_lanes_model::LaneTable,
    options: PickOptions,
}

fn fixture() -> Fixture {
    let mut invoice = leaf("invoice", "Invoice export");
    invoice.edges.push(PickEdge {
        kind: EdgeKind::After,
        target: TaskId::new("usage"),
        unblock: "merged".to_string(),
        reached: false,
        progress: "step 5 of 8 not done".to_string(),
    });
    let graph = PickGraph::new(
        ["usage", "search", "invoice"].map(TaskId::new).to_vec(),
        [
            leaf("usage", "Usage events"),
            leaf("search", "Search index"),
            invoice,
        ],
    );
    let (record, owner) = running_lane(1, "usage");
    let mut state = LanesState::default();
    state.lanes.insert(record.id(), record);
    state.owners.insert(TaskId::new("usage"), owner);
    for index in [2, 3] {
        let id = LaneId::new("build", index);
        state
            .lanes
            .insert(id.clone(), LaneRecord::new(&id, LaneState::Idle));
    }
    Fixture {
        graph,
        state,
        table: parse("schema = 1\n[[kind]]\nname = \"build\"\ncount = 3\n").unwrap(),
        options: PickOptions::default(),
    }
}

fn files_under(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                found.push((entry.path(), meta.len()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn the_dry_run_prints_its_lines_and_writes_nothing() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let fixture = fixture();
    writer
        .transact(|tx| write_state(tx, &fixture.state))
        .unwrap();
    let generation = writer.generation().unwrap();
    let files = files_under(root.path());
    let state = writer.read(read_state).unwrap();

    let text = dry_run_buffer(
        &DryRunInputs {
            graph: &fixture.graph,
            state: &state,
            table: &fixture.table,
            options: &fixture.options,
        },
        &[],
        &WaitTextFormatters::default(),
    );

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "build-1 would pick: Usage events    workflow: implement step    rank 1",
            "build-2 would pick: Search index    workflow: implement step    rank 1",
            "build-3 would pick: nothing",
            "waiting:",
            "  Invoice export   waits on usage: step 5 of 8 not done",
            "ready set: 2   critical path: 2 jobs   lanes that can be busy at once: 2",
        ]
    );
    assert_eq!(writer.generation().unwrap(), generation);
    assert_eq!(files_under(root.path()), files);
    assert_eq!(writer.read(read_state).unwrap(), state);
}

struct Lines;

impl PickExtension for Lines {
    fn explain(&self, _state: &LanesState) -> Vec<String> {
        vec!["pool gpu: 1 of 1 in use".to_string()]
    }
}

#[test]
fn extension_explain_lines_come_before_the_summary_line() {
    let fixture = fixture();
    let extensions: Vec<Arc<dyn PickExtension>> = vec![Arc::new(Lines)];
    let text = dry_run_buffer(
        &DryRunInputs {
            graph: &fixture.graph,
            state: &fixture.state,
            table: &fixture.table,
            options: &fixture.options,
        },
        &extensions,
        &WaitTextFormatters::default(),
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[lines.len() - 2], "pool gpu: 1 of 1 in use");
    assert!(lines[lines.len() - 1].starts_with("ready set:"));
}

fn pool_text(reason: &WaitReason) -> Option<String> {
    match reason {
        WaitReason::Pool { name } => Some(format!("waits for a slot in {name} (2 of 2 in use)")),
        _ => None,
    }
}

fn status_with_pool_wait() -> LanesStatus {
    let mut waiting = LaneStatus {
        id: LaneId::new("build", 1),
        state: LaneState::Waiting,
        reason: Some(WaitReason::Pool {
            name: "gpu".to_string(),
        }),
        task: Some(TaskId::new("usage")),
        pause_requested: false,
        stop_requested: false,
        asked: None,
        job: None,
        jobs_done: 0,
    };
    waiting.asked = Some("approve the merge of Usage events".to_string());
    LanesStatus {
        phase: Some(DispatcherPhase::Running),
        table_commit: None,
        table_wait: None,
        lanes: vec![waiting],
    }
}

#[test]
fn a_registered_wait_text_formatter_supplies_the_pool_text_in_status_and_the_view() {
    let status = status_with_pool_wait();
    let mut formatters = WaitTextFormatters::default();
    let plain = WaitTextFormatters::default();
    assert_eq!(
        plain.text(&WaitReason::Pool { name: "gpu".into() }),
        "waits for pool gpu"
    );
    formatters.add(pool_text);

    let facts = StatusFacts {
        project: "p",
        split: None,
        goal: &|_| None,
        run_lifecycle: &|_| None,
        waits_on_landing: &|_| false,
    };
    let doc = status_doc_with(&status, &facts, &formatters);
    assert_eq!(
        doc["lanes"][0]["reason"],
        Value::String("waits for a slot in gpu (2 of 2 in use)".to_string())
    );

    let rows = lanes_view_rows(
        &status,
        &RowFacts {
            goal: &|_| Some("Usage events".to_string()),
            run_lifecycle: &|_| None,
        },
        None,
        &formatters,
    );
    assert_eq!(
        rows.rows[0].reason.as_deref(),
        Some("waits for a slot in gpu (2 of 2 in use)")
    );
}

#[gpui::test]
fn the_registry_is_global(cx: &mut TestAppContext) {
    cx.update(|cx| {
        WaitTextFormatters::register(pool_text, cx);
        let text = WaitTextFormatters::global(cx).text(&WaitReason::Pool { name: "q".into() });
        assert!(text.starts_with("waits for a slot in q"));
    });
}

#[test]
fn the_rows_have_a_header_idle_reason_and_a_needs_you_listing() {
    let fixture = fixture();
    let dry = explain(
        &fixture.graph,
        &fixture.state,
        &fixture.table,
        &fixture.options,
    );
    let formatters = WaitTextFormatters::default();
    let idle = idle_reasons(&dry, &formatters);
    assert!(
        idle[&LaneId::new("build", 3)]
            .starts_with("nothing eligible: 1 task waits for dependencies")
    );

    let mut status = status_with_pool_wait();
    status.lanes[0].state = LaneState::Running;
    status.lanes[0].reason = None;
    for index in [2, 3] {
        status.lanes.push(LaneStatus {
            id: LaneId::new("build", index),
            state: LaneState::Idle,
            reason: None,
            task: None,
            pause_requested: false,
            stop_requested: false,
            asked: None,
            job: None,
            jobs_done: 0,
        });
    }
    let rows = lanes_view_rows(
        &status,
        &RowFacts {
            goal: &|task| Some(format!("goal {task}")),
            run_lifecycle: &|_| None,
        },
        Some(&dry),
        &formatters,
    );
    assert_eq!(rows.header, "1 of 3 lanes busy, 2 tasks ready, 1 waiting");
    assert_eq!(rows.dispatcher, "running");
    assert_eq!(rows.rows[0].reason, None);
    assert_eq!(rows.rows[0].goal.as_deref(), Some("goal usage"));
    assert_eq!(
        rows.rows[2].reason,
        idle.get(&LaneId::new("build", 3)).cloned()
    );
    assert_eq!(rows.needs_you.len(), 1);
    assert_eq!(rows.needs_you[0].lane, LaneId::new("build", 1));
    assert_eq!(rows.needs_you[0].text, "approve the merge of Usage events");
    // The text helper is the same one the dry-run buffer uses.
    assert!(dry_run_text(&fixture.graph, &dry, &[], &formatters).contains("waiting:"));
}

struct PoolSection;

impl SummarySection for PoolSection {
    fn title(&self) -> String {
        "pools".to_string()
    }

    fn lines(&self, _cx: &gpui::App) -> Vec<String> {
        vec!["gpu: 2 slots".to_string()]
    }
}

#[gpui::test]
fn the_summary_lists_the_table_sections_and_the_handover_note(cx: &mut TestAppContext) {
    cx.update(|cx| {
        SummarySections::register(Arc::new(PoolSection), cx);
        let sections = SummarySections::sections(cx);
        let none = summary_text(&SummaryInputs::default(), &[], cx);
        assert!(none.starts_with("no lane table: one lane takes every eligible task"));

        let inputs = SummaryInputs {
            counts: vec![KindCount {
                kind: "build".to_string(),
                count: 3,
                layer: "project profile invoicing".to_string(),
            }],
            handover: HandoverNote {
                window_mode: true,
                runs_in_progress: 2,
            },
            ..SummaryInputs::default()
        };
        let text = summary_text(&inputs, &sections, cx);
        assert!(text.contains("  3 build, from project profile invoicing"));
        assert!(text.contains("pools:\n  gpu: 2 slots"));
        assert!(text.contains("this window becomes its client"));
        assert!(text.ends_with("2 runs finish their current step first"));
    });
}

#[derive(Default)]
struct FakeSink(Mutex<Vec<(String, String)>>);

impl DesktopSink for FakeSink {
    fn notify(&self, title: &str, body: &str) {
        self.0
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string()));
    }
}

#[test]
fn a_gate_notifies_exactly_once() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let sink = Arc::new(FakeSink::default());
    let notifier = GateNotifier::new(sink.clone());
    let notify = |run: &str, node: &str| {
        let pending = writer
            .transact(|tx| notifier.notify_gate(tx, run, node, "build-2", "needs you", SINCE))
            .unwrap();
        pending.inspect(|pending| notifier.send(pending)).is_some()
    };
    assert!(notify("run-1", "merge"));
    assert!(!notify("run-1", "merge"));
    assert!(!notify("run-1", "merge"));
    assert!(notify("run-1", "deploy"));
    assert!(notify("run-2", "merge"));
    assert_eq!(sink.0.lock().unwrap().len(), 3);
}

#[test]
fn a_gate_sends_nothing_when_its_transaction_fails() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    let sink = Arc::new(FakeSink::default());
    let notifier = GateNotifier::new(sink.clone());
    let result = writer.transact::<()>(|tx| {
        let pending = notifier.notify_gate(tx, "run-1", "merge", "build-2", "needs you", SINCE)?;
        assert!(pending.is_some());
        anyhow::bail!("the transaction fails after the gate was recorded")
    });
    assert!(result.is_err());
    assert!(sink.0.lock().unwrap().is_empty());
}

const SHIP_FLOW: &str = r#"
schema = 1
name = "ship"
start = "merge"

[nodes.merge]
kind = "program"
use = "merge"
output = { result = { type = "choice", values = ["merged", "conflict"] } }
routes = [{ to = "deploy" }]

[nodes.deploy]
kind = "program"
use = "wait"
with = { condition = "deploy" }
output = { result = { type = "choice", values = ["satisfied", "waiting"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const GATED_FLOW: &str = r#"
schema = 1
name = "careful"
start = "merge"

[nodes.merge]
kind = "program"
use = "merge"
gate = { when = "before", question = "Merge now?" }
output = { result = { type = "choice", values = ["merged", "conflict"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const AFTER_GATED_FLOW: &str = r#"
schema = 1
name = "after"
start = "merge"

[nodes.merge]
kind = "program"
use = "merge"
gate = { when = "after", question = "Merged, carry on?" }
output = { result = { type = "choice", values = ["merged", "conflict"] } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_gate_after_a_step_does_not_make_it_gated() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("after");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("workflow.toml"), AFTER_GATED_FLOW).unwrap();
    let dirs = bongos_workflow_model::LayerDirs {
        profile: Some(root.path().to_path_buf()),
        ..Default::default()
    };
    let found = crate::ungated_steps(&dirs, &["after".to_string()]);
    assert_eq!(found["after"], vec!["merge".to_string()]);
}

#[test]
fn the_summary_finds_steps_that_merge_or_deploy_without_a_gate() {
    let root = TempDir::new().unwrap();
    for (name, text) in [("ship", SHIP_FLOW), ("careful", GATED_FLOW)] {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workflow.toml"), text).unwrap();
    }
    let dirs = bongos_workflow_model::LayerDirs {
        profile: Some(root.path().to_path_buf()),
        ..Default::default()
    };
    let names = ["careful", "missing", "ship"].map(str::to_string);
    let found = crate::ungated_steps(&dirs, &names);
    assert_eq!(
        found["ship"],
        vec!["merge".to_string(), "deploy".to_string()]
    );
    assert_eq!(found["careful"], Vec::<String>::new());
    assert!(!found.contains_key("missing"));
}

#[test]
fn the_summary_takes_counts_unattended_and_bound_from_their_layers() {
    let table = crate::locations_tests::table(
        r#"schema = 1

[[kind]]
name = "design"
count = 2
unattended = false
rules = ["prepare"]

[[kind]]
name = "build"
count = 3
rules = ["prepare"]

[rules.prepare]
workflow = "ship"
"#,
    );
    let profile = crate::locations_tests::FakeProfile {
        name: Some("fast-box".to_string()),
        counts: [("build".to_string(), 5)].into(),
        ..Default::default()
    };
    let context = crate::SummaryContext {
        project_id: "project-1".to_string(),
        profile: Some(Arc::new(profile)),
        ungated: [("ship".to_string(), vec!["merge".to_string()])].into(),
        within: Some("0193".to_string()),
    };
    let inputs = crate::summary_inputs(&table, HandoverNote::default(), &context);
    let counts: Vec<_> = inputs
        .counts
        .iter()
        .map(|count| (count.kind.as_str(), count.count, count.layer.as_str()))
        .collect();
    assert_eq!(
        counts,
        [
            ("design", 2, "lanes.toml"),
            ("build", 5, "project profile fast-box")
        ]
    );
    let unattended: Vec<_> = inputs
        .unattended
        .iter()
        .map(|choice| {
            (
                choice.kind.as_str(),
                choice.unattended,
                choice.source.as_str(),
            )
        })
        .collect();
    assert_eq!(
        unattended,
        [
            ("design", false, "lanes.toml"),
            ("build", true, "the project default")
        ]
    );
    assert_eq!(inputs.workflows[0].ungated, vec!["merge".to_string()]);
    assert_eq!(inputs.bound.as_deref(), Some("within 0193"));
}

#[test]
fn the_time_in_job_reads_from_the_given_clock() {
    use crate::lanes_rows::time_in_job;
    let at = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    let now = at("2026-03-01T11:05:09Z");
    assert_eq!(time_in_job(SINCE, now).as_deref(), Some("1h 05m"));
    assert_eq!(
        time_in_job(SINCE, at("2026-03-01T10:00:45Z")).as_deref(),
        Some("45s")
    );
    assert_eq!(
        time_in_job(SINCE, at("2026-03-01T10:12:59Z")).as_deref(),
        Some("12m")
    );
    // A start in the future, and one that does not parse.
    assert_eq!(
        time_in_job(SINCE, at("2026-03-01T09:00:00Z")).as_deref(),
        Some("0s")
    );
    assert_eq!(time_in_job("yesterday", now), None);
}
