use std::path::PathBuf;

use bongos_work_model::store::{FileError, Task, WorkTree};
use bongos_work_model::{
    Kind, ResultKind, ResultState, Status, TaskResult, Todo, TodoRole, TodoState, UnblockPoint,
};

use crate::diff::{Choices, Diff, KeeperSource, Op, TaskRef, TodoOp, diff, fingerprint};
use crate::print::LiveSet;
use crate::test_support::*;
use crate::validate::{Checks, validate};
use crate::{Parsed, Severity, parse, print};

/// A proposal for the task `root`: three header lines, a blank line, then `body`, whose first
/// line is line 4.
fn doc(root: &str, body: &str) -> String {
    format!("proposal 1\nfor {}\n\n{body}", id_of(root))
}

/// Parses `text`, checks the precondition (no parse or validation error) and diffs it.
fn run_with(tree: &WorkTree, text: &str, live: &LiveSet, choices: &Choices) -> Diff {
    let parsed = checked(tree, text);
    diff(&parsed, tree, live, choices)
}

fn checked(tree: &WorkTree, text: &str) -> Parsed {
    let parsed = parse(text);
    assert_eq!(
        parsed.problems,
        vec![],
        "the fixture must parse cleanly:\n{text}"
    );
    let errors: Vec<_> = validate(&parsed, tree, &Checks::default())
        .into_iter()
        .filter(|problem| problem.severity == Severity::Error)
        .collect();
    assert_eq!(errors, vec![], "the fixture must validate:\n{text}");
    parsed
}

fn run(tree: &WorkTree, text: &str) -> Diff {
    run_with(tree, text, &LiveSet::default(), &Choices::default())
}

fn slug_of(tree: &WorkTree, id: &str) -> String {
    tree.task_by_id(id)
        .map_or_else(|| id.to_string(), |task| task.slug.clone())
}

/// Every `Kept` as `slug: what: reason`.
fn kept(diff: &Diff, tree: &WorkTree) -> Vec<String> {
    diff.ops
        .iter()
        .filter_map(|op| match op {
            Op::Kept {
                id, what, reason, ..
            } => Some(format!("{}: {what}: {reason}", slug_of(tree, id))),
            _ => None,
        })
        .collect()
}

fn untouched(diff: &Diff, tree: &WorkTree) -> Vec<String> {
    diff.ops
        .iter()
        .filter_map(|op| match op {
            Op::Untouched { id } => Some(slug_of(tree, id)),
            _ => None,
        })
        .collect()
}

fn creates(diff: &Diff) -> Vec<String> {
    diff.ops
        .iter()
        .filter_map(|op| match op {
            Op::Create { label, parent, .. } => Some(format!("{label} under {}", parent.name())),
            _ => None,
        })
        .collect()
}

/// The operations other than `Kept` and `Untouched`, as short text.
fn changes(diff: &Diff, tree: &WorkTree) -> Vec<String> {
    diff.ops
        .iter()
        .filter_map(|op| {
            Some(match op {
                Op::Create { label, .. } => format!("create {label}"),
                Op::Rename { id, to, .. } => format!("rename {} to {to}", slug_of(tree, id)),
                Op::SetKind { id, kind, .. } => {
                    format!(
                        "kind {} {}",
                        slug_of(tree, id),
                        crate::grammar::kind_name(*kind)
                    )
                }
                Op::SetPlannedResult { id, text, .. } => {
                    format!("result {} {text:?}", slug_of(tree, id))
                }
                Op::Todos { id, ops, .. } => format!("todos {} {ops:?}", slug_of(tree, id)),
                Op::AddEdge {
                    from,
                    target,
                    kind,
                    default,
                    ..
                } => format!(
                    "add {} {kind:?} {}{}",
                    from.name(),
                    target.name(),
                    if *default { " (default)" } else { "" }
                ),
                Op::RemoveEdge {
                    id, target, kind, ..
                } => format!("remove {} {kind:?} {target}", slug_of(tree, id)),
                Op::Cancel { id, with, .. } => format!(
                    "cancel {} with {:?}",
                    slug_of(tree, id),
                    with.iter().map(|id| slug_of(tree, id)).collect::<Vec<_>>()
                ),
                Op::Move { id, new_parent, .. } => {
                    format!("move {} under {}", slug_of(tree, id), new_parent.name())
                }
                Op::Split { id, keeper, .. } => format!(
                    "split {} keeper {:?}",
                    slug_of(tree, id),
                    keeper
                        .as_ref()
                        .map(|keeper| (keeper.label.clone(), keeper.source))
                ),
                Op::PlanDone { id, todo, .. } => format!("plan done {} {todo}", slug_of(tree, id)),
                Op::Kept { .. } | Op::Untouched { .. } => return None,
            })
        })
        .collect()
}

fn todo_list(diff: &Diff, slug: &str, tree: &WorkTree) -> Option<Vec<Todo>> {
    diff.ops.iter().find_map(|op| match op {
        Op::Todos { id, todos, .. } if slug_of(tree, id) == slug => Some(todos.clone()),
        _ => None,
    })
}

fn started(mut todo: Todo) -> Todo {
    todo.started = Some(timestamp("2026-11-08T11:00:00Z"));
    todo
}

fn plan_todo() -> Todo {
    started(todo("plan", Some(TodoRole::Plans), TodoState::Open))
}

fn with_file(mut task: Task, edit: impl FnOnce(&mut bongos_work_model::TaskFile)) -> Task {
    edit(file_mut(&mut task));
    task
}

