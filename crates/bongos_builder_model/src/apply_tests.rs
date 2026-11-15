use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use bongos_work_model::commands::{
    Context, Plan, Write, apply as write_change, default_unblock, plan,
};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::journal::{run_steps, write_journal};
use bongos_work_model::layout::{self, OP_FILE};
use bongos_work_model::store::{Task, WorkTree, load};
use bongos_work_model::{
    Author, ByKind, Kind, PersonAtKeyboard, ProposalState, Status, TaskFile, TodoRole, TodoState,
    UnblockPoint, serialize_project, serialize_task,
};
use tempfile::TempDir;

use crate::apply::{Applied, Request, STALE, applied, checked, file_hash, reject, rewritten};
use crate::diff::{Choices, Diff, Op, TaskRef, diff, fingerprint};
use crate::grammar::Node;
use crate::print::LiveSet;
use crate::test_support::*;
use crate::validate::{Checks, validate};
use crate::{Parsed, Severity, parse};

fn ctx() -> Context {
    Context::new("kyle", timestamp("2026-11-09T12:00:00Z"))
}

fn doc(root: &str, body: &str) -> String {
    format!("proposal 1\nfor {}\n\n{body}", id_of(root))
}

fn checked_parse(tree: &WorkTree, text: &str) -> Parsed {
    let parsed = parse(text);
    assert_eq!(parsed.problems, vec![], "the fixture must parse:\n{text}");
    let errors: Vec<_> = validate(&parsed, tree, &Checks::default())
        .into_iter()
        .filter(|problem| problem.severity == Severity::Error)
        .collect();
    assert_eq!(errors, vec![], "the fixture must validate:\n{text}");
    parsed
}

/// Writes the in-memory fixture `tree` (rooted at `/w`) under `root`, with `extra` files given by
/// their path relative to the root.
fn materialize(tree: &WorkTree, root: &Path, extra: &[(&str, &str)]) {
    for project in &tree.projects {
        let dir = layout::project_dir(root, &project.slug);
        fs::create_dir_all(&dir).unwrap();
        let file = project.file.as_ref().expect("a project file");
        fs::write(layout::project_file(&dir), serialize_project(file).unwrap()).unwrap();
    }
    for task in tree.tasks() {
        let dir = root.join(task.dir.strip_prefix(&tree.root).unwrap());
        fs::create_dir_all(&dir).unwrap();
        let file = task.file.as_ref().expect("a task file");
        fs::write(layout::task_file(&dir), serialize_task(file).unwrap()).unwrap();
    }
    for (path, text) in extra {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

fn assert_clean(tree: &WorkTree) {
    for task in tree.tasks() {
        assert!(task.errors.is_empty(), "{}: {:?}", task.slug, task.errors);
    }
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
}

/// Every file under `root` but journals, with its content.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else if !path.ends_with(OP_FILE) {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                files.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn journals(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.ends_with(OP_FILE) {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found
}

fn id_counts(tree: &WorkTree) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for task in tree.tasks() {
        *counts
            .entry(task.id().expect("every task parsed").to_string())
            .or_insert(0) += 1;
    }
    counts
}

fn task<'a>(tree: &'a WorkTree, id: &str) -> &'a Task {
    tree.task_by_id(id)
        .unwrap_or_else(|| panic!("no task {id}"))
}

fn file<'a>(tree: &'a WorkTree, id: &str) -> &'a TaskFile {
    task(tree, id).file.as_ref().expect("a task file")
}

fn bytes(tree: &WorkTree, id: &str) -> Vec<u8> {
    fs::read(layout::task_file(&task(tree, id).dir)).unwrap()
}

/// The id of the parent of the task `id`.
fn parent_id(tree: &WorkTree, id: &str) -> Option<String> {
    tree.tasks().into_iter().find_map(|candidate| {
        candidate
            .children
            .iter()
            .any(|child| child.id() == Some(id))
            .then(|| candidate.id().unwrap_or_default().to_string())
    })
}

/// The proposal with the ids the apply gave the tasks it created, as a re-apply of it reads.
fn with_ids(parsed: &Parsed, created: &BTreeMap<usize, String>) -> Parsed {
    fn visit(node: &mut Node, created: &BTreeMap<usize, String>) {
        if let Some(id) = created.get(&node.line) {
            node.attributes.id = Some(id.clone());
            node.attributes.keeps_work = false;
        }
        for child in &mut node.children {
            visit(child, created);
        }
    }
    let mut parsed = parsed.clone();
    visit(&mut parsed.root, created);
    parsed
}

/// A fixture on disk, loaded.
struct Fixture {
    temp: TempDir,
    tree: WorkTree,
}

