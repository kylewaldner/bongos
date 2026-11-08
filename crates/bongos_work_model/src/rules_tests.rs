//! Each refusal of the tree rules, with the exact message the UI shows.

use crate::commands::*;
use crate::fixtures::*;
use crate::rules::EdgeKind;
use crate::store::WorkTree;
use crate::*;

fn refusal(outcome: Outcome) -> String {
    outcome.expect_err("the command should be refused")
}

fn goal(text: &str) -> NewTask {
    NewTask {
        goal: text.to_string(),
        ..NewTask::default()
    }
}

fn pr(url: &str, state: ResultState) -> TaskResult {
    TaskResult {
        kind: ResultKind::Pr,
        url: Some(url.to_string()),
        state: Some(state),
        urls: Vec::new(),
        extra: Extra::new(),
    }
}

/// Runs `command` on `tree` and applies its change, failing on a refusal.
fn accept(tree: &mut WorkTree, command: impl FnOnce(&WorkTree) -> Outcome) {
    let change = command(tree).expect("the command should be accepted");
    update(tree, &change);
}

#[test]
fn child_on_a_leaf_with_a_worktree_is_refused() {
    let mut a = pr_ship("a");
    file_mut(&mut a).worktree = Some("/src/invoicing/.bongos-worktrees/a".to_string());
    let t = tree(vec![a]);
    assert_eq!(
        refusal(new_child(&t, &ctx(), &id_of(&t, "a"), goal("Part"))),
        "a is a leaf with a worktree; use task: split"
    );

    let mut b = pr_ship("b");
    file_mut(&mut b).worktree = Some("/w/b".to_string());
    file_mut(&mut b).branch = Some("b".to_string());
    file_mut(&mut b).result = Some(pr("https://x/pull/1", ResultState::Open));
    let t = tree(vec![b]);
    assert_eq!(
        refusal(new_child(&t, &ctx(), &id_of(&t, "b"), goal("Part"))),
        "b is a leaf with a worktree, a branch and a result; use task: split"
    );
}

#[test]
fn child_on_a_leaf_in_a_stack_is_refused() {
    let t = tree(vec![
        pr_ship("a"),
        stacked_on(pr_ship("b"), "a", UnblockPoint::Review),
    ]);
    assert_eq!(
        refusal(new_child(&t, &ctx(), &id_of(&t, "b"), goal("Part"))),
        "b is stacked on a, and a group cannot be stacked; remove that edge first"
    );
    assert_eq!(
        refusal(new_child(&t, &ctx(), &id_of(&t, "a"), goal("Part"))),
        "b is stacked on a, and a group cannot be stacked; remove that edge first"
    );
}

#[test]
fn second_result_is_refused_and_the_same_result_updates() {
    let mut t = tree(vec![pr_ship("a")]);
    let a = id_of(&t, "a");
    accept(&mut t, |t| {
        set_result(t, &ctx(), &a, pr("https://x/pull/1", ResultState::Open))
    });
    assert_eq!(
        refusal(set_result(
            &t,
            &ctx(),
            &a,
            pr("https://x/pull/2", ResultState::Open)
        )),
        "a already has a result; clear it first"
    );
    // The same PR again moves its state.
    let change = set_result(&t, &ctx(), &a, pr("https://x/pull/1", ResultState::Merged)).unwrap();
    assert_eq!(
        change.file().unwrap().result.as_ref().unwrap().state,
        Some(ResultState::Merged)
    );
    // After a clear, a new result is accepted.
    accept(&mut t, |t| clear_result(t, &ctx(), &a));
    assert!(set_result(&t, &ctx(), &a, pr("https://x/pull/2", ResultState::Open)).is_ok());
}

#[test]
fn babysit_result_is_a_set_of_prs() {
    let mut t = tree(vec![task("watch", TaskFile::new("watch", Kind::Babysit))]);
    let id = id_of(&t, "watch");
    accept(&mut t, |t| {
        set_result(t, &ctx(), &id, pr("https://x/pull/1", ResultState::Open))
    });
    accept(&mut t, |t| {
        set_result(t, &ctx(), &id, pr("https://x/pull/2", ResultState::Open))
    });
    accept(&mut t, |t| {
        set_result(t, &ctx(), &id, pr("https://x/pull/1", ResultState::Open))
    });
    let result = find(&t, "watch")
        .file
        .as_ref()
        .unwrap()
        .result
        .clone()
        .unwrap();
    assert_eq!(result.kind, ResultKind::Prs);
    assert_eq!(result.urls, vec!["https://x/pull/1", "https://x/pull/2"]);
}