/// Replaces the first `from` after the first line containing `anchor`.
fn edit_after(text: &str, anchor: &str, from: &str, to: &str) -> String {
    let start = text.find(anchor).expect("the anchor is in the text");
    let at = start
        + text[start..]
            .find(from)
            .expect("the text to replace follows the anchor");
    format!("{}{to}{}", &text[..at], &text[at + from.len()..])
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|line| line.contains(needle))
        .expect("the needle is in the text")
        + 1
}

// Scenario 1: one leaf, plan done, three todos added.

fn rounding_tree() -> WorkTree {
    tree(group(
        "fixes",
        "fixes",
        vec![leaf_with(
            "rounding",
            "fix rounding in invoice totals",
            Kind::PrShip,
            vec![plan_todo()],
        )],
    ))
}

#[test]
fn scenario_1_a_plan_keeps_a_leaf_and_adds_its_todos() {
    let tree = rounding_tree();
    let mut live = LiveSet::default();
    live.add_chat(id_of("rounding"));
    for plan_line in ["  todo plan | role plans | started\n", ""] {
        let text = doc(
            "rounding",
            &format!(
                "task rounding: fix rounding in invoice totals | id id-rounding\n  result a PR \
                 fixing rounding\n{plan_line}  todo implement | role produces-branch\n  todo \
                 review\n  todo ship | role ships\n"
            ),
        );
        let diff = run_with(&tree, &text, &live, &Choices::default());
        assert_eq!(kept(&diff, &tree), Vec::<String>::new(), "{text}");
        assert_eq!(
            changes(&diff, &tree),
            vec![
                "plan done rounding plan".to_string(),
                "result rounding Some(\"a PR fixing rounding\")".to_string(),
                "todos rounding [Add { name: \"implement\", index: 1, after_frozen: None }, Add \
                 { name: \"review\", index: 2, after_frozen: None }, Add { name: \"ship\", index: \
                 3, after_frozen: None }]"
                    .to_string(),
            ]
        );
        let todos = todo_list(&diff, "rounding", &tree).expect("a todo list");
        let names: Vec<&str> = todos.iter().map(|todo| todo.name.as_str()).collect();
        assert_eq!(names, ["plan", "implement", "review", "ship"]);
        assert_eq!(
            todos[0],
            plan_todo(),
            "the plan todo is unchanged; PlanDone closes it"
        );
        assert_eq!(todos[1].role, Some(TodoRole::ProducesBranch));
        assert_eq!(todos[3].role, Some(TodoRole::Ships));
    }
}

// Scenario 2: one task to a five-leaf tree.

const D3_BODY: &str = "\
task billing: implement proration for billing | id id-billing
  task schema: add proration columns to plans
    result a PR adding proration columns
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task engine: proration engine
    stacked on schema
    result a PR with the proration engine
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task integration: use proration in invoices
    after engine
    result a PR wiring proration into invoice totals
  task api: proration endpoint
    after integration
    result a PR adding the endpoint
  task deploy: deploy proration | kind other
    after schema | unblock merged
    after engine
    after integration
    after api
    result proration live in production
    todo deploy
";

fn planned_billing() -> WorkTree {
    tree(group(
        "work",
        "work",
        vec![leaf_with(
            "billing",
            "implement proration for billing",
            Kind::PrShip,
            vec![plan_todo()],
        )],
    ))
}

