use std::collections::BTreeSet;

use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, TodoRole, TodoState, UnblockPoint};

use crate::test_support::*;
use crate::validate::{Checks, ProposalFile, validate};
use crate::{Parsed, Problem, Severity, parse};

/// A proposal for `billing`: four header lines (the first task line is line 4), then `body`, whose
/// first line is line 5.
fn doc(body: &str) -> String {
    format!("proposal 1\nfor id-billing\n\ntask billing: proration | id id-billing\n{body}")
}

fn parsed(text: &str) -> Parsed {
    let parsed = parse(text);
    assert_eq!(parsed.problems, vec![], "the fixture must parse cleanly");
    parsed
}

fn show(problems: Vec<Problem>) -> Vec<String> {
    problems
        .into_iter()
        .map(|problem| {
            let severity = match problem.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            format!("{} {severity}: {}", problem.line, problem.message)
        })
        .collect()
}

fn check_with(body: &str, tree: &WorkTree, checks: &Checks) -> Vec<String> {
    show(validate(&parsed(&doc(body)), tree, checks))
}

fn check(body: &str, tree: &WorkTree) -> Vec<String> {
    check_with(body, tree, &Checks::default())
}

fn empty_tree() -> WorkTree {
    tree(group("billing", "proration", Vec::new()))
}

fn tree_of(children: Vec<bongos_work_model::store::Task>) -> WorkTree {
    tree(group("billing", "proration", children))
}

/// A new leaf with default todos (so stackable) and a result.
fn new_leaf(label: &str, extra: &str) -> String {
    format!("  task {label}: do {label}\n{extra}    result a PR for {label}\n")
}

#[test]
fn a_clean_proposal_has_no_problems() {
    let body = format!(
        "{}{}{}",
        new_leaf("schema", ""),
        new_leaf("engine", "    stacked on schema\n"),
        new_leaf("api", "    after engine | unblock review\n"),
    );
    assert_eq!(check(&body, &empty_tree()), Vec::<String>::new());
}

