use std::path::PathBuf;

use bongos_work_model::derive::{self, State};
use bongos_work_model::graph::child_order;
use bongos_work_model::rules::EdgeKind;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Kind, Status, TodoRole, TodoState, UnblockPoint};

use crate::grammar::{Edge, Node, Todo, TodoMark, kind_name};
use crate::print::{LiveSet, print, state_word};
use crate::test_support::*;
use crate::{By, Located, parse};

fn nothing_live() -> LiveSet {
    LiveSet::default()
}

/// The node a printed task should parse back to, built from the model without the printer.
fn expected(task: &Task, tree: &WorkTree, live: &LiveSet, is_root: bool) -> Node {
    let file = task.file.as_ref().expect("a task with a file");
    let state = derive::state(task, tree);
    let mut node = Node::new(0, task.slug.clone(), file.goal.clone());
    node.attributes.id = Some(file.id.clone());
    node.attributes.kind = Some(file.kind);
    node.attributes.state = Some(state_word(state).to_string());
    let collapsed = !is_root
        && matches!(state, State::Done | State::Cancelled)
        && !task
            .walk()
            .iter()
            .any(|inner| inner.id().is_some_and(|id| live.running.contains(id)));
    if collapsed {
        return node;
    }
    let edge = |kind, target: &str, unblock, default| Edge {
        line: 0,
        kind,
        target: vec![target.to_string()],
        unblock: (unblock != default).then_some(unblock),
    };
    for after in &file.after {
        node.edges.push(edge(
            EdgeKind::After,
            &after.task,
            after.unblock,
            UnblockPoint::Merged,
        ));
    }
    if let Some(stacked) = &file.stacked_on {
        node.edges.push(edge(
            EdgeKind::StackedOn,
            &stacked.task,
            stacked.unblock,
            UnblockPoint::Review,
        ));
    }
    node.results = file
        .planned_result
        .iter()
        .map(|result| Located {
            value: result.clone(),
            line: 0,
        })
        .collect();
    node.todos = file
        .todos
        .iter()
        .map(|todo| Todo {
            line: 0,
            name: todo.name.clone(),
            role: todo.role,
            workflow: todo.workflow.clone(),
            mark: match todo.state {
                TodoState::Open => None,
                TodoState::Done => Some(TodoMark::Done),
                TodoState::Skipped => Some(TodoMark::Skipped),
                TodoState::Cancelled => Some(TodoMark::Cancelled),
            },
            started: todo.started.is_some(),
        })
        .collect();
    node.children = child_order(task)
        .into_iter()
        .map(|child| expected(child, tree, live, false))
        .collect();
    node
}

fn clear_lines(node: &mut Node) {
    node.line = 0;
    for result in &mut node.results {
        result.line = 0;
    }
    for todo in &mut node.todos {
        todo.line = 0;
    }
    for edge in &mut node.edges {
        edge.line = 0;
    }
    for child in &mut node.children {
        clear_lines(child);
    }
}

/// Prints `root_slug`'s subtree and checks that parsing it gives back exactly that subtree.
fn round_trip(tree: &WorkTree, root_slug: &str, live: &LiveSet) -> String {
    let root = tree
        .tasks()
        .into_iter()
        .find(|task| task.slug == root_slug)
        .expect("the root is in the tree");
    let text = print(tree, &id_of(root_slug), live).expect("the root prints");
    let mut parsed = parse(&text);
    assert_eq!(parsed.problems, vec![], "{text}");
    assert_eq!(
        parsed
            .header
            .for_task
            .as_ref()
            .map(|task| task.value.clone()),
        Some(id_of(root_slug))
    );
    assert_eq!(
        parsed.header.by.as_ref().map(|by| by.value.clone()),
        Some(By::Kyle)
    );
    assert_eq!(
        parsed.header.why.as_ref().map(|why| why.value.as_str()),
        Some("")
    );
    clear_lines(&mut parsed.root);
    assert_eq!(parsed.root, expected(root, tree, live, true), "{text}");
    text
}