#[test]
fn scenario_2_a_plan_makes_a_group_of_five_leaves() {
    let tree = planned_billing();
    let mut live = LiveSet::default();
    // The plan chat runs on the leaf; its chats stay, so the plan still applies.
    live.add_chat(id_of("billing"));
    let diff = run_with(&tree, &doc("billing", D3_BODY), &live, &Choices::default());
    assert_eq!(kept(&diff, &tree), Vec::<String>::new());
    assert_eq!(
        creates(&diff),
        [
            "schema under billing",
            "engine under billing",
            "integration under billing",
            "api under billing",
            "deploy under billing"
        ]
    );
    let todos: usize = diff
        .ops
        .iter()
        .map(|op| match op {
            Op::Create { task, .. } => task.todos.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(todos, 13, "3 + 3 + 3 (pr-ship defaults) + 3 (defaults) + 1");
    let edges = diff
        .ops
        .iter()
        .filter(|op| matches!(op, Op::AddEdge { .. }))
        .count();
    assert_eq!(edges, 7);
    let split = diff.ops.iter().find_map(|op| match op {
        Op::Split { keeper, work, .. } => Some((keeper.clone(), work.todos.clone())),
        _ => None,
    });
    assert_eq!(split, Some((None, vec![plan_todo()])));
    assert!(changes(&diff, &tree).contains(&"plan done billing plan".to_string()));
    assert_eq!(untouched(&diff, &tree), Vec::<String>::new());
    // A new pr-ship leaf whose todo lines carry no role gets them at creation (D3).
    let deploy = diff.ops.iter().find_map(|op| match op {
        Op::Create { label, task, .. } if label == "deploy" => Some(task.clone()),
        _ => None,
    });
    let deploy = deploy.expect("deploy is created");
    assert_eq!(deploy.kind, Kind::Other);
    assert_eq!(deploy.todos.len(), 1);
    assert_eq!(
        deploy.planned_result.as_deref(),
        Some("proration live in production")
    );
}

// Scenario 4: done and live tasks untouched; a drop of a live leaf kept; a todo rename.

fn scenario_4_tree() -> WorkTree {
    let schema = with_file(
        planned(
            leaf("schema", "add proration columns", Kind::PrShip),
            "columns",
        ),
        |file| {
            file.worktree = Some("/w/worktrees/schema".to_string());
            file.branch = Some("schema".to_string());
        },
    );
    let engine = planned(leaf("engine", "proration engine", Kind::PrShip), "engine");
    let integration = planned(
        finished(leaf(
            "integration",
            "use proration in invoices",
            Kind::PrShip,
        )),
        "wired",
    );
    let api = after(
        planned(leaf("api", "proration endpoint", Kind::PrShip), "endpoint"),
        "integration",
        UnblockPoint::Merged,
    );
    tree(group(
        "billing",
        "implement proration for billing",
        vec![schema, engine, integration, api],
    ))
}

#[test]
fn scenario_4_done_and_live_tasks_are_not_disturbed() {
    let tree = scenario_4_tree();
    let mut live = LiveSet::default();
    live.add_chat(id_of("engine"));
    let printed = print(&tree, &id_of("billing"), &live).expect("prints");
    let text = edit_after(&printed, "task engine:", " | state", " | drop | state");
    let text = edit_after(&text, "task api:", "todo review", "todo code review");
    let diff = run_with(&tree, &text, &live, &Choices::default());
    let engine_line = line_of(&text, "task engine:");
    assert!(diff.ops.contains(&Op::Kept {
        id: id_of("engine"),
        line: engine_line,
        what: "not removed".to_string(),
        reason: "has a running chat".to_string(),
    }));
    assert_eq!(kept(&diff, &tree).len(), 1, "{:?}", kept(&diff, &tree));
    assert_eq!(
        changes(&diff, &tree),
        vec!["todos api [Rename { index: 1, from: \"review\", to: \"code review\" }]".to_string()]
    );
    assert_eq!(
        untouched(&diff, &tree),
        ["billing", "schema", "integration"]
    );
    assert_eq!(diff.refusal, None);
}

#[test]
fn a_printed_tree_diffs_to_nothing() {
    let mut live = LiveSet::default();
    live.add_chat(id_of("engine"));
    for (tree, root) in [
        (scenario_4_tree(), "billing"),
        (rounding_tree(), "fixes"),
        (keeper_tree(), "billing"),
    ] {
        let printed = print(&tree, &id_of(root), &live).expect("prints");
        let diff = run_with(&tree, &printed, &live, &Choices::default());
        assert!(diff.is_empty(), "{printed}\n{:?}", diff.ops);
    }
}

#[test]
fn a_goal_rename_renames_and_creates_nothing() {
    let tree = scenario_4_tree();
    let mut live = LiveSet::default();
    live.add_chat(id_of("engine"));
    let printed = print(&tree, &id_of("billing"), &live).expect("prints");
    let text = printed
        .replace(
            "task engine: proration engine",
            "task engine: the proration engine",
        )
        .replace(
            "task schema: add proration columns",
            "task schema: add the columns",
        );
    let diff = run_with(&tree, &text, &live, &Choices::default());
    assert_eq!(
        changes(&diff, &tree),
        [
            "rename engine to the proration engine",
            "rename schema to add the columns"
        ]
    );
    assert_eq!(kept(&diff, &tree), Vec::<String>::new());
    assert_eq!(creates(&diff), Vec::<String>::new());
}

#[test]
fn a_task_left_out_is_untouched_and_only_drop_cancels() {
    let tree = tree(group(
        "billing",
        "proration",
        vec![
            leaf("schema", "schema", Kind::PrShip),
            group(
                "old",
                "old work",
                vec![
                    leaf("one", "one", Kind::PrShip),
                    finished(leaf("two", "two", Kind::PrShip)),
                ],
            ),
        ],
    ));
    let text = doc("billing", "task billing: proration | id id-billing\n");
    let diff = run(&tree, &text);
    assert_eq!(changes(&diff, &tree), Vec::<String>::new());
    assert!(diff.is_empty());
    assert_eq!(
        untouched(&diff, &tree),
        ["billing", "schema", "old", "one", "two"]
    );

    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task old: old work | id id-old | drop\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(changes(&diff, &tree), ["cancel old with [\"one\"]"]);
    assert_eq!(untouched(&diff, &tree), ["billing", "schema", "two"]);

    // A task listed before its dropped parent cannot move out of it: it is cancelled with it.
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task one: one | id id-one\n  task old: old \
         work | id id-old | drop\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(changes(&diff, &tree), ["cancel old with [\"one\"]"]);
    assert_eq!(
        kept(&diff, &tree),
        ["one: not moved under 'billing': 'old' is dropped"]
    );
}

fn live_leaf(edit: impl FnOnce(&mut bongos_work_model::TaskFile)) -> WorkTree {
    tree(group(
        "billing",
        "proration",
        vec![
            leaf("schema", "schema", Kind::PrShip),
            with_file(
                after(
                    leaf("engine", "engine", Kind::PrShip),
                    "schema",
                    UnblockPoint::Merged,
                ),
                edit,
            ),
        ],
    ))
}

#[test]
fn every_live_source_keeps_a_drop_with_its_reason() {
    let pr = |state| TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://example.com/pr/1".to_string()),
        state: Some(state),
        urls: Vec::new(),
        extra: Default::default(),
    };
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task engine: engine | id id-engine | drop\n    \
         after schema\n",
    );
    let cases: Vec<(WorkTree, LiveSet, &str)> = vec![
        (
            live_leaf(|file| file.worktree = Some("/w/wt".to_string())),
            LiveSet::default(),
            "has a worktree",
        ),
        (
            live_leaf(|file| file.branch = Some("engine".to_string())),
            LiveSet::default(),
            "has a branch",
        ),
        (
            live_leaf(|file| file.result = Some(pr(ResultState::Open))),
            LiveSet::default(),
            "has an open PR",
        ),
        (
            live_leaf(|file| file.result = Some(pr(ResultState::Approved))),
            LiveSet::default(),
            "has an open PR",
        ),
        (
            live_leaf(|_| {}),
            {
                let mut live = LiveSet::default();
                live.add_chat(id_of("engine"));
                live
            },
            "has a running chat",
        ),
        (
            live_leaf(|_| {}),
            {
                let mut live = LiveSet::default();
                live.add_run(id_of("engine"));
                live
            },
            "has a running run",
        ),
        (
            live_leaf(|_| {}),
            {
                let mut live = LiveSet::default();
                live.running.insert(id_of("engine"));
                live
            },
            "has running work",
        ),
    ];
    for (tree, live, reason) in cases {
        let diff = run_with(&tree, &text, &live, &Choices::default());
        assert_eq!(
            kept(&diff, &tree),
            [format!("engine: not removed: {reason}")]
        );
        assert_eq!(changes(&diff, &tree), Vec::<String>::new());
    }

    // A merged PR is not live: the drop goes through.
    let tree = live_leaf(|file| file.result = Some(pr(ResultState::Merged)));
    let diff = run(&tree, &text);
    assert_eq!(changes(&diff, &tree), ["cancel engine with []"]);
}