#[test]
fn a_cycle_names_the_closing_edge_and_its_chain() {
    let body = format!(
        "{}{}{}",
        new_leaf("a", "    after b\n"),
        new_leaf("b", "    after c\n"),
        new_leaf("c", "    after a\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec!["12 error: cycle: 'c after a' closes a -> b -> c -> a"]
    );
}

#[test]
fn a_two_task_cycle_reads_target_first() {
    let body = format!(
        "{}{}",
        new_leaf("integration", "    after api\n"),
        new_leaf("api", "    after integration\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec!["9 error: cycle: 'api after integration' closes integration -> api -> integration"]
    );
}

#[test]
fn a_cycle_runs_through_edges_kept_on_disk() {
    // `engine` is untouched by the proposal and stays after `schema`; `schema` after `engine`
    // would close the loop.
    let tree = tree_of(vec![
        leaf("schema", "schema", Kind::PrShip),
        after(
            leaf("engine", "engine", Kind::PrShip),
            "schema",
            UnblockPoint::Merged,
        ),
    ]);
    let body = "  task schema: schema | id id-schema\n    after engine\n    result r\n";
    assert_eq!(
        check(body, &tree),
        vec!["6 error: cycle: 'schema after engine' closes engine -> schema -> engine"]
    );
}

#[test]
fn an_edge_removed_in_the_proposal_is_not_kept() {
    // `engine` is in the proposal without its edge, so the edge is going away.
    let tree = tree_of(vec![
        leaf("schema", "schema", Kind::PrShip),
        after(
            leaf("engine", "engine", Kind::PrShip),
            "schema",
            UnblockPoint::Merged,
        ),
    ]);
    let body = "  task schema: schema | id id-schema\n    after engine\n    result r\n  task engine: engine | id id-engine\n    result r\n";
    assert_eq!(check(body, &tree), Vec::<String>::new());
}

#[test]
fn a_path_target_is_cross_level() {
    let body = format!(
        "{}{}",
        "  task billing2: group\n    task schema: s\n      result r\n",
        new_leaf("api", "    after billing2/schema\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec!["9 error: cross-level: 'api after billing2/schema' joins siblings only"]
    );
}

#[test]
fn a_label_elsewhere_in_the_proposal_is_cross_level() {
    let body = format!(
        "{}{}",
        "  task group: group\n    task schema: s\n      result r\n",
        new_leaf("api", "    after schema\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec!["9 error: cross-level: 'api after schema' joins siblings only"]
    );
}

#[test]
fn a_label_that_names_nothing_is_unknown() {
    let body = new_leaf("api", "    after x\n");
    assert_eq!(
        check(&body, &empty_tree()),
        vec!["6 error: unknown task 'x' in 'api after x'"]
    );
}

#[test]
fn stacked_on_needs_both_tasks_stackable() {
    let body = format!(
        "{}{}",
        "  task spec: write the spec | kind document\n    result a doc\n",
        new_leaf("engine", "    stacked on spec\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec![
            "8 error: 'engine stacked on spec': stacked on needs both tasks stackable; spec \
             needs a produces-branch todo and a ships todo"
        ]
    );
}

#[test]
fn proposed_todos_decide_stackability() {
    // A new leaf with todos and no roles gets the roles of its names; one named otherwise has none.
    let body = "  task schema: s\n    result r\n    todo implement\n    todo review\n    todo ship\n  task engine: e\n    stacked on schema\n    result r\n    todo build\n    todo ship | role ships\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec![
            "11 error: 'engine stacked on schema': stacked on needs both tasks stackable; engine \
             needs a produces-branch todo and a ships todo"
        ]
    );
}

#[test]
fn a_task_has_one_stacked_on() {
    let body = format!(
        "{}{}{}",
        new_leaf("a", ""),
        new_leaf("b", ""),
        "  task c: do c\n    stacked on a\n    stacked on b\n    result r\n",
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec![
            "11 error: 'c stacked on b': c is already stacked on a; a task has at most one stacked on"
        ]
    );
}

#[test]
fn a_stack_stays_in_one_repository() {
    let mut schema = leaf("schema", "schema", Kind::PrShip);
    file_mut(&mut schema).repository = Some("alpha".to_string());
    let mut engine = leaf("engine", "engine", Kind::PrShip);
    file_mut(&mut engine).repository = Some("beta".to_string());
    let tree = tree_of(vec![schema, engine]);
    let body = "  task schema: schema | id id-schema\n    result r\n  task engine: engine | id id-engine\n    stacked on schema\n    result r\n";
    assert_eq!(
        check(body, &tree),
        vec![
            "8 error: 'engine stacked on schema': stacked on needs both tasks in one repository; \
             engine is in beta, schema in alpha"
        ]
    );
}

#[test]
fn a_group_cannot_be_in_a_stack() {
    let body = format!(
        "{}{}",
        "  task schema: s\n    task part: p\n      result r\n",
        new_leaf("engine", "    stacked on schema\n"),
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec![
            "9 error: 'engine stacked on schema': engine is stacked on schema, and a group \
             cannot be stacked; remove that edge first"
        ]
    );
    let body = format!(
        "{}{}",
        new_leaf("schema", ""),
        "  task engine: e\n    stacked on schema\n    task part: p\n      result r\n",
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec![
            "8 error: 'engine stacked on schema': engine is stacked on schema, and a group \
             cannot be stacked; remove that edge first"
        ]
    );
}

#[test]
fn a_leaf_stacked_on_on_disk_cannot_become_a_group() {
    let tree = tree_of(vec![
        leaf("schema", "schema", Kind::PrShip),
        stacked_on(
            leaf("engine", "engine", Kind::PrShip),
            "schema",
            UnblockPoint::Merged,
        ),
    ]);
    let body = "  task schema: schema | id id-schema\n    task part: p\n      result r\n";
    assert_eq!(
        check(body, &tree),
        vec![
            "5 error: 'engine stacked on schema': engine is stacked on schema, and a group \
             cannot be stacked; remove that edge first"
        ]
    );
}

#[test]
fn an_edge_to_itself_or_twice_is_refused() {
    let body = format!(
        "{}{}",
        new_leaf("a", ""),
        "  task b: do b\n    after b\n    after a\n    after a\n    result r\n",
    );
    assert_eq!(
        check(&body, &empty_tree()),
        vec![
            "8 error: 'b after b': b cannot have an edge to itself",
            "10 error: 'b after a': b already has an edge to a",
        ]
    );
}

#[test]
fn a_leaf_has_one_result() {
    let body = "  task a: do a\n    result one\n    result two\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec!["7 error: two results, split or merge"]
    );
}

#[test]
fn a_group_has_no_todos_or_result() {
    let body =
        "  task g: group\n    result r\n    todo implement\n    task a: do a\n      result r\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec![
            "6 error: a group has no todos or result; move them to a leaf",
            "7 error: a group has no todos or result; move them to a leaf",
        ]
    );
}