#[test]
fn cross_level_edge_is_refused() {
    let t = tree(vec![group("g", vec![pr_ship("c")]), pr_ship("b")]);
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "c"),
            &id_of(&t, "b"),
            EdgeKind::After,
            None
        )),
        "edges join siblings only: c and b have different parents"
    );
    // A child and its own parent are not siblings either.
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "c"),
            &id_of(&t, "g"),
            EdgeKind::After,
            None
        )),
        "edges join siblings only: c and g have different parents"
    );
}

#[test]
fn edge_to_itself_and_a_duplicate_edge_are_refused() {
    let t = tree(vec![
        pr_ship("a"),
        after(pr_ship("b"), "a", UnblockPoint::Merged),
    ]);
    let (a, b) = (id_of(&t, "a"), id_of(&t, "b"));
    assert_eq!(
        refusal(add_edge(&t, &ctx(), &a, &a, EdgeKind::After, None)),
        "a cannot have an edge to itself"
    );
    assert_eq!(
        refusal(add_edge(&t, &ctx(), &b, &a, EdgeKind::StackedOn, None)),
        "b already has an edge to a"
    );
}

#[test]
fn second_stacked_on_is_refused() {
    let t = tree(vec![
        pr_ship("a"),
        stacked_on(pr_ship("b"), "a", UnblockPoint::Review),
        pr_ship("c"),
    ]);
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "b"),
            &id_of(&t, "c"),
            EdgeKind::StackedOn,
            None
        )),
        "b is already stacked on a; a task has at most one stacked on"
    );
    // Any number of `after` edges is fine.
    assert!(
        add_edge(
            &t,
            &ctx(),
            &id_of(&t, "b"),
            &id_of(&t, "c"),
            EdgeKind::After,
            None
        )
        .is_ok()
    );
}

#[test]
fn cycle_is_refused_naming_the_chain() {
    let t = tree(vec![
        pr_ship("a"),
        after(pr_ship("b"), "c", UnblockPoint::Merged),
        stacked_on(pr_ship("c"), "a", UnblockPoint::Review),
    ]);
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "a"),
            &id_of(&t, "b"),
            EdgeKind::After,
            None
        )),
        "cycle: a -> b -> c -> a"
    );
}

#[test]
fn unstackable_stack_is_refused() {
    let t = tree(vec![pr_ship("a"), leaf("d", Status::Open)]);
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "a"),
            &id_of(&t, "d"),
            EdgeKind::StackedOn,
            None
        )),
        "stacked on needs both tasks stackable; d needs a produces-branch todo and a ships todo"
    );
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "d"),
            &id_of(&t, "a"),
            EdgeKind::StackedOn,
            None
        )),
        "stacked on needs both tasks stackable; d needs a produces-branch todo and a ships todo"
    );
    // Renamed todos keep their roles, so the task stays stackable.
    let mut renamed = pr_ship("r");
    file_mut(&mut renamed).todos[0].name = "build".to_string();
    file_mut(&mut renamed).todos[2].name = "deploy".to_string();
    let t = tree(vec![pr_ship("a"), renamed]);
    assert!(
        add_edge(
            &t,
            &ctx(),
            &id_of(&t, "r"),
            &id_of(&t, "a"),
            EdgeKind::StackedOn,
            None
        )
        .is_ok()
    );
}

#[test]
fn stack_across_repositories_is_refused() {
    let mut a = pr_ship("a");
    file_mut(&mut a).repository = Some("billing".to_string());
    let mut t = tree(vec![a, pr_ship("b")]);
    t.projects[0].file.as_mut().unwrap().repositories = vec!["invoicing".to_string()];
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "b"),
            &id_of(&t, "a"),
            EdgeKind::StackedOn,
            None
        )),
        "stacked on needs both tasks in one repository; b is in invoicing, a in billing"
    );
}