impl Fixture {
    fn new(memory: &WorkTree, extra: &[(&str, &str)]) -> Self {
        let temp = TempDir::new().unwrap();
        materialize(memory, temp.path(), extra);
        let tree = load(temp.path());
        assert_clean(&tree);
        Self { temp, tree }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn reload(&self) -> WorkTree {
        let tree = load(self.root());
        assert_clean(&tree);
        tree
    }
}

/// Parses, diffs and builds the apply of `text` on the fixture, without writing it.
fn build(fixture: &Fixture, text: &str, live: &LiveSet) -> (Parsed, Diff, Applied) {
    build_by(fixture, text, live, &ctx())
}

fn build_by(
    fixture: &Fixture,
    text: &str,
    live: &LiveSet,
    ctx: &Context,
) -> (Parsed, Diff, Applied) {
    let parsed = checked_parse(&fixture.tree, text);
    let diff = diff(&parsed, &fixture.tree, live, &Choices::default());
    assert_eq!(diff.refusal, None);
    let applied = applied(
        &parsed,
        &fixture.tree,
        &diff,
        ctx,
        &file_hash(text.as_bytes()),
    )
    .expect("the apply is built");
    (parsed, diff, applied)
}

// The main fixture: a project `billing` whose top group the proposal is for.

fn billing_tree() -> WorkTree {
    let schema = planned(
        finished(leaf("schema", "schema", Kind::PrShip)),
        "proration columns",
    );
    let api = after(
        leaf("api", "api", Kind::PrShip),
        "schema",
        default_unblock(bongos_work_model::rules::EdgeKind::After),
    );
    let mut engine = leaf("engine", "engine", Kind::PrShip);
    file_mut(&mut engine).branch = Some("engine".to_string());
    file_mut(&mut engine).worktree = Some("/src/billing/.worktrees/engine".to_string());
    let mut backfill = planned(
        leaf("backfill", "backfill", Kind::PrShip),
        "usage backfilled",
    );
    {
        let file = file_mut(&mut backfill);
        file.todos[0].state = TodoState::Done;
        file.branch = Some("backfill".to_string());
        file.worktree = Some("/src/billing/.worktrees/backfill".to_string());
    }
    let tidy = leaf("tidy", "tidy", Kind::PrShip);
    // `archive` is closed by hand with an open leaf under it: done, so never written.
    let mut archive = group(
        "archive",
        "archive",
        vec![leaf("stray", "stray", Kind::PrShip)],
    );
    file_mut(&mut archive).status = Status::Done;
    let old = group(
        "old",
        "old",
        vec![
            leaf("one", "one", Kind::PrShip),
            finished(leaf("two", "two", Kind::PrShip)),
            archive,
        ],
    );
    let area = group(
        "area",
        "area",
        vec![
            leaf("piece", "piece", Kind::Other),
            finished(leaf("finished-piece", "finished piece", Kind::Other)),
        ],
    );
    let guide = after(
        leaf("guide", "guide", Kind::Document),
        "area",
        UnblockPoint::Merged,
    );
    tree(group(
        "billing",
        "proration",
        vec![schema, api, engine, backfill, tidy, old, area, guide],
    ))
}

const BACKFILL_CHAT: &str = "projects/billing/tasks/backfill/chats/c1/transcript.md";
const BACKFILL_RUN: &str = "projects/billing/tasks/backfill/runs/r1/log.txt";
const BACKFILL_NOTES: &str = "projects/billing/tasks/backfill/notes/plan.md";

fn billing_fixture() -> Fixture {
    Fixture::new(
        &billing_tree(),
        &[
            (BACKFILL_CHAT, "the chat"),
            (BACKFILL_RUN, "the run"),
            (BACKFILL_NOTES, "the notes"),
        ],
    )
}

const BILLING_PROPOSAL: &str = "\
task billing: proration | id id-billing
  task schema: schema | id id-schema
  task api: api | id id-api
  task engine: engine | id id-engine
  task backfill: backfill | id id-backfill
    task backfill-core: backfill core | keeps work
      todo implement | role produces-branch
      todo review
      todo ship | role ships
    task backfill-check: check the backfill
      after backfill-core
  task tidy: tidy the invoices | id id-tidy | kind pr-merge
    after api
    todo implement | role produces-branch
    todo code review
    todo merge | role ships
  task old: old | id id-old | drop
  task reporting: reporting
    task report-a: first report
    task report-b: second report
      after report-a
    task area: area | id id-area
  task guide: guide | id id-guide
";

fn billing_text() -> String {
    doc("billing", BILLING_PROPOSAL)
}

/// Builds and writes the billing proposal.
fn apply_billing() -> (Fixture, Parsed, Diff, Applied, WorkTree) {
    let fixture = billing_fixture();
    let text = billing_text();
    let (parsed, diff, applied) = build(&fixture, &text, &LiveSet::default());
    write_change(fixture.root(), &applied.change).expect("the change is written");
    assert!(journals(fixture.root()).is_empty());
    let after = fixture.reload();
    (fixture, parsed, diff, applied, after)
}

#[test]
fn the_billing_diff_has_every_kind_of_operation() {
    let fixture = billing_fixture();
    let (_, diff, applied) = build(&fixture, &billing_text(), &LiveSet::default());
    let has = |name: &str| {
        diff.ops
            .iter()
            .any(|op| format!("{op:?}").starts_with(name))
    };
    for name in [
        "Create",
        "Rename",
        "SetKind",
        "Todos",
        "AddEdge",
        "RemoveEdge",
        "Cancel",
        "Move",
        "Split",
        "Untouched",
    ] {
        assert!(has(name), "no {name} in {:#?}", diff.ops);
    }
    assert_eq!(
        applied.summary,
        "proposal 1 applied: created 5 tasks, 12 todos, 3 edges; changed 2 tasks, moved 1, \
         cancelled 1, split 1"
    );
    assert_eq!(
        applied.change.moves.len(),
        3,
        "area, then backfill's chats/ and runs/"
    );
}

#[test]
fn applying_then_diffing_again_gives_nothing() {
    let (_, parsed, _, applied, after) = apply_billing();
    let again = with_ids(&parsed, &applied.created);
    let diff = diff(&again, &after, &LiveSet::default(), &Choices::default());
    assert!(diff.is_empty(), "{:#?}", diff.ops);
}

#[test]
fn the_change_does_what_the_diff_lists() {
    let fixture = billing_fixture();
    let before = &fixture.tree;
    let text = billing_text();
    let (_, diff, applied) = build(&fixture, &text, &LiveSet::default());
    let before_bytes: BTreeMap<String, Vec<u8>> = before
        .tasks()
        .into_iter()
        .filter_map(Task::id)
        .map(|id| (id.to_string(), bytes(before, id)))
        .collect();
    write_change(fixture.root(), &applied.change).unwrap();
    let after = fixture.reload();
    let created = &applied.created;
    let id_of_ref = |task: &TaskRef| match task {
        TaskRef::Existing { id, .. } => id.clone(),
        TaskRef::New { line, .. } => created[line].clone(),
    };

    for op in &diff.ops {
        match op {
            Op::Create {
                line, parent, task, ..
            } => {
                let id = &created[line];
                let file = file(&after, id);
                assert_eq!(file.goal, task.goal);
                assert_eq!(file.kind, task.kind);
                assert_eq!(file.todos, task.todos);
                assert_eq!(file.planned_result, task.planned_result);
                assert_eq!(parent_id(&after, id), Some(id_of_ref(parent)));
                assert!(
                    layout::prompts_file(&task_dir(&after, id)).exists(),
                    "a new task has prompts.txt"
                );
            }
            Op::Rename { id, to, .. } => assert_eq!(&file(&after, id).goal, to),
            Op::SetKind { id, kind, .. } => assert_eq!(file(&after, id).kind, *kind),
            Op::SetPlannedResult { id, text, .. } => {
                assert_eq!(&file(&after, id).planned_result, text);
            }
            Op::Todos { id, todos, .. } => assert_eq!(&file(&after, id).todos, todos),
            Op::AddEdge {
                from,
                target,
                kind,
                unblock,
                ..
            } => {
                let from = file(&after, &id_of_ref(from));
                let target = task(&after, &id_of_ref(target)).slug.clone();
                let found = match kind {
                    bongos_work_model::rules::EdgeKind::After => from
                        .after
                        .iter()
                        .any(|edge| edge.task == target && edge.unblock == *unblock),
                    bongos_work_model::rules::EdgeKind::StackedOn => from
                        .stacked_on
                        .as_ref()
                        .is_some_and(|edge| edge.task == target && edge.unblock == *unblock),
                };
                assert!(found, "{op:?}");
            }
            Op::RemoveEdge { id, target, .. } => {
                let file = file(&after, id);
                assert!(
                    !file.after.iter().any(|edge| &edge.task == target),
                    "{op:?}"
                );
            }
            Op::Cancel { id, with, .. } => {
                for id in std::iter::once(id).chain(with) {
                    assert_eq!(file(&after, id).status, Status::Cancelled, "{id}");
                }
            }
            Op::Move { id, new_parent, .. } => {
                assert_eq!(parent_id(&after, id), Some(id_of_ref(new_parent)));
            }
            Op::Split {
                id, keeper, work, ..
            } => {
                assert_eq!(derive::shape(task(&after, id), &after), Shape::Group);
                let leaf = file(&after, id);
                assert!(leaf.todos.is_empty() && leaf.branch.is_none());
                let keeper = keeper.as_ref().expect("a keeper");
                let kept = file(&after, &created[&keeper.line]);
                assert_eq!(kept.branch, work.branch);
                assert_eq!(kept.worktree, work.worktree);
            }
            Op::PlanDone { .. } | Op::Kept { .. } | Op::Untouched { .. } => {}
        }
    }

    // Exactly the root and the tasks the diff changes are rewritten; every other task keeps its
    // bytes, wherever it now is.
    let mut expected = rewritten(&diff);
    expected.insert(diff.root);
    let changed: HashSet<String> = before_bytes
        .iter()
        .filter(|(id, old)| bytes(&after, id) != **old)
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(changed, expected);
    // Every existing task is still there, once; the created ones are added.
    let counts = id_counts(&after);
    assert!(counts.values().all(|count| *count == 1));
    assert_eq!(counts.len(), before_bytes.len() + created.len());

    // One history entry per written task, by the context, `proposal applied`.
    for id in expected.iter().chain(created.values()) {
        let history = &file(&after, id).history;
        let old = before
            .task_by_id(id)
            .and_then(|task| task.file.as_ref())
            .map_or(0, |file| file.history.len());
        assert_eq!(history.len(), old + 1, "{id}");
        let entry = history.last().unwrap();
        assert_eq!(entry.event, "proposal applied");
        assert_eq!(entry.by, "kyle");
        assert_eq!(entry.at, ctx().at);
    }
    let root = file(&after, &id_of("billing"));
    assert_eq!(
        root.history.last().unwrap().detail.as_deref(),
        Some(applied.summary.as_str())
    );
    assert_eq!(root.proposals.len(), 1);
    assert_eq!(root.proposals[0].number, 1);
    assert_eq!(root.proposals[0].state, ProposalState::Applied);
    assert_eq!(root.proposals[0].hash, file_hash(text.as_bytes()));
    let tidy = file(&after, &id_of("tidy"));
    assert_eq!(
        tidy.history.last().unwrap().detail.as_deref(),
        Some(
            "proposal 1: renamed \"tidy\" -> \"tidy the invoices\"; kind pr-ship -> pr-merge; \
             renamed todo 'review' -> 'code review'; renamed todo 'ship' -> 'merge'; added \
             edge 'after api'"
        )
    );
}

fn task_dir(tree: &WorkTree, id: &str) -> PathBuf {
    task(tree, id).dir.clone()
}

#[test]
fn a_split_moves_the_work_into_the_keeper() {
    let (fixture, _, _, applied, after) = apply_billing();
    let keeper_id = applied
        .created
        .values()
        .find(|id| file(&after, id).goal == "backfill core")
        .unwrap();
    let keeper = file(&after, keeper_id);
    assert_eq!(keeper.branch.as_deref(), Some("backfill"));
    assert_eq!(
        keeper.worktree.as_deref(),
        Some("/src/billing/.worktrees/backfill")
    );
    assert_eq!(keeper.planned_result.as_deref(), Some("usage backfilled"));
    assert_eq!(keeper.todos[0].state, TodoState::Done);
    assert_eq!(keeper.todos[0].role, Some(TodoRole::ProducesBranch));
    let keeper_dir = task_dir(&after, keeper_id);
    assert_eq!(
        fs::read_to_string(keeper_dir.join("chats/c1/transcript.md")).unwrap(),
        "the chat"
    );
    assert_eq!(
        fs::read_to_string(keeper_dir.join("runs/r1/log.txt")).unwrap(),
        "the run"
    );
    // The leaf keeps its own artifacts.
    assert_eq!(
        fs::read_to_string(fixture.root().join(BACKFILL_NOTES)).unwrap(),
        "the notes"
    );
    let leaf = file(&after, &id_of("backfill"));
    assert_eq!(leaf.planned_result, None);
    assert!(
        leaf.history
            .last()
            .unwrap()
            .detail
            .as_deref()
            .unwrap()
            .starts_with("proposal 1: split; moved: todos, planned result, worktree, branch")
    );
}

#[test]
fn done_tasks_keep_their_bytes() {
    let fixture = billing_fixture();
    let done = [
        id_of("schema"),
        id_of("two"),
        id_of("archive"),
        id_of("stray"),
        id_of("finished-piece"),
    ];
    let before: Vec<Vec<u8>> = done.iter().map(|id| bytes(&fixture.tree, id)).collect();
    let (_, diff, applied) = build(&fixture, &billing_text(), &LiveSet::default());
    // No write names a done task.
    for write in &applied.change.writes {
        if let Write::Task { file, .. } = write {
            assert!(!done.contains(&file.id), "{} is written", file.id);
        }
    }
    // `two` is under the cancelled `old`, but stays done.
    assert!(diff.ops.iter().any(|op| matches!(
        op,
        Op::Cancel { id, with, .. } if *id == id_of("old") && *with == vec![id_of("one")]
    )));
    write_change(fixture.root(), &applied.change).unwrap();
    let after = fixture.reload();
    let now: Vec<Vec<u8>> = done.iter().map(|id| bytes(&after, id)).collect();
    assert_eq!(now, before);
    // `finished-piece` moved with `area`, bytes and all.
    assert!(
        task_dir(&after, &id_of("finished-piece"))
            .to_string_lossy()
            .contains("reporting/tasks/area/tasks/finished-piece")
    );
}

#[test]
fn a_kill_between_any_two_journal_steps_recovers() {
    let extra = [
        (BACKFILL_CHAT, "the chat"),
        (BACKFILL_RUN, "the run"),
        (BACKFILL_NOTES, "the notes"),
    ];
    assert_recovers(&billing_tree(), &extra, &billing_text());
    let (memory, extra, text) = nested_split();
    assert_recovers(&memory, &extra, &text);
}

/// Applies `text` on `memory` with a kill after every journal step, and checks the loader always
/// finishes the journal into the one new tree: no task lost, none twice.
fn assert_recovers(memory: &WorkTree, extra: &[(&str, &str)], text: &str) {
    let fixture = Fixture::new(memory, extra);
    let (_, _, applied) = build(&fixture, text, &LiveSet::default());
    let (dir, journal) = match plan(fixture.root(), &applied.change).unwrap() {
        Plan::Journal { dir, journal } => (
            dir.strip_prefix(fixture.root()).unwrap().to_path_buf(),
            journal,
        ),
        Plan::One { .. } => panic!("the apply writes one file"),
    };
    assert!(journal.steps.len() > 5, "{:?}", journal.steps);

    // The tree the apply leaves (prompts.txt is written after the journal, so not here).
    let reference = TempDir::new().unwrap();
    materialize(memory, reference.path(), extra);
    write_journal(reference.path(), &reference.path().join(&dir), &journal).unwrap();
    run_steps(reference.path(), &journal, journal.steps.len()).unwrap();
    for path in journals(reference.path()) {
        fs::remove_file(path).unwrap();
    }
    let expected_tree = load(reference.path());
    assert_clean(&expected_tree);
    let expected_ids = id_counts(&expected_tree);
    assert!(expected_ids.values().all(|count| *count == 1));
    let expected = snapshot(reference.path());

    // With no journal written, the tree is the old one.
    let untouched = TempDir::new().unwrap();
    materialize(memory, untouched.path(), extra);
    let old = snapshot(untouched.path());
    assert_ne!(old, expected);

    // 0 is a journal written and no step run; `len` is every step run but the journal kept.
    for done in 0..=journal.steps.len() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        materialize(memory, root, extra);
        write_journal(root, &root.join(&dir), &journal).unwrap();
        run_steps(root, &journal, done).unwrap();
        let tree = load(root);
        assert!(journals(root).is_empty(), "journal left after {done} steps");
        assert_clean(&tree);
        assert_eq!(id_counts(&tree), expected_ids, "after {done} steps");
        assert_eq!(snapshot(root), expected, "after {done} steps");
    }
}