#[test]
fn a_role_is_held_by_one_todo() {
    let body = "  task a: do a\n    result r\n    todo implement | role produces-branch\n    todo build | role produces-branch\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec!["8 error: a already has a produces-branch todo (implement)"]
    );
}

#[test]
fn labels_and_ids_are_unique() {
    let tree = tree_of(vec![leaf("schema", "schema", Kind::PrShip)]);
    let body = format!(
        "{}{}{}",
        new_leaf("a", ""),
        new_leaf("a", ""),
        "  task schema: s | id id-schema\n    result r\n  task again: s | id id-schema\n    result r\n",
    );
    assert_eq!(
        check(&body, &tree),
        vec![
            "7 error: duplicate label 'a' among siblings: also on line 5",
            "11 error: duplicate id 'id-schema': also on line 9",
        ]
    );
}

#[test]
fn an_id_must_name_a_task_under_the_root() {
    let tree = tree_of(vec![leaf("schema", "schema", Kind::PrShip)]);
    let body = "  task schema: s | id id-schema\n    result r\n  task ghost: g | id id-ghost\n    result r\n";
    assert_eq!(
        check(body, &tree),
        vec!["7 error: unknown id 'id-ghost': no task under the root has it"]
    );
}

#[test]
fn the_root_is_the_file_task() {
    let text = "proposal 2\nfor id-billing\n\ntask billing: proration | id id-other\n  task a: a\n    result r\n";
    let checks = Checks {
        file: Some(ProposalFile {
            number: 1,
            task_id: "id-billing".to_string(),
        }),
        ..Checks::default()
    };
    assert_eq!(
        show(validate(&parsed(text), &empty_tree(), &checks)),
        vec![
            "1 error: proposal 2 does not match the file name: this file is 1.proposal.txt",
            "4 error: the root's id id-other is not the task this file is for (id-billing)",
        ]
    );
    let text =
        "proposal 1\nfor id-elsewhere\n\ntask billing: proration\n  task a: a\n    result r\n";
    assert_eq!(
        show(validate(&parsed(text), &empty_tree(), &checks)),
        vec![
            "2 error: 'for id-elsewhere' is not the task this file belongs to (id-billing)",
            "4 error: the root task needs '| id id-billing': it is the task this file is for",
        ]
    );
}

#[test]
fn the_root_id_must_equal_for_without_a_file() {
    let text = "proposal 1\nfor id-billing\n\ntask billing: proration | id id-other\n  task a: a\n    result r\n";
    assert_eq!(
        show(validate(&parsed(text), &empty_tree(), &Checks::default())),
        vec!["4 error: the root's id id-other is not the task this file is for (id-billing)"]
    );
}

fn split_tree() -> WorkTree {
    tree_of(vec![
        leaf("backfill", "backfill", Kind::PrShip),
        group("area", "area", vec![leaf("inner", "inner", Kind::PrShip)]),
    ])
}

#[test]
fn keeps_work_marks_one_child_of_a_split_leaf() {
    let ok = "  task backfill: b | id id-backfill\n    task core: c | keeps work\n      result r\n    task rest: r\n      result r\n";
    assert_eq!(check(ok, &split_tree()), Vec::<String>::new());

    let twice = "  task backfill: b | id id-backfill\n    task core: c | keeps work\n      result r\n    task rest: r | keeps work\n      result r\n";
    assert_eq!(
        check(twice, &split_tree()),
        vec![
            "8 error: a second 'keeps work' under 'backfill': one child keeps the work, marked on line 6"
        ]
    );

    let new_parent = "  task fresh: f\n    task core: c | keeps work\n      result r\n";
    assert_eq!(
        check(new_parent, &split_tree()),
        vec![
            "6 error: 'keeps work' is for a child of a leaf being split; 'fresh' is not an \
             existing leaf"
        ]
    );

    let group_parent =
        "  task area: a | id id-area\n    task core: c | keeps work\n      result r\n";
    assert_eq!(
        check(group_parent, &split_tree()),
        vec![
            "6 error: 'keeps work' is for a child of a leaf being split; 'area' is not an \
             existing leaf"
        ]
    );

    let on_leaf = "  task backfill: b | keeps work | id id-backfill\n    result r\n";
    assert_eq!(
        check(on_leaf, &split_tree()),
        vec![
            "5 error: 'keeps work' is for a child of a leaf being split; 'billing' is not an \
             existing leaf"
        ]
    );
}