#[test]
fn a_group_is_live_through_its_descendants() {
    let tree = tree(group(
        "billing",
        "proration",
        vec![group(
            "backend",
            "backend",
            vec![leaf("engine", "engine", Kind::PrShip)],
        )],
    ));
    let mut live = LiveSet::default();
    live.add_chat(id_of("engine"));
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task backend: backend | id id-backend | drop\n",
    );
    let diff = run_with(&tree, &text, &live, &Choices::default());
    assert_eq!(
        kept(&diff, &tree),
        ["backend: not removed: 'engine' has a running chat"]
    );
}

#[test]
fn a_live_task_may_be_renamed_and_gain_todos_and_after_edges_only() {
    let mut live = LiveSet::default();
    live.add_run(id_of("engine"));
    let tree = tree(group(
        "billing",
        "proration",
        vec![
            leaf("schema", "schema", Kind::PrShip),
            leaf("other", "other", Kind::PrShip),
            after(
                leaf("engine", "engine", Kind::PrShip),
                "schema",
                UnblockPoint::Merged,
            ),
        ],
    ));
    // Renamed, a new todo, a new `after`, the old edge dropped, a `stacked on` added, a new kind.
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task schema: schema | id id-schema\n    todo \
         implement | role produces-branch\n    todo review\n    todo ship | role ships\n  task \
         other: other | id id-other\n    todo implement | role produces-branch\n    todo review\n    \
         todo ship | role ships\n  task engine: the engine | id id-engine | kind pr-merge\n    \
         after other\n    stacked on schema\n    todo implement | role produces-branch\n    todo \
         review\n    todo ship | role ships\n    todo docs\n",
    );
    let diff = run_with(&tree, &text, &live, &Choices::default());
    assert_eq!(
        changes(&diff, &tree),
        [
            "rename engine to the engine",
            "add engine After other",
            "todos engine [Add { name: \"docs\", index: 3, after_frozen: None }]",
        ]
    );
    assert_eq!(
        kept(&diff, &tree),
        [
            "engine: kind not changed to pr-merge: has a running run",
            "engine: edge 'engine after schema' not removed: has a running run",
            "engine: edge 'engine stacked on schema' not added: has a running run",
        ]
    );
}

#[test]
fn done_and_cancelled_tasks_are_never_touched() {
    let done = with_file(
        after(
            planned(finished(leaf("schema", "schema", Kind::PrShip)), "columns"),
            "spec",
            UnblockPoint::Merged,
        ),
        |_| {},
    );
    let cancelled = with_file(leaf("old", "old", Kind::PrShip), |file| {
        file.status = Status::Cancelled;
    });
    let shipped = finished(leaf("shipped-one", "one", Kind::PrShip));
    let tree = tree(group(
        "billing",
        "proration",
        vec![
            leaf("spec", "spec", Kind::Document),
            done,
            cancelled,
            group("shipped", "shipped", vec![shipped]),
        ],
    ));
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task spec: spec | id id-spec\n    todo \
         draft\n    todo review\n    todo publish\n  task schema: renamed | id id-schema | kind \
         spike | drop\n    result other columns\n    todo implement | role produces-branch\n    \
         todo ship | role ships\n  task old: old | id id-old | drop\n    after spec\n  task \
         shipped: shipped | id id-shipped\n    task shipped-one: one | id id-shipped-one\n      \
         result something\n      todo implement\n    task extra: a new task\n      result r\n  \
         task fresh: fresh\n    after schema\n    result r\n",
    );
    let diff = run(&tree, &text);
    for op in &diff.ops {
        match op {
            Op::Kept { .. } | Op::Untouched { .. } => {}
            Op::AddEdge { from, target, .. } => {
                // A new sibling may point at a done task; the edge is stored on the new task.
                assert!(matches!(from, TaskRef::New { .. }), "{op:?}");
                assert_eq!(target.name(), "schema");
            }
            Op::Create { label, .. } => assert_eq!(label, "fresh", "{op:?}"),
            other => panic!("only kept, untouched and the new task are expected: {other:?}"),
        }
    }
    assert_eq!(
        kept(&diff, &tree),
        [
            "schema: not removed: done",
            "schema: not renamed to 'renamed': done",
            "schema: kind not changed to spike: done",
            "schema: edges not changed: done",
            "schema: result not changed: done",
            "schema: todos not changed: done",
            "old: edges not changed: cancelled",
            "shipped-one: result not changed: done",
            "shipped-one: todos not changed: done",
            "shipped: 'extra' not created: 'shipped' is done",
        ]
    );
    assert_eq!(untouched(&diff, &tree), ["billing", "spec"]);
}