#[test]
fn a_stale_fingerprint_refuses_and_writes_nothing() {
    let fixture = billing_fixture();
    let text = billing_text();
    let parsed = checked_parse(&fixture.tree, &text);
    let live = LiveSet::default();
    let choices = Choices::default();
    let hash = file_hash(text.as_bytes());
    let previewed = fingerprint(
        &diff(&parsed, &fixture.tree, &live, &choices),
        &choices,
        &hash,
    );

    // A chat started on `tidy` between the preview and the confirm.
    let mut later = LiveSet::default();
    later.add_chat(id_of("tidy"));
    // The file changed between the preview and the confirm.
    let other_hash = file_hash(b"another file");
    // A todo of `tidy` was started on disk.
    let mut edited = file(&fixture.tree, &id_of("tidy")).clone();
    edited.todos[0].started = Some(timestamp("2026-11-09T11:00:00Z"));
    let started_root = TempDir::new().unwrap();
    materialize(&billing_tree(), started_root.path(), &[]);
    fs::write(
        layout::task_file(&started_root.path().join("projects/billing/tasks/tidy")),
        serialize_task(&edited).unwrap(),
    )
    .unwrap();
    let started_tree = load(started_root.path());

    let before = snapshot(fixture.root());
    for (tree, live, hash) in [
        (&fixture.tree, &later, hash.as_str()),
        (&fixture.tree, &live, other_hash.as_str()),
        (&started_tree, &live, hash.as_str()),
    ] {
        let refused = checked(&parsed, tree, &ctx(), live, &choices, &previewed, hash);
        assert_eq!(refused, Err(STALE.to_string()));
    }
    assert_eq!(
        STALE,
        "The tree changed since the preview; run task: apply proposal again"
    );
    assert_eq!(snapshot(fixture.root()), before);

    // The same tree and file apply.
    let change = checked(
        &parsed,
        &fixture.tree,
        &ctx(),
        &live,
        &choices,
        &previewed,
        &hash,
    )
    .expect("the previewed tree applies");
    write_change(fixture.root(), &change).unwrap();
    assert_ne!(snapshot(fixture.root()), before);
}