#[test]
fn keeps_work_on_the_root_is_refused() {
    let text = "proposal 1\nfor id-billing\n\ntask billing: proration | id id-billing | keeps work\n  task a: a\n    result r\n";
    assert_eq!(
        show(validate(&parsed(text), &empty_tree(), &Checks::default())),
        vec![
            "4 error: 'keeps work' is for a child of a leaf being split, and the root has no parent"
        ]
    );
}

#[test]
fn a_workflow_is_a_warning_until_a_source_exists() {
    let body = "  task a: a\n    result r\n    todo review | workflow review-pr\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec!["7 warning: workflow 'review-pr' is not checked: no workflow engine yet"]
    );

    let known: BTreeSet<String> = ["ship-pr".to_string()].into();
    let checks = Checks {
        workflows: Some(known),
        ..Checks::default()
    };
    assert_eq!(
        check_with(body, &empty_tree(), &checks),
        vec!["7 error: unknown workflow 'review-pr': the known workflows are ship-pr"]
    );
    let body = "  task a: a\n    result r\n    todo ship | workflow ship-pr\n";
    assert_eq!(
        check_with(body, &empty_tree(), &checks),
        Vec::<String>::new()
    );
    let checks = Checks {
        workflows: Some(BTreeSet::new()),
        ..Checks::default()
    };
    assert_eq!(
        check_with(body, &empty_tree(), &checks),
        vec!["7 error: unknown workflow 'ship-pr': no workflows are registered"]
    );
}

#[test]
fn many_todos_warn_above_the_setting() {
    let body = "  task a: a\n    result r\n    todo one\n    todo two\n    todo three\n";
    assert_eq!(check(body, &empty_tree()), Vec::<String>::new());
    let checks = Checks {
        todo_warning: 2,
        ..Checks::default()
    };
    assert_eq!(
        check_with(body, &empty_tree(), &checks),
        vec!["5 warning: 3 todos; is this more than one result?"]
    );
}

#[test]
fn a_leaf_with_no_result_warns() {
    let body = "  task a: a\n  task b: b\n    result r\n";
    assert_eq!(
        check(body, &empty_tree()),
        vec!["5 warning: no result named"]
    );
}

#[test]
fn a_done_task_needs_no_result_and_keeps_its_edges() {
    // `schema` is done and stays after `engine` on disk; it prints as one line.
    let mut schema = after(
        leaf("schema", "schema", Kind::PrShip),
        "engine",
        UnblockPoint::Merged,
    );
    for todo in &mut file_mut(&mut schema).todos {
        todo.state = TodoState::Done;
    }
    let tree = tree_of(vec![schema, leaf("engine", "engine", Kind::PrShip)]);
    let body = "  task schema: schema | id id-schema | state done\n  task engine: engine | id id-engine\n    after schema\n    result r\n";
    assert_eq!(
        check(body, &tree),
        vec!["7 error: cycle: 'engine after schema' closes schema -> engine -> schema"]
    );
}

#[test]
fn roles_of_existing_todos_are_read_as_written() {
    let mut schema = leaf_with(
        "schema",
        "schema",
        Kind::PrShip,
        vec![
            todo("implement", Some(TodoRole::ProducesBranch), TodoState::Open),
            todo("ship", Some(TodoRole::Ships), TodoState::Open),
        ],
    );
    file_mut(&mut schema).repository = None;
    let tree = tree_of(vec![schema]);
    // No todo lines: the todos on disk stand, so the leaf is stackable.
    let body = "  task schema: schema | id id-schema\n    result r\n  task engine: e\n    stacked on schema\n    result r\n";
    assert_eq!(check(body, &tree), Vec::<String>::new());
}