// Moves.

fn move_tree() -> WorkTree {
    tree(group(
        "billing",
        "proration",
        vec![
            group(
                "backend",
                "backend",
                vec![
                    leaf("engine", "engine", Kind::PrShip),
                    after(
                        leaf("rules", "rules", Kind::PrShip),
                        "engine",
                        UnblockPoint::Merged,
                    ),
                ],
            ),
            group("front", "front", vec![leaf("page", "page", Kind::PrShip)]),
            leaf("api", "api", Kind::PrShip),
        ],
    ))
}

#[test]
fn a_task_moves_under_another_parent() {
    let tree = move_tree();
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task backend: backend | id id-backend\n    \
         task engine: engine | id id-engine\n    task api: api | id id-api\n      after engine\n  \
         task front: front | id id-front\n    task page: page | id id-page\n  task rest: rest \
         endpoints\n    task rules: rules | id id-rules\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(kept(&diff, &tree), Vec::<String>::new());
    assert_eq!(
        changes(&diff, &tree),
        [
            "move api under backend",
            "add api After engine",
            "create rest",
            "move rules under rest",
        ]
    );
    let unlinked = diff.ops.iter().find_map(|op| match op {
        Op::Move { id, unlinked, .. } if *id == id_of("rules") => Some(unlinked.clone()),
        _ => None,
    });
    assert_eq!(unlinked, Some(Vec::new()), "rules stores its own edge");
}

#[test]
fn a_move_inside_a_moved_subtree_is_refused() {
    let tree = move_tree();
    // backend moves under front; engine, inside backend on disk, moves to the top: refused.
    // page, from front, is listed inside the moving backend: refused too.
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task front: front | id id-front\n    task \
         backend: backend | id id-backend\n      task rules: rules | id id-rules\n      task \
         page: page | id id-page\n  task engine: engine | id id-engine\n  task api: api | id \
         id-api\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(changes(&diff, &tree), ["move backend under front"]);
    assert_eq!(
        kept(&diff, &tree),
        [
            // engine was to leave rules' sibling set; it stays, and so does rules' edge to it.
            "rules: edge 'rules after engine' not removed: 'engine' is not moved",
            "page: not moved under 'backend': 'backend' also moves; apply, then propose again",
            "engine: not moved under 'billing': 'backend' also moves; apply, then propose again",
        ]
    );
}

#[test]
fn a_move_is_refused_for_live_tasks_and_linked_done_siblings() {
    let moving = move_tree();
    let mut live = LiveSet::default();
    live.add_chat(id_of("api"));
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task front: front | id id-front\n    task \
         page: page | id id-page\n    task api: api | id id-api\n",
    );
    let diff = run_with(&moving, &text, &live, &Choices::default());
    assert_eq!(
        kept(&diff, &moving),
        ["api: not moved under 'front': has a running chat"]
    );
    assert_eq!(changes(&diff, &moving), Vec::<String>::new());

    // engine is the target of rules' edge; with rules done, moving engine would edit rules.
    let tree = tree(group(
        "billing",
        "proration",
        vec![
            group(
                "backend",
                "backend",
                vec![
                    leaf("engine", "engine", Kind::PrShip),
                    finished(after(
                        leaf("rules", "rules", Kind::PrShip),
                        "engine",
                        UnblockPoint::Merged,
                    )),
                ],
            ),
            group("front", "front", vec![leaf("page", "page", Kind::PrShip)]),
        ],
    ));
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task front: front | id id-front\n    task \
         page: page | id id-page\n    task engine: engine | id id-engine\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(
        kept(&diff, &tree),
        ["engine: not moved under 'front': 'rules' has an edge to it and is done"]
    );
}

// Splits (D8).

fn keeper_tree() -> WorkTree {
    let backfill = with_file(
        leaf_with(
            "backfill",
            "backfill usage",
            Kind::PrShip,
            vec![
                todo("implement", Some(TodoRole::ProducesBranch), TodoState::Done),
                todo("review", None, TodoState::Open),
                todo("ship", Some(TodoRole::Ships), TodoState::Open),
            ],
        ),
        |file| {
            file.branch = Some("backfill-usage".to_string());
            file.worktree = Some("/w/worktrees/backfill".to_string());
            file.planned_result = Some("usage backfilled".to_string());
        },
    );
    let mut backfill = backfill;
    backfill.files.insert(
        PathBuf::from("chats/c1/record.toml"),
        std::time::SystemTime::UNIX_EPOCH,
    );
    backfill.files.insert(
        PathBuf::from("chats/c2/record.toml"),
        std::time::SystemTime::UNIX_EPOCH,
    );
    tree(group("billing", "proration", vec![backfill]))
}

fn split_text(notes: &str, core: &str) -> String {
    doc(
        "billing",
        &format!(
            "task billing: proration | id id-billing\n  task backfill: backfill usage | id \
             id-backfill\n    task notes: write the notes{notes} | kind document\n      result \
             notes\n    task core: backfill core{core}\n      result the backfill\n      todo \
             implement | role produces-branch\n      todo review\n      todo ship | role ships\n"
        ),
    )
}