/// The request the app queues after previewing `text` on the fixture with no live work.
fn previewed_request(fixture: &Fixture, text: &str) -> Request {
    let parsed = checked_parse(&fixture.tree, text);
    let choices = Choices::default();
    let previewed = diff(&parsed, &fixture.tree, &LiveSet::default(), &choices);
    Request {
        fingerprint: fingerprint(&previewed, &choices, &file_hash(text.as_bytes())),
        parsed,
        choices,
    }
}

#[test]
fn a_request_applies_when_nothing_changed_since_the_preview() {
    let fixture = billing_fixture();
    let text = billing_text();
    let request = previewed_request(&fixture, &text);
    let change = request
        .run(&fixture.tree, &ctx(), &LiveSet::default(), text.as_bytes())
        .expect("an unchanged preview applies");
    let before = snapshot(fixture.root());
    write_change(fixture.root(), &change).unwrap();
    assert_ne!(snapshot(fixture.root()), before);
    // The record holds the hash of the bytes read at confirm time.
    let tree = fixture.reload();
    let record = &file(&tree, &id_of("billing")).proposals[0];
    assert_eq!(record.hash, file_hash(text.as_bytes()));
}

#[test]
fn a_request_is_stale_when_the_file_changed_after_the_preview() {
    let fixture = billing_fixture();
    let text = billing_text();
    let request = previewed_request(&fixture, &text);
    let before = snapshot(fixture.root());
    let edited = format!("{text}\n");
    let refused = request.run(
        &fixture.tree,
        &ctx(),
        &LiveSet::default(),
        edited.as_bytes(),
    );
    assert_eq!(refused, Err(STALE.to_string()));
    assert_eq!(snapshot(fixture.root()), before, "nothing is written");
}

