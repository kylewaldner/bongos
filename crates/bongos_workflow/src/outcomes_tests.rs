//! Tests of run outcomes (D16): a PR result, a document as a shared artifact under a free name,
//! a proposal pending in the task builder that creates no task, a missing promised outcome failing
//! the run, and `report_outcome`. A child of the lifecycle tests; it drives runs through the
//! engine of the task link tests.

use bongos_builder_model::By;
use bongos_work_model::commands::Write;
use bongos_work_model::{ResultKind, ResultState, Visibility};
use bongos_workflow_model::FailureReason;
use gpui::TestAppContext;

use super::task_link_tests::{Linked, auto_runner, linked};
use super::*;
use crate::outcomes::{self, Delivered, RunOutcome};
use crate::task_link::MERGE_GATE;

/// One node delivering a PR, a document and a proposal, all promised.
const DELIVER: &str = r#"
schema = 1
name = "deliver"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { pr = { type = "text", outcome = "pr" }, title = { type = "text" }, doc = { type = "path", outcome = "document" }, proposal = { type = "path", outcome = "tasks" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
outcomes = ["pr", "document", "tasks"]
"#;

const PROPOSAL: &str = "format 1\nproposal 4\nfor someone-else\nby kyle\nwhy split the login work\n\n\
                        Add login | id=leaf-id\n  todo: implement\n";

fn deliver_world() -> World {
    let mut world = World::new();
    write_definition(world.profile(), "deliver", DELIVER);
    world.leaf_file().todos[1].workflow = Some("deliver".to_string());
    let leaf_dir = world.tree.projects[0].task.children[0].dir.clone();
    fs::create_dir_all(&leaf_dir).unwrap();
    world
}

fn leaf_dir(world: &World) -> PathBuf {
    world.tree.projects[0].task.children[0].dir.clone()
}

/// Starts `deliver` with the work node reporting `doc` and `proposal`, writing `files` into the
/// run worktree first.
fn run_deliver(
    world: &World,
    doc: &str,
    files: &[(&str, &str)],
    cx: &mut TestAppContext,
) -> (Linked, String) {
    let mut runner = auto_runner();
    runner.script(
        "work",
        json!({
            "pr": "https://example.com/pr/12",
            "title": "Add login",
            "doc": doc,
            "proposal": "out/next.proposal.txt",
        }),
    );
    let linked = linked(world, runner);
    let run = linked.start(StartRequest::new(LEAF), cx);
    let worktree = linked.engine.lifecycle().worktree(&run).unwrap().path.clone();
    for (path, text) in files {
        let path = worktree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    (linked, run)
}

#[gpui::test]
fn outcomes_a_document_is_a_shared_artifact_under_a_free_name(cx: &mut TestAppContext) {
    let world = deliver_world();
    let leaf_dir = leaf_dir(&world);
    fs::write(leaf_dir.join("design.md"), "Kyle's own design\n").unwrap();
    let (linked, run) = run_deliver(
        &world,
        "notes/design.md",
        &[
            ("notes/design.md", "the run's design\n"),
            ("out/next.proposal.txt", PROPOSAL),
        ],
        cx,
    );
    assert_eq!(linked.status(&run), RunStatus::Succeeded);

    // Never overwriting: the run's copy takes the next free name.
    assert_eq!(
        fs::read_to_string(leaf_dir.join("design.md")).unwrap(),
        "Kyle's own design\n"
    );
    assert_eq!(
        fs::read_to_string(leaf_dir.join("design-2.md")).unwrap(),
        "the run's design\n"
    );
    let changes = linked.writes.changes.lock().unwrap().clone();
    let manifest = changes
        .iter()
        .find_map(|change| {
            change.writes.iter().find_map(|write| match write {
                Write::Artifacts { file, .. } => Some(file.clone()),
                _ => None,
            })
        })
        .expect("the manifest is written");
    let entry = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.path == "design-2.md")
        .unwrap();
    assert_eq!(entry.visibility, Some(Visibility::Shared));
    assert_eq!(
        entry.description,
        format!("from run {}, node work", crate::worktrees::short_id(&run))
    );

    // The PR is the task's result.
    let result = linked.writes.leaf().file.unwrap().result.unwrap();
    assert_eq!(result.kind, ResultKind::Pr);
    assert_eq!(result.url.as_deref(), Some("https://example.com/pr/12"));
    assert_eq!(result.state, Some(ResultState::Open));
    assert_eq!(
        result.extra.get("title").and_then(|value| value.as_str()),
        Some("Add login")
    );
    // Delivered once, then the todo.
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Done);
}

#[gpui::test]
fn outcomes_a_proposal_is_pending_and_creates_no_task(cx: &mut TestAppContext) {
    let world = deliver_world();
    let leaf_dir = leaf_dir(&world);
    // A proposal 1 the builder already knows on disk: the run's copy is number 2.
    fs::create_dir_all(leaf_dir.join("proposals")).unwrap();
    fs::write(leaf_dir.join("proposals/1.proposal.txt"), "proposal 1\n").unwrap();
    let children_before = linked_children(&world);
    let (linked, run) = run_deliver(
        &world,
        "design.md",
        &[
            ("design.md", "a design\n"),
            ("out/next.proposal.txt", PROPOSAL),
        ],
        cx,
    );
    assert_eq!(linked.status(&run), RunStatus::Succeeded);

    let written = fs::read_to_string(leaf_dir.join("proposals/2.proposal.txt")).unwrap();
    let parsed = bongos_builder_model::parse(&written);
    assert_eq!(
        parsed.header.by.map(|by| by.value),
        Some(By::Workflow("deliver".to_string()))
    );
    assert_eq!(parsed.header.number.map(|number| number.value), Some(2));
    assert_eq!(
        parsed.header.for_task.map(|task| task.value).as_deref(),
        Some(LEAF)
    );
    assert!(written.contains("why split the login work"));
    assert!(written.contains("Add login | id=leaf-id"));

    // Pending: no `[[proposals]]` record settles it, and nothing was created from it.
    let leaf = linked.writes.leaf();
    assert!(leaf.file.as_ref().unwrap().proposals.is_empty());
    assert_eq!(leaf.children.len(), 0);
    assert_eq!(
        linked.writes.tree.lock().unwrap().projects[0].task.children.len(),
        children_before
    );
    assert!(!leaf_dir.join("tasks").exists(), "no task directory");
}

fn linked_children(world: &World) -> usize {
    world.tree.projects[0].task.children.len()
}

#[gpui::test]
fn outcomes_a_missing_promised_outcome_fails_the_run(cx: &mut TestAppContext) {
    let world = deliver_world();
    let (linked, run) = run_deliver(
        &world,
        "notes/missing.md",
        &[("out/next.proposal.txt", PROPOSAL)],
        cx,
    );
    assert_eq!(linked.status(&run), RunStatus::Failed);
    let state = linked.engine.lifecycle().state(&run).unwrap();
    let failure = state.ended.unwrap().1.unwrap();
    assert_eq!(failure.reason, FailureReason::OutcomePromisedNotDelivered);
    assert_eq!(failure.node.map(|key| key.node).as_deref(), Some("work"));
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Open);
    assert_eq!(
        linked.writes.history().last().unwrap().0,
        "run failed: outcome promised but not delivered at work"
    );
}