fn split_of(diff: &Diff) -> Option<(String, KeeperSource)> {
    diff.ops.iter().find_map(|op| match op {
        Op::Split {
            keeper: Some(keeper),
            ..
        } => Some((keeper.label.clone(), keeper.source)),
        _ => None,
    })
}

#[test]
fn the_keeper_is_marked_else_chosen_else_the_child_with_the_branch_todo() {
    let tree = keeper_tree();
    let diff = run(&tree, &split_text("", ""));
    assert_eq!(
        split_of(&diff),
        Some(("core".to_string(), KeeperSource::Default))
    );

    let diff = run(&tree, &split_text(" | keeps work", ""));
    assert_eq!(
        split_of(&diff),
        Some(("notes".to_string(), KeeperSource::Marked))
    );

    let mut choices = Choices::default();
    choices
        .keepers
        .insert(id_of("backfill"), "notes".to_string());
    let diff = run_with(&tree, &split_text("", ""), &LiveSet::default(), &choices);
    assert_eq!(
        split_of(&diff),
        Some(("notes".to_string(), KeeperSource::Chosen))
    );
    // A mark wins over a choice.
    let diff = run_with(
        &tree,
        &split_text("", " | keeps work"),
        &LiveSet::default(),
        &choices,
    );
    assert_eq!(
        split_of(&diff),
        Some(("core".to_string(), KeeperSource::Marked))
    );
}

#[test]
fn without_a_branch_todo_match_the_first_child_keeps_the_work() {
    let tree = keeper_tree();
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task backfill: backfill usage | id \
         id-backfill\n    task notes: notes | kind document\n      result notes\n    task core: \
         core\n      result the backfill\n      todo build | role produces-branch\n      todo \
         ship | role ships\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(
        split_of(&diff),
        Some(("notes".to_string(), KeeperSource::Default))
    );
}

#[test]
fn the_keeper_takes_the_work_and_frozen_todos_stay() {
    let tree = keeper_tree();
    let diff = run(&tree, &split_text("", ""));
    let core = diff.ops.iter().find_map(|op| match op {
        Op::Create { label, task, .. } if label == "core" => Some(task.clone()),
        _ => None,
    });
    let core = core.expect("the keeper is created");
    let backfill = tree.task_by_id(&id_of("backfill")).expect("backfill");
    let disk = &backfill.file.as_ref().expect("a file").todos;
    assert_eq!(core.todos, *disk, "the same todos, the done one unchanged");
    assert_eq!(core.kind, Kind::PrShip);
    assert_eq!(core.planned_result.as_deref(), Some("the backfill"));
    // notes is fresh: it gains `after core` by default.
    assert!(
        changes(&diff, &tree).contains(&"add notes After core (default)".to_string()),
        "{:?}",
        changes(&diff, &tree)
    );
    let work = diff.ops.iter().find_map(|op| match op {
        Op::Split { work, fresh, .. } => Some((work.clone(), fresh.clone())),
        _ => None,
    });
    let (work, fresh) = work.expect("a split");
    assert_eq!(work.chats, 2);
    assert_eq!(work.branch.as_deref(), Some("backfill-usage"));
    assert_eq!(
        fresh.iter().map(TaskRef::name).collect::<Vec<_>>(),
        ["notes"]
    );
}

#[test]
fn the_default_after_keeper_needs_no_edge_and_no_cycle() {
    let tree = keeper_tree();
    // notes already names the keeper: no default.
    let text = split_text("", "").replace(
        "    task notes: write the notes | kind document\n",
        "    task notes: write the notes | kind document\n      after core | unblock review\n",
    );
    let diff = run(&tree, &text);
    let edges: Vec<String> = changes(&diff, &tree)
        .into_iter()
        .filter(|change| change.starts_with("add "))
        .collect();
    assert_eq!(edges, ["add notes After core"]);

    // core after notes: notes after core would close a cycle, so no default.
    let text = split_text("", "").replace(
        "      result the backfill\n",
        "      after notes\n      result the backfill\n",
    );
    let diff = run(&tree, &text);
    let edges: Vec<String> = changes(&diff, &tree)
        .into_iter()
        .filter(|change| change.starts_with("add "))
        .collect();
    assert_eq!(edges, ["add core After notes"]);
}