#[test]
fn a_request_is_stale_when_work_went_live_after_the_preview() {
    let fixture = billing_fixture();
    let text = billing_text();
    let request = previewed_request(&fixture, &text);
    let before = snapshot(fixture.root());
    for add in [LiveSet::add_chat, LiveSet::add_run] {
        let mut live = LiveSet::default();
        add(&mut live, id_of("tidy"));
        let refused = request.run(&fixture.tree, &ctx(), &live, text.as_bytes());
        assert_eq!(refused, Err(STALE.to_string()));
    }
    assert_eq!(snapshot(fixture.root()), before, "nothing is written");
}

#[test]
fn a_done_root_takes_no_proposal() {
    let memory = tree(group(
        "shipped",
        "shipped",
        vec![finished(leaf("only", "only", Kind::PrShip))],
    ));
    let fixture = Fixture::new(&memory, &[]);
    let text = doc(
        "shipped",
        &format!(
            "task shipped: shipped | id {}\n  task only: only | id {}\n",
            id_of("shipped"),
            id_of("only")
        ),
    );
    let parsed = checked_parse(&fixture.tree, &text);
    let diff = diff(
        &parsed,
        &fixture.tree,
        &LiveSet::default(),
        &Choices::default(),
    );
    let refused = applied(
        &parsed,
        &fixture.tree,
        &diff,
        &ctx(),
        &file_hash(text.as_bytes()),
    )
    .expect_err("a done root is refused");
    assert!(refused.contains("is done"), "{refused}");
}

