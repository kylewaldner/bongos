use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, TodoRole, TodoState, UnblockPoint};

use crate::diff::{Choices, diff};
use crate::preview::preview;
use crate::print::LiveSet;
use crate::test_support::*;
use crate::{parse, print};

fn show(tree: &WorkTree, text: &str, live: &LiveSet) -> String {
    let parsed = parse(text);
    assert_eq!(parsed.problems, vec![], "{text}");
    let diff = diff(&parsed, tree, live, &Choices::default());
    preview(&parsed, tree, &diff)
}

fn billing_tree() -> WorkTree {
    let schema = finished(planned(
        leaf("schema", "add proration columns", Kind::PrShip),
        "columns",
    ));
    let engine = planned(leaf("engine", "proration engine", Kind::PrShip), "engine");
    let integration = planned(
        leaf("integration", "use proration in invoices", Kind::PrShip),
        "wired",
    );
    let api = planned(leaf("api", "proration endpoint", Kind::PrShip), "endpoint");
    tree(group(
        "billing",
        "implement proration for billing",
        vec![schema, engine, integration, api],
    ))
}

#[test]
fn an_unchanged_proposal_says_nothing_to_change() {
    let tree = billing_tree();
    let live = LiveSet::default();
    let text = print(&tree, &id_of("billing"), &live).expect("prints");
    assert_eq!(
        show(&tree, &text, &live),
        "Proposal 1 for \"implement proration for billing\"\nNothing to change: the tree already \
         matches proposal 1\n"
    );
}

#[test]
fn the_preview_lists_every_section() {
    let tree = billing_tree();
    let mut live = LiveSet::default();
    live.add_chat(id_of("engine"));
    let printed = print(&tree, &id_of("billing"), &live).expect("prints");
    let text = printed
        .replace(
            "task engine: proration engine | id id-engine | kind pr-ship | state ready",
            "task engine: proration engine | id id-engine | kind pr-ship | state ready | drop",
        )
        .replace(
            "task api: proration endpoint | id id-api | kind pr-ship | state ready",
            "task api: proration endpoint | id id-api | kind pr-ship | state ready | drop",
        );
    let text = text.replacen(
        "  task integration: use proration in invoices | id id-integration | kind pr-ship | state \
         ready\n    result wired\n    todo implement | role produces-branch\n    todo review\n",
        "  task integration: use proration in invoices | id id-integration | kind pr-ship | state \
         ready\n    result wired\n    todo implement | role produces-branch\n    todo code review\n",
        1,
    );
    let text = format!(
        "{text}  task docs: write the docs | kind document\n    after integration\n    result the \
         docs\n"
    );
    let engine_line = text
        .lines()
        .position(|line| line.contains("task engine:"))
        .expect("engine")
        + 1;
    let shown = show(&tree, &text, &live);
    assert_eq!(
        shown,
        format!(
            "Proposal 1 for \"implement proration for billing\"\ncreate 1 task, 3 todos, 1 \
             edge; change 1 task, cancel 1; 1 kept\n\nCreate\n  docs: write the docs \
             (document, 3 todos; after integration)\nChange\n  integration: rename todo 2 \
             \"review\" -> \"code review\"\nCancel\n  api\nKept\n  engine: kept, not removed \
             (line {engine_line}): has a running chat\nSplit\nUntouched\n  billing, schema \
             (done)\n"
        )
    );
}

#[test]
fn a_planned_leaf_becoming_a_group_touches_nothing_existing() {
    let tree = tree(group(
        "work",
        "work",
        vec![leaf_with(
            "billing",
            "implement proration for billing",
            Kind::PrShip,
            vec![todo("plan", Some(TodoRole::Plans), TodoState::Open)],
        )],
    ));
    let text = "proposal 2\nfor id-billing\n\ntask billing: implement proration for billing | id \
                id-billing\n  task schema: add proration columns\n    result a PR\n  task api: \
                endpoint\n    after schema | unblock review\n    result a PR\n";
    let shown = show(&tree, text, &LiveSet::default());
    assert_eq!(
        shown,
        "Proposal 2 for \"implement proration for billing\"\ncreate 2 tasks, 6 todos, 1 edge; \
         nothing existing is touched\n\nCreate\n  schema: add proration columns (pr-ship, 3 \
         todos)\n  api: endpoint (pr-ship, 3 todos; after schema | unblock review)\nChange\n  \
         billing: todo \"plan\" done\nCancel\nKept\nSplit\n  billing becomes a group; its todos \
         plan go to its history\nUntouched\n"
    );
}

#[test]
fn a_split_names_the_keeper_and_what_moves() {
    let mut backfill = leaf_with(
        "backfill",
        "backfill usage",
        Kind::PrShip,
        vec![
            todo("implement", Some(TodoRole::ProducesBranch), TodoState::Done),
            todo("review", None, TodoState::Open),
            todo("ship", Some(TodoRole::Ships), TodoState::Open),
        ],
    );
    let file = file_mut(&mut backfill);
    file.branch = Some("backfill-usage".to_string());
    file.worktree = Some("/w/worktrees/backfill".to_string());
    for chat in ["chats/a/record.toml", "chats/b/record.toml"] {
        backfill
            .files
            .insert(chat.into(), std::time::SystemTime::UNIX_EPOCH);
    }
    let tree = tree(group("billing", "proration", vec![backfill]));
    let text = "proposal 3\nfor id-billing\n\ntask billing: proration | id id-billing\n  task \
                backfill: backfill usage | id id-backfill\n    task core: backfill core\n      \
                result the backfill\n      todo implement | role produces-branch | done\n      \
                todo review\n      todo test\n      todo ship | role ships\n    task notes: notes \
                | kind document\n      result notes\n";
    let shown = show(&tree, text, &LiveSet::default());
    let split = shown
        .split("\nSplit\n")
        .nth(1)
        .and_then(|rest| rest.split("\nUntouched\n").next())
        .expect("a split section");
    assert_eq!(
        split,
        "  backfill becomes a group; branch backfill-usage, worktree, 2 chats and todos \
         implement (done), review, ship move into \"core\" (keeps work, default)\n    core: add \
         todo \"test\""
    );
    assert!(
        shown.contains("  notes: notes (document, 3 todos; after core (default))\n"),
        "{shown}"
    );
    assert!(
        shown.contains("  core: backfill core (pr-ship, 4 todos, keeps work)\n"),
        "{shown}"
    );
}

#[test]
fn a_todo_placed_before_a_done_one_says_where_it_went() {
    let tree = tree(group(
        "billing",
        "proration",
        vec![
            after(
                leaf_with(
                    "work",
                    "the work",
                    Kind::PrShip,
                    vec![
                        todo("implement", Some(TodoRole::ProducesBranch), TodoState::Done),
                        todo("ship", Some(TodoRole::Ships), TodoState::Open),
                    ],
                ),
                "spec",
                UnblockPoint::Merged,
            ),
            leaf("spec", "spec", Kind::Document),
        ],
    ));
    let text = "proposal 1\nfor id-billing\n\ntask billing: proration | id id-billing\n  task \
                spec: spec | id id-spec\n    todo draft\n    todo review\n    todo publish\n  \
                task work: the work | id id-work\n    todo design\n    todo implement | role \
                produces-branch | done\n    todo ship | role ships\n";
    let shown = show(&tree, text, &LiveSet::default());
    assert!(
        shown.contains(
            "  work: add todo \"design\" after \"implement\", not before it: done and started \
             todos keep their place\n"
        ),
        "{shown}"
    );
    assert!(shown.contains("  work: remove after spec\n"), "{shown}");
}