fn billing() -> WorkTree {
    tree(group(
        "billing",
        "implement proration for billing",
        vec![
            planned(
                leaf("schema", "add proration columns to plans", Kind::PrShip),
                "a PR adding proration columns",
            ),
            stacked_on(
                planned(
                    leaf("engine", "proration engine", Kind::PrShip),
                    "a PR with the proration engine",
                ),
                "schema",
                UnblockPoint::Review,
            ),
            after(
                planned(
                    leaf("integration", "use proration in invoices", Kind::PrMerge),
                    "a PR wiring it in",
                ),
                "engine",
                UnblockPoint::Merged,
            ),
            after(
                leaf("api", "proration endpoint", Kind::PrShip),
                "integration",
                UnblockPoint::Implemented,
            ),
            after(
                after(
                    stacked_on(
                        leaf("deploy", "deploy proration", Kind::Other),
                        "api",
                        UnblockPoint::Merged,
                    ),
                    "schema",
                    UnblockPoint::Merged,
                ),
                "engine",
                UnblockPoint::Review,
            ),
        ],
    ))
}

#[test]
fn header_and_group_print_in_child_order() {
    let tree = billing();
    let text = round_trip(&tree, "billing", &nothing_live());
    assert_eq!(
        text,
        "\
# bongos task proposal. Lines starting with # are comments. The format is in ~/.bongos/task-builder/format.txt
proposal 1
for id-billing
by kyle
why

task billing: implement proration for billing | id id-billing | kind other | state ready
  task schema: add proration columns to plans | id id-schema | kind pr-ship | state ready
    result a PR adding proration columns
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task engine: proration engine | id id-engine | kind pr-ship | state blocked
    stacked on schema
    result a PR with the proration engine
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task integration: use proration in invoices | id id-integration | kind pr-merge | state blocked
    after engine
    result a PR wiring it in
    todo implement | role produces-branch
    todo review
    todo merge | role ships
  task api: proration endpoint | id id-api | kind pr-ship | state blocked
    after integration | unblock implemented
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task deploy: deploy proration | id id-deploy | kind other | state blocked
    after schema
    after engine | unblock review
    stacked on api | unblock merged
    todo do it
"
    );
}

#[test]
fn a_leaf_root_round_trips() {
    let tree = billing();
    let text = round_trip(&tree, "schema", &nothing_live());
    assert!(text.contains("for id-schema\n"), "{text}");
    assert!(text.contains("\ntask schema: "), "{text}");
}

#[test]
fn every_kind_round_trips() {
    let kinds = [
        Kind::PrShip,
        Kind::PrMerge,
        Kind::Document,
        Kind::Spike,
        Kind::Babysit,
        Kind::Other,
    ];
    let children = kinds
        .iter()
        .map(|kind| leaf(kind_name(*kind), "a goal", *kind))
        .collect();
    let tree = tree(group("kinds", "every kind", children));
    let text = round_trip(&tree, "kinds", &nothing_live());
    for kind in kinds {
        assert!(
            text.contains(&format!("| kind {} |", kind_name(kind))),
            "{kind:?}: {text}"
        );
    }
}

#[test]
fn roles_workflows_and_marks_round_trip() {
    let mut planning = todo("plan", Some(TodoRole::Plans), TodoState::Done);
    planning.workflow = Some("plan".to_string());
    planning.started = Some(timestamp("2026-11-08T10:05:00Z"));
    let mut implementing = todo(
        "write the code",
        Some(TodoRole::ProducesBranch),
        TodoState::Open,
    );
    implementing.started = Some(timestamp("2026-11-08T10:06:00Z"));
    let mut reviewing = todo("review", None, TodoState::Open);
    reviewing.workflow = Some("review pr".to_string());
    let tree = tree(group(
        "roles",
        "roles",
        vec![leaf_with(
            "work",
            "work",
            Kind::PrShip,
            vec![
                planning,
                implementing,
                reviewing,
                todo("lint", None, TodoState::Skipped),
                todo("drop it", None, TodoState::Cancelled),
                todo("ship", Some(TodoRole::Ships), TodoState::Open),
            ],
        )],
    ));
    let text = round_trip(&tree, "roles", &nothing_live());
    assert!(
        text.contains("    todo plan | role plans | workflow plan | done | started\n"),
        "{text}"
    );
    assert!(
        text.contains("    todo write the code | role produces-branch | started\n"),
        "{text}"
    );
    assert!(
        text.contains("    todo review | workflow review pr\n"),
        "{text}"
    );
    assert!(text.contains("    todo lint | skipped\n"), "{text}");
    assert!(text.contains("    todo drop it | cancelled\n"), "{text}");
}