#[test]
fn a_reject_writes_only_the_record() {
    let fixture = billing_fixture();
    let before = snapshot(fixture.root());
    let hash = file_hash(billing_text().as_bytes());
    let change = reject(&fixture.tree, &ctx(), &id_of("billing"), 1, &hash).unwrap();
    assert!(change.moves.is_empty());
    assert_eq!(change.writes.len(), 1);
    write_change(fixture.root(), &change).unwrap();
    let after = snapshot(fixture.root());
    let changed: Vec<&PathBuf> = after
        .keys()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert_eq!(changed, [&PathBuf::from("projects/billing/task.toml")]);
    assert_eq!(before.len(), after.len());

    let tree = fixture.reload();
    let mut expected = file(&fixture.tree, &id_of("billing")).clone();
    let written = file(&tree, &id_of("billing"));
    assert_eq!(written.proposals.len(), 1);
    let record = &written.proposals[0];
    assert_eq!(
        (record.number, record.state, record.by.as_str(), record.at),
        (1, ProposalState::Rejected, "kyle", ctx().at)
    );
    assert_eq!(record.hash, hash);
    expected.proposals = written.proposals.clone();
    assert_eq!(*written, expected, "nothing but the record changes");

    // Rejecting it again, edited, replaces the record.
    let edited = file_hash(b"edited");
    let again = reject(&tree, &ctx(), &id_of("billing"), 1, &edited).unwrap();
    write_change(fixture.root(), &again).unwrap();
    let tree = fixture.reload();
    let records = &file(&tree, &id_of("billing")).proposals;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].hash, edited);
}

#[test]
fn a_sixty_task_proposal_applies_in_one_journal() {
    let memory = tree(group(
        "big",
        "a big plan",
        vec![leaf("existing", "existing", Kind::PrShip)],
    ));
    let fixture = Fixture::new(&memory, &[]);
    let mut body = "task big: a big plan | id id-big\n  task existing: existing | id id-existing\n"
        .to_string();
    for area in 0..6 {
        body.push_str(&format!("  task area-{area}: area {area}\n"));
        for piece in 0..9 {
            body.push_str(&format!("    task piece-{piece}: piece {area}.{piece}\n"));
            if piece > 0 {
                body.push_str(&format!("      after piece-{}\n", piece - 1));
            }
        }
    }
    let text = doc("big", &body);
    let (parsed, diff, applied) = build(&fixture, &text, &LiveSet::default());
    assert_eq!(applied.created.len(), 60);
    match plan(fixture.root(), &applied.change).unwrap() {
        Plan::Journal { journal, .. } => {
            let writes = journal
                .steps
                .iter()
                .filter(|step| matches!(step, bongos_work_model::journal::Step::WriteFile { .. }))
                .count();
            assert_eq!(writes, 61, "60 new tasks and the root, in one journal");
        }
        Plan::One { .. } => panic!("one file"),
    }
    write_change(fixture.root(), &applied.change).unwrap();
    assert!(journals(fixture.root()).is_empty());
    let after = fixture.reload();
    assert_eq!(after.tasks().len(), 62);
    assert!(
        applied
            .summary
            .starts_with("proposal 1 applied: created 60 tasks, 162 todos, 48 edges")
    );
    let again = with_ids(&parsed, &applied.created);
    let redo = crate::diff::diff(&again, &after, &LiveSet::default(), &Choices::default());
    assert!(redo.is_empty(), "{:#?}", redo.ops);
    assert!(!diff.is_empty());
}