#[gpui::test]
async fn outcomes_report_outcome_records_a_document(cx: &mut TestAppContext) {
    let world = deliver_world();
    let linked = linked(&world, FakeRunner::new());
    let run = linked.start(StartRequest::new(LEAF), cx);
    let worktree = linked.engine.lifecycle().worktree(&run).unwrap().path.clone();
    fs::write(worktree.join("report.md"), "reported\n").unwrap();
    let key = NodeKey::new("work", 1);
    let task = cx.update(|cx| {
        linked.engine.report_outcome(
            &run,
            &key,
            RunOutcome::Document {
                path: "report.md".to_string(),
            },
            cx,
        )
    });
    let delivered = task.await.unwrap();
    assert_eq!(
        delivered,
        Delivered::Document {
            name: "report.md".to_string()
        }
    );
    assert_eq!(
        fs::read_to_string(leaf_dir(&world).join("report.md")).unwrap(),
        "reported\n"
    );
    let escaping = cx.update(|cx| {
        linked.engine.report_outcome(
            &run,
            &key,
            RunOutcome::Document {
                path: "../outside.md".to_string(),
            },
            cx,
        )
    });
    assert!(escaping.await.is_err());
}

#[test]
fn outcomes_proposal_text_sets_the_header() {
    let text = outcomes::proposal_text("Add login\n  todo: implement\n", 3, LEAF, "plan");
    assert_eq!(
        text,
        "proposal 3\nfor leaf-id\nby workflow plan\nAdd login\n  todo: implement\n"
    );
    let text = outcomes::proposal_text(PROPOSAL, 7, LEAF, "plan");
    assert!(text.starts_with("format 1\nproposal 7\nfor leaf-id\nby workflow plan\nwhy split"));
}

#[test]
fn outcomes_source_paths_stay_in_the_worktree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("wt");
    fs::create_dir_all(&root).unwrap();
    fs::write(temp.path().join("outside.md"), "x").unwrap();
    fs::write(root.join("inside.md"), "x").unwrap();
    std::os::unix::fs::symlink(temp.path().join("outside.md"), root.join("link.md")).unwrap();
    assert!(outcomes::source_file(&root, "inside.md").is_ok());
    assert!(outcomes::source_file(&root, "../outside.md").is_err());
    assert!(outcomes::source_file(&root, "/etc/passwd").is_err());
    assert!(outcomes::source_file(&root, "link.md").is_err());
}