#[test]
fn a_split_is_refused_in_a_stack_and_with_a_live_chat_or_run() {
    // In a stack on disk: the proposal leaves out backfill's `stacked on base`, which validation
    // accepts, but the edge stays (backfill is live), so backfill cannot become a group.
    let mut top = keeper_tree().projects[0].task.clone();
    top.children.insert(0, leaf("base", "base", Kind::PrShip));
    if let Some(backfill) = top.children.iter_mut().find(|task| task.slug == "backfill") {
        *backfill = stacked_on(backfill.clone(), "base", UnblockPoint::Review);
    }
    let stacked = tree(top);
    let diff = run(&stacked, &split_text("", ""));
    assert_eq!(split_of(&diff), None);
    let refusal = "backfill is stacked on base, and a group cannot be stacked; remove that edge \
                   first";
    assert_eq!(
        kept(&diff, &stacked),
        [
            format!("backfill: not split: {refusal}"),
            "backfill: edge 'backfill stacked on base' not removed: has a worktree, has a branch"
                .to_string(),
            format!("backfill: 'notes' not created: {refusal}"),
            format!("backfill: 'core' not created: {refusal}"),
        ]
    );
    assert_eq!(creates(&diff), Vec::<String>::new());
    assert_eq!(changes(&diff, &stacked), Vec::<String>::new());

    // With a running chat, then a running run.
    let tree = keeper_tree();
    for (live, reason) in [
        (
            {
                let mut live = LiveSet::default();
                live.add_chat(id_of("backfill"));
                live
            },
            "has a running chat; close it first",
        ),
        (
            {
                let mut live = LiveSet::default();
                live.add_run(id_of("backfill"));
                live
            },
            "has a running run; close it first",
        ),
    ] {
        let diff = run_with(&tree, &split_text("", ""), &live, &Choices::default());
        assert_eq!(split_of(&diff), None);
        assert_eq!(
            kept(&diff, &tree).first().map(String::as_str),
            Some(format!("backfill: not split: {reason}").as_str())
        );
        assert_eq!(creates(&diff), Vec::<String>::new());
    }

    // A running chat on the current todo is work even without a branch: refused too.
    let plain = crate::test_support::tree(group(
        "billing",
        "proration",
        vec![leaf("backfill", "backfill usage", Kind::PrShip)],
    ));
    let mut live = LiveSet::default();
    live.add_chat(id_of("backfill"));
    let diff = run_with(&plain, &split_text("", ""), &live, &Choices::default());
    assert_eq!(
        kept(&diff, &plain).first().map(String::as_str),
        Some("backfill: not split: has a running chat; close it first")
    );
}

#[test]
fn a_leaf_without_work_simply_gains_children() {
    let tree = tree(group(
        "billing",
        "proration",
        vec![leaf("backfill", "backfill usage", Kind::PrShip)],
    ));
    let diff = run(&tree, &split_text("", ""));
    let split = diff.ops.iter().find_map(|op| match op {
        Op::Split {
            keeper,
            fresh,
            work,
            ..
        } => Some((keeper.clone(), fresh.len(), work.todos.len())),
        _ => None,
    });
    assert_eq!(split, Some((None, 2, 3)));
    assert_eq!(
        creates(&diff),
        ["notes under backfill", "core under backfill"]
    );
    assert!(
        !changes(&diff, &tree)
            .iter()
            .any(|change| change.contains("(default)")),
        "no keeper, no default edge"
    );

    // A planning chat on the plan todo does not stop it: nothing of the chat moves.
    let planned = crate::test_support::tree(group(
        "billing",
        "proration",
        vec![leaf_with(
            "backfill",
            "backfill usage",
            Kind::PrShip,
            vec![plan_todo()],
        )],
    ));
    let mut live = LiveSet::default();
    live.add_chat(id_of("backfill"));
    let diff = run_with(&planned, &split_text("", ""), &live, &Choices::default());
    assert_eq!(kept(&diff, &planned), Vec::<String>::new());
    assert!(matches!(
        diff.ops.iter().find(|op| matches!(op, Op::Split { .. })),
        Some(Op::Split { keeper: None, .. })
    ));
}

// Error badges.

#[test]
fn a_task_with_an_error_badge_refuses_the_whole_apply() {
    let mut tree = scenario_4_tree();
    let api = tree.projects[0]
        .task
        .children
        .iter_mut()
        .find(|task| task.slug == "api")
        .expect("api");
    api.errors.push(FileError {
        path: PathBuf::from("/w/projects/billing/tasks/api/task.toml"),
        line: Some(3),
        reason: "expected a string".to_string(),
    });
    let live = LiveSet::default();
    let printed = print(&tree, &id_of("billing"), &live).expect("prints");
    // Untouched, the badge does not matter.
    let diff = run(&tree, &printed);
    assert_eq!(diff.refusal, None);
    assert!(diff.is_empty());

    let text = printed.replace("task api: proration endpoint", "task api: the endpoint");
    let diff = run(&tree, &text);
    let refusal = diff.refusal.clone().expect("refused");
    assert!(
        refusal.contains("/w/projects/billing/tasks/api/task.toml:3: expected a string"),
        "{refusal}"
    );
    assert!(!diff.is_empty());
}

// Frozen todos across many random proposals.