// The plan todo (D9).

fn plan_todo() -> bongos_work_model::Todo {
    todo("plan", Some(TodoRole::Plans), TodoState::Open)
}

fn planned_leaf_tree() -> WorkTree {
    tree(group(
        "fixes",
        "fixes",
        vec![leaf_with(
            "rounding",
            "fix rounding",
            Kind::PrShip,
            vec![plan_todo()],
        )],
    ))
}

#[test]
fn a_plan_that_keeps_the_leaf_closes_the_plan_todo_first() {
    let fixture = Fixture::new(&planned_leaf_tree(), &[]);
    let text = doc(
        "rounding",
        "task rounding: fix rounding | id id-rounding\n  result a PR fixing rounding\n  todo \
         implement\n  todo review\n  todo ship\n",
    );
    let (parsed, _, applied) = build(&fixture, &text, &LiveSet::default());
    write_change(fixture.root(), &applied.change).unwrap();
    let after = fixture.reload();
    let leaf = file(&after, &id_of("rounding"));
    let names: Vec<(&str, TodoState)> = leaf
        .todos
        .iter()
        .map(|todo| (todo.name.as_str(), todo.state))
        .collect();
    assert_eq!(
        names,
        [
            ("plan", TodoState::Done),
            ("implement", TodoState::Open),
            ("review", TodoState::Open),
            ("ship", TodoState::Open)
        ]
    );
    assert_eq!(leaf.planned_result.as_deref(), Some("a PR fixing rounding"));
    assert_eq!(leaf.todos[1].role, Some(TodoRole::ProducesBranch));
    let detail = leaf.history.last().unwrap().detail.clone().unwrap();
    assert!(
        detail.starts_with("proposal 1 applied: created 0 tasks"),
        "{detail}"
    );
    assert!(detail.contains("plan done: todo 'plan' done"), "{detail}");
    let again = diff(&parsed, &after, &LiveSet::default(), &Choices::default());
    assert!(again.is_empty(), "{:#?}", again.ops);
}

#[test]
fn a_plan_that_makes_a_group_records_the_plan_todo_done() {
    let fixture = Fixture::new(&planned_leaf_tree(), &[]);
    let text = doc(
        "rounding",
        "task rounding: fix rounding | id id-rounding\n  task totals: fix the totals\n  task \
         tests: cover the totals\n    after totals\n",
    );
    let (parsed, _, applied) = build(&fixture, &text, &LiveSet::default());
    write_change(fixture.root(), &applied.change).unwrap();
    let after = fixture.reload();
    let root = task(&after, &id_of("rounding"));
    assert_eq!(derive::shape(root, &after), Shape::Group);
    let file = root.file.as_ref().unwrap();
    assert!(file.todos.is_empty());
    let detail = file.history.last().unwrap().detail.clone().unwrap();
    assert_eq!(
        detail,
        "proposal 1 applied: created 2 tasks, 6 todos, 1 edge; became a group; plan done: todo \
         'plan'"
    );
    let again = with_ids(&parsed, &applied.created);
    let redo = diff(&again, &after, &LiveSet::default(), &Choices::default());
    assert!(redo.is_empty(), "{:#?}", redo.ops);
}

#[test]
fn a_bad_file_under_a_moved_task_refuses_the_whole_apply() {
    let fixture = billing_fixture();
    let broken = fixture
        .root()
        .join("projects/billing/tasks/area/tasks/piece/task.toml");
    fs::write(&broken, "this is not toml = [").unwrap();
    let tree = load(fixture.root());
    let before = snapshot(fixture.root());
    let text = billing_text();
    let parsed = parse(&text);
    let diff = diff(&parsed, &tree, &LiveSet::default(), &Choices::default());
    let refusal = diff.refusal.clone().expect("the move of area is refused");
    assert!(refusal.contains("piece"), "{refusal}");
    let result = applied(&parsed, &tree, &diff, &ctx(), &file_hash(text.as_bytes()));
    assert_eq!(result.map(|applied| applied.change), Err(refusal));
    assert_eq!(snapshot(fixture.root()), before);
}

#[test]
fn a_diff_of_another_tree_is_refused() {
    // Built on one tree and handed another: an edge the diff removes is gone.
    let fixture = billing_fixture();
    let text = billing_text();
    let parsed = checked_parse(&fixture.tree, &text);
    let diff = diff(
        &parsed,
        &fixture.tree,
        &LiveSet::default(),
        &Choices::default(),
    );
    let other = Fixture::new(
        &tree(group(
            "billing",
            "proration",
            vec![leaf("api", "api", Kind::PrShip)],
        )),
        &[],
    );
    assert!(applied(&parsed, &other.tree, &diff, &ctx(), "hash").is_err());
}

const NESTED_CHAT: &str = "projects/work/tasks/area/tasks/piece/chats/c1/transcript.md";