#[test]
fn ship_before_parent_done_is_refused_and_the_override_marks_it() {
    let mut a = done(pr_ship("a"), &[0]);
    file_mut(&mut a).todos[1].started = Some(at("2026-11-08T11:00:00Z"));
    let b = done(stacked_on(pr_ship("b"), "a", UnblockPoint::Review), &[0, 1]);
    let t = tree(vec![a, b]);
    let b = id_of(&t, "b");
    assert_eq!(
        refusal(advance(&t, &ctx(), &b, false)),
        "b cannot ship before a is Done (it is Review); use task: advance override"
    );

    let change = advance(&t, &ctx(), &b, true).unwrap();
    let file = change.file().unwrap();
    assert_eq!(file.todos[2].state, TodoState::Done);
    let marker = file.ship_overridden.as_ref().unwrap();
    assert_eq!(marker.parent_state, "review");
    assert_eq!(marker.at, ctx().at);
    assert_eq!(change.entry.event, "todo done, ship overridden");
    assert_eq!(change.entry.detail.as_deref(), Some("ship"));

    // Once the parent is Done the ships todo completes with no marker.
    let t = tree(vec![
        leaf("a", Status::Done),
        done(stacked_on(pr_ship("b"), "a", UnblockPoint::Review), &[0, 1]),
    ]);
    let change = advance(&t, &ctx(), &id_of(&t, "b"), false).unwrap();
    assert!(change.file().unwrap().ship_overridden.is_none());
    assert_eq!(change.entry.event, "todo done");
}

#[test]
fn parent_with_two_stacked_children_each_with_a_child_is_accepted() {
    let mut t = tree(vec![
        pr_ship("a"),
        pr_ship("b"),
        pr_ship("c"),
        pr_ship("d"),
        pr_ship("e"),
    ]);
    for (from, to) in [("b", "a"), ("c", "a"), ("d", "b"), ("e", "c")] {
        accept(&mut t, |t| {
            add_edge(
                t,
                &ctx(),
                &id_of(t, from),
                &id_of(t, to),
                EdgeKind::StackedOn,
                None,
            )
        });
    }
    for (from, to) in [("b", "a"), ("c", "a"), ("d", "b"), ("e", "c")] {
        let edge = find(&t, from).file.as_ref().unwrap().stacked_on.clone();
        assert_eq!(edge.unwrap().task, to);
    }
    // The tree still refuses to close a loop through it.
    assert_eq!(
        refusal(add_edge(
            &t,
            &ctx(),
            &id_of(&t, "a"),
            &id_of(&t, "e"),
            EdgeKind::After,
            None
        )),
        "cycle: a -> e -> c -> a"
    );
}

#[test]
fn drop_of_a_group_lists_its_open_descendants() {
    let t = tree(vec![group(
        "g",
        vec![
            pr_ship("x"),
            leaf("y", Status::Done),
            group("h", vec![pr_ship("z")]),
        ],
    )]);
    let g = id_of(&t, "g");
    assert_eq!(
        refusal(drop_task(&t, &ctx(), &g, false)),
        "dropping g also cancels its open tasks: x, h, z; confirm to drop them all"
    );
    let change = drop_task(&t, &ctx(), &g, true).unwrap();
    assert_eq!(change.writes.len(), 4);
    for slug in ["g", "x", "h", "z"] {
        let file = change.task_file(&find(&t, slug).dir).unwrap();
        assert_eq!(file.status, Status::Cancelled, "{slug}");
    }
    assert!(change.task_file(&find(&t, "y").dir).is_none());
}

#[test]
fn stacked_tasks_keep_their_roles() {
    let t = tree(vec![
        pr_ship("a"),
        stacked_on(pr_ship("b"), "a", UnblockPoint::Review),
    ]);
    let (a, b) = (id_of(&t, "a"), id_of(&t, "b"));
    assert_eq!(
        refusal(todo_remove(&t, &ctx(), &b, 2)),
        "b is stacked on a, so it must keep a produces-branch todo and a ships todo"
    );
    assert_eq!(
        refusal(todo_set_role(&t, &ctx(), &a, 0, None)),
        "b is stacked on a, so a must keep a produces-branch todo and a ships todo"
    );
    assert_eq!(
        refusal(todo_set_role(&t, &ctx(), &a, 1, Some(TodoRole::Ships))),
        "a already has a ships todo (ship)"
    );
    // Moving a role to another todo keeps the task stackable.
    let mut moved = tree(vec![pr_ship("a")]);
    let a = id_of(&moved, "a");
    accept(&mut moved, |moved| {
        todo_set_role(moved, &ctx(), &a, 2, None)
    });
    accept(&mut moved, |moved| {
        todo_set_role(moved, &ctx(), &a, 1, Some(TodoRole::Ships))
    });
}