/// A small deterministic generator, so the property test is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[test]
fn frozen_todos_are_never_changed_by_any_proposal() {
    const NAMES: [&str; 7] = [
        "implement",
        "review",
        "ship",
        "test",
        "docs",
        "fix ci",
        "plan",
    ];
    let mut rng = Rng(0x5eed_0026_0004);
    for round in 0..500 {
        let count = rng.below(6);
        let disk: Vec<Todo> = (0..count)
            .map(|_| {
                let name = NAMES[rng.below(NAMES.len())];
                let state = match rng.below(5) {
                    0 => TodoState::Done,
                    1 => TodoState::Skipped,
                    2 => TodoState::Cancelled,
                    _ => TodoState::Open,
                };
                let todo = todo(name, None, state);
                if rng.below(4) == 0 {
                    started(todo)
                } else {
                    todo
                }
            })
            .collect();
        let mut live = LiveSet::default();
        let running = rng.below(3) == 0;
        if running {
            live.add_chat(id_of("work"));
        }
        let tree = tree(group(
            "billing",
            "proration",
            vec![leaf_with("work", "the work", Kind::Other, disk.clone())],
        ));
        let current = disk.iter().position(|todo| todo.state == TodoState::Open);
        let frozen: Vec<usize> = (0..disk.len())
            .filter(|&index| {
                disk[index].state != TodoState::Open
                    || disk[index].started.is_some()
                    || (running && current == Some(index))
            })
            .collect();

        let lines: Vec<String> = (0..1 + rng.below(6))
            .map(|_| {
                let mut line = format!("    todo {}", NAMES[rng.below(NAMES.len())]);
                match rng.below(6) {
                    0 => line.push_str(" | done"),
                    1 => line.push_str(" | started"),
                    2 => line.push_str(" | workflow fix"),
                    _ => {}
                }
                line
            })
            .collect();
        let text = doc(
            "billing",
            &format!(
                "task billing: proration | id id-billing\n  task work: the work | id id-work\n    \
                 result r\n{}\n",
                lines.join("\n")
            ),
        );
        let parsed = checked(&tree, &text);
        let diff = diff(&parsed, &tree, &live, &Choices::default());
        let context = format!(
            "round {round}\n{text}\ndisk {disk:?}\nlive {running}\n{:?}",
            diff.ops
        );

        let Some(op) = diff.ops.iter().find(|op| matches!(op, Op::Todos { .. })) else {
            continue;
        };
        let Op::Todos { ops, todos, .. } = op else {
            continue;
        };
        // Every frozen todo is in the new list exactly as on disk, in its order.
        let mut at = 0;
        let mut last_frozen = None;
        for &index in &frozen {
            let found = todos[at..]
                .iter()
                .position(|todo| *todo == disk[index])
                .unwrap_or_else(|| panic!("frozen todo {index} lost or changed\n{context}"));
            last_frozen = Some(at + found);
            at += found + 1;
        }
        for todo_op in ops {
            match todo_op {
                TodoOp::Rename { index, .. } | TodoOp::Remove { index, .. } => {
                    assert!(
                        !frozen.contains(index),
                        "{todo_op:?} on a frozen todo\n{context}"
                    )
                }
                TodoOp::Move { from, .. } => {
                    assert!(
                        !frozen.contains(from),
                        "{todo_op:?} on a frozen todo\n{context}"
                    )
                }
                TodoOp::Add { index, .. } => {
                    if let Some(last) = last_frozen {
                        assert!(*index > last, "{todo_op:?} before a frozen todo\n{context}");
                    }
                }
                TodoOp::SetRole { .. } | TodoOp::SetWorkflow { .. } => {}
            }
        }
        // Nothing but the open, unfrozen todos and the additions changed.
        let open: Vec<&Todo> = (0..disk.len())
            .filter(|index| !frozen.contains(index))
            .map(|index| &disk[index])
            .collect();
        assert_eq!(
            todos.len(),
            frozen.len()
                + open.len()
                + ops
                    .iter()
                    .filter(|op| matches!(op, TodoOp::Add { .. }))
                    .count()
                - ops
                    .iter()
                    .filter(|op| matches!(op, TodoOp::Remove { .. }))
                    .count(),
            "{context}"
        );
    }
}

#[test]
fn a_new_todo_before_a_frozen_one_goes_after_it() {
    let tree = tree(group(
        "billing",
        "proration",
        vec![leaf_with(
            "work",
            "the work",
            Kind::PrShip,
            vec![
                todo("implement", Some(TodoRole::ProducesBranch), TodoState::Done),
                todo("review", None, TodoState::Open),
                todo("ship", Some(TodoRole::Ships), TodoState::Open),
            ],
        )],
    ));
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task work: the work | id id-work\n    result \
         r\n    todo design\n    todo implement | role produces-branch | done\n    todo review\n    \
         todo ship | role ships\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(
        changes(&diff, &tree)
            .into_iter()
            .filter(|change| change.starts_with("todos"))
            .collect::<Vec<_>>(),
        ["todos work [Add { name: \"design\", index: 1, after_frozen: Some(\"implement\") }]"]
    );
    // Renaming or dropping a done todo is kept.
    let text = doc(
        "billing",
        "task billing: proration | id id-billing\n  task work: the work | id id-work\n    result \
         r\n    todo build | role produces-branch | done\n    todo review\n",
    );
    let diff = run(&tree, &text);
    assert_eq!(
        kept(&diff, &tree),
        ["work: todo 'implement' not renamed to 'build': done"]
    );
    assert_eq!(
        changes(&diff, &tree),
        [
            "result work Some(\"r\")",
            "todos work [Remove { index: 2, name: \"ship\" }]"
        ]
    );
}

// The fingerprint.

#[test]
fn the_fingerprint_is_stable_and_follows_the_operations_the_keeper_and_the_file() {
    let tree = keeper_tree();
    let text = split_text("", "");
    let none = Choices::default();
    let first = fingerprint(&run(&tree, &text), &none, "hash-1");
    assert_eq!(first, fingerprint(&run(&tree, &text), &none, "hash-1"));
    assert_eq!(first.len(), 64);
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    let mut notes = Choices::default();
    notes.keepers.insert(id_of("backfill"), "notes".to_string());
    let chosen = fingerprint(
        &run_with(&tree, &text, &LiveSet::default(), &notes),
        &notes,
        "hash-1",
    );
    assert_ne!(first, chosen, "the keeper choice changes the fingerprint");
    // Choosing the default keeper still changes it: the choice is part of the hash.
    let mut core = Choices::default();
    core.keepers.insert(id_of("backfill"), "core".to_string());
    let same_ops = run_with(&tree, &text, &LiveSet::default(), &core);
    assert_ne!(first, fingerprint(&same_ops, &core, "hash-1"));
    assert_ne!(first, fingerprint(&run(&tree, &text), &none, "hash-2"));

    // A tree change between preview and apply changes it.
    let mut live = LiveSet::default();
    live.add_chat(id_of("backfill"));
    assert_ne!(
        first,
        fingerprint(&run_with(&tree, &text, &live, &none), &none, "hash-1")
    );
}