/// `area` moves under a new group while its leaf `piece`, which has done work and a chat, is split.
fn nested_split() -> (WorkTree, [(&'static str, &'static str); 1], String) {
    let piece = leaf_with(
        "piece",
        "piece",
        Kind::Other,
        vec![
            todo("implement", None, TodoState::Done),
            todo("review", None, TodoState::Open),
        ],
    );
    let memory = tree(group(
        "work",
        "work",
        vec![group("area", "area", vec![piece])],
    ));
    let text = doc(
        "work",
        "task work: work | id id-work\n  task reporting: reporting\n    task area: area | id \
         id-area\n      task piece: piece | id id-piece\n        task piece-core: piece core | \
         keeps work\n          todo implement\n          todo review\n        task piece-more: \
         more\n          after piece-core\n",
    );
    (memory, [(NESTED_CHAT, "the chat")], text)
}

#[test]
fn a_leaf_split_inside_a_moved_group_takes_its_chats_along() {
    let (memory, extra, text) = nested_split();
    let chat = NESTED_CHAT;
    let fixture = Fixture::new(&memory, &extra);
    let (parsed, first, applied) = build(&fixture, &text, &LiveSet::default());
    assert!(first.ops.iter().any(|op| matches!(op, Op::Move { .. })));
    assert!(first.ops.iter().any(|op| matches!(op, Op::Split { .. })));
    write_change(fixture.root(), &applied.change).unwrap();
    assert!(journals(fixture.root()).is_empty());
    let after = fixture.reload();
    let keeper = applied
        .created
        .values()
        .find(|id| file(&after, id).goal == "piece core")
        .unwrap();
    let keeper_dir = task_dir(&after, keeper);
    assert!(
        keeper_dir.ends_with("reporting/tasks/area/tasks/piece/tasks/piece-core"),
        "{}",
        keeper_dir.display()
    );
    assert_eq!(
        fs::read_to_string(keeper_dir.join("chats/c1/transcript.md")).unwrap(),
        "the chat"
    );
    assert!(!fixture.root().join(chat).exists());
    assert_eq!(file(&after, keeper).todos[0].state, TodoState::Done);
    let again = with_ids(&parsed, &applied.created);
    let redo = diff(&again, &after, &LiveSet::default(), &Choices::default());
    assert!(redo.is_empty(), "{:#?}", redo.ops);
}

#[test]
fn a_locked_goal_is_never_written() {
    // The diff comes from an unlinked `tidy`, so it renames it; the apply runs on a tree where
    // `tidy` is synced with Linear, and refuses without writing anything.
    let unlinked = billing_fixture();
    let text = billing_text();
    let parsed = checked_parse(&unlinked.tree, &text);
    let diff = diff(
        &parsed,
        &unlinked.tree,
        &LiveSet::default(),
        &Choices::default(),
    );
    assert!(
        diff.ops
            .iter()
            .any(|op| matches!(op, Op::Rename { id, .. } if *id == id_of("tidy"))),
        "{:?}",
        diff.ops
    );

    let locked_root = TempDir::new().unwrap();
    materialize(&billing_tree(), locked_root.path(), &[]);
    let mut locked = file(&unlinked.tree, &id_of("tidy")).clone();
    locked.linear = Some("SEQ-121".to_string());
    locked.linear_state = Some(bongos_work_model::LinearState {
        issue_id: "6f0c".to_string(),
        ..Default::default()
    });
    fs::write(
        layout::task_file(&locked_root.path().join("projects/billing/tasks/tidy")),
        serialize_task(&locked).unwrap(),
    )
    .unwrap();
    let tree = load(locked_root.path());
    let before = snapshot(locked_root.path());
    let refused = applied(&parsed, &tree, &diff, &ctx(), &file_hash(text.as_bytes()));
    assert_eq!(
        refused.err().as_deref(),
        Some(
            "tidy: the title comes from Linear (SEQ-121); change it there or run task: open in linear"
        )
    );
    assert_eq!(snapshot(locked_root.path()), before);
}

/// The kind of the author of an apply, as the root's proposal record stores it.
fn recorded_kind(ctx: &Context) -> (String, Option<ByKind>) {
    let fixture = billing_fixture();
    let (_, _, applied) = build_by(&fixture, &billing_text(), &LiveSet::default(), ctx);
    write_change(fixture.root(), &applied.change).expect("the change is written");
    let after = fixture.reload();
    let record = file(&after, &id_of("billing")).proposals[0].clone();
    (record.by, record.by_kind)
}

#[test]
fn the_proposal_record_stores_the_kind_of_the_author_that_applied_it() {
    let at = timestamp("2026-11-09T12:00:00Z");
    assert_eq!(
        recorded_kind(&Context::new("kyle", at)),
        ("kyle".to_string(), Some(ByKind::Unattributed))
    );
    let person = Context::at_keyboard(PersonAtKeyboard::for_test());
    assert_eq!(recorded_kind(&person).1, Some(ByKind::Person));
    let agent = Context::new("agent", at).with_author(Author::agent("agent").with_chat("c1"));
    assert_eq!(
        recorded_kind(&agent),
        ("agent".to_string(), Some(ByKind::Agent))
    );
    let workflow = Context::new("w", at).with_author(Author::workflow("linear sync"));
    assert_eq!(
        recorded_kind(&workflow),
        ("linear sync".to_string(), Some(ByKind::Workflow))
    );
}