#[test]
fn done_and_cancelled_subtrees_print_as_one_line() {
    let mut cancelled = leaf("dropped", "an abandoned idea", Kind::Spike);
    file_mut(&mut cancelled).status = Status::Cancelled;
    let tree = tree(group(
        "project",
        "the project",
        vec![
            group(
                "shipped",
                "the shipped part",
                vec![
                    finished(leaf("one", "first", Kind::PrShip)),
                    after(
                        finished(leaf("two", "second", Kind::PrShip)),
                        "one",
                        UnblockPoint::Merged,
                    ),
                ],
            ),
            finished(planned(leaf("single", "a done leaf", Kind::PrShip), "a PR")),
            cancelled,
            leaf("open", "still open", Kind::PrShip),
        ],
    ));
    let text = round_trip(&tree, "project", &nothing_live());
    assert!(
        text.contains(
            "\n  task shipped: the shipped part | id id-shipped | kind other | state done\n  task"
        ),
        "{text}"
    );
    assert!(
        text.ends_with("\n  task single: a done leaf | id id-single | kind pr-ship | state done\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "  task dropped: an abandoned idea | id id-dropped | kind spike | state cancelled\n"
        ),
        "{text}"
    );
    assert!(!text.contains("id-one"), "{text}");
    assert!(!text.contains("a PR\n"), "{text}");
    assert!(text.contains("  task open: still open"), "{text}");
    assert!(
        text.contains("    todo implement | role produces-branch\n"),
        "{text}"
    );
}

#[test]
fn a_done_root_still_prints_in_full() {
    let tree = tree(group(
        "root",
        "root",
        vec![finished(leaf("only", "only", Kind::PrShip))],
    ));
    let text = round_trip(&tree, "root", &nothing_live());
    assert!(
        text.contains("task root: root | id id-root | kind other | state done\n"),
        "{text}"
    );
    assert!(
        text.contains("  task only: only | id id-only | kind pr-ship | state done\n"),
        "{text}"
    );
    assert!(!text.contains("todo"), "{text}");
}

#[test]
fn a_done_subtree_with_running_work_prints_in_full() {
    let tree = tree(group(
        "project",
        "the project",
        vec![group(
            "shipped",
            "the shipped part",
            vec![finished(leaf("one", "first", Kind::PrShip))],
        )],
    ));
    let mut live = LiveSet::default();
    live.running.insert(id_of("one"));
    let text = round_trip(&tree, "project", &live);
    assert!(text.contains("    task one: first"), "{text}");
    assert!(
        text.contains("      todo ship | role ships | done\n"),
        "{text}"
    );
}

#[test]
fn the_number_follows_the_highest_proposal() {
    let mut tree = billing();
    let root = &mut tree.projects[0].task;
    root.files.insert(
        PathBuf::from("proposals/3.proposal.txt"),
        std::time::SystemTime::UNIX_EPOCH,
    );
    root.files.insert(
        PathBuf::from("proposals/notes.txt"),
        std::time::SystemTime::UNIX_EPOCH,
    );
    root.files.insert(
        PathBuf::from("5.proposal.txt"),
        std::time::SystemTime::UNIX_EPOCH,
    );
    let text = print(&tree, &id_of("billing"), &nothing_live()).expect("prints");
    assert!(text.contains("\nproposal 4\n"), "{text}");

    use bongos_work_model::{ProposalRecord, ProposalState};
    file_mut(&mut tree.projects[0].task)
        .proposals
        .push(ProposalRecord {
            number: 9,
            state: ProposalState::Applied,
            at: timestamp("2026-11-08T10:00:00Z"),
            by: "kyle".to_string(),
            hash: String::new(),
            extra: Default::default(),
        });
    let text = print(&tree, &id_of("billing"), &nothing_live()).expect("prints");
    assert!(text.contains("\nproposal 10\n"), "{text}");
}

#[test]
fn an_unknown_root_prints_nothing() {
    assert_eq!(print(&billing(), "no-such-id", &nothing_live()), None);
}
