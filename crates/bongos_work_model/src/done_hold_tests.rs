//! A done hold keeps a finished task from Done in every derived state and every rule that tests
//! Done, and nothing about it is written to a file.

use std::collections::BTreeMap;

use crate::activity;
use crate::commands::*;
use crate::derive::{self, State};
use crate::fixtures::*;
use crate::rollup;
use crate::rules::{self, Place};
use crate::store::WorkTree;
use crate::*;

const HOLD: &str = "not done: 1 check has no pass on the current tree (tests)";

fn hold(tree: &mut WorkTree, slugs: &[&str]) {
    let holds: BTreeMap<String, String> = slugs
        .iter()
        .map(|slug| (id_of(tree, slug), HOLD.to_string()))
        .collect();
    assert!(tree.set_done_holds(&holds));
}

fn state_of(tree: &WorkTree, slug: &str) -> State {
    derive::state(find(tree, slug), tree)
}

#[test]
fn a_held_leaf_is_not_done_and_keeps_its_stage_before_done() {
    // An `other` leaf with every todo closed, and one closed outright.
    let mut finished = leaf("finished", Status::Open);
    file_mut(&mut finished).todos = vec![todo("do it", None)];
    let finished = done(finished, &[0]);
    let closed = leaf("closed", Status::Done);
    // A pr-ship leaf with every todo done.
    let shipped = done(pr_ship("shipped"), &[0, 1, 2]);
    let mut t = tree(vec![finished, closed, shipped]);
    for slug in ["finished", "closed", "shipped"] {
        assert_eq!(state_of(&t, slug), State::Done, "{slug}");
    }

    hold(&mut t, &["finished", "closed", "shipped"]);
    assert_eq!(state_of(&t, "finished"), State::InProgress);
    assert_eq!(state_of(&t, "closed"), State::InProgress);
    assert_eq!(state_of(&t, "shipped"), State::Review);
    for slug in ["finished", "closed", "shipped"] {
        assert!(!derive::leaf_done(find(&t, slug)), "{slug}");
    }

    // Clearing the holds makes them Done again; setting the same holds changes nothing.
    let mut cleared = t.clone();
    assert!(cleared.set_done_holds(&BTreeMap::new()));
    assert_eq!(state_of(&cleared, "shipped"), State::Done);
    assert!(!cleared.set_done_holds(&BTreeMap::new()));
}

#[test]
fn a_held_group_is_not_done() {
    let mut t = tree(vec![group("g", vec![leaf("a", Status::Done)])]);
    assert_eq!(state_of(&t, "g"), State::Done);
    hold(&mut t, &["g"]);
    assert_eq!(state_of(&t, "g"), State::InProgress);

    // A group closed outright is not Done while held either.
    let mut t = tree(vec![group("g", vec![pr_ship("a")])]);
    file_mut(t.projects[0].task.children.get_mut(0).unwrap()).status = Status::Done;
    assert_eq!(state_of(&t, "g"), State::Done);
    hold(&mut t, &["g"]);
    assert_eq!(state_of(&t, "g"), State::Ready);

    // A held child keeps its group from Done.
    let mut t = tree(vec![group(
        "g",
        vec![leaf("a", Status::Done), leaf("b", Status::Done)],
    )]);
    hold(&mut t, &["b"]);
    assert_eq!(state_of(&t, "g"), State::InProgress);
}

#[test]
fn a_model_level_advance_on_the_last_todo_leaves_the_task_held() {
    let mut t = tree(vec![done(pr_ship("a"), &[0, 1])]);
    hold(&mut t, &["a"]);
    let id = id_of(&t, "a");
    let change = advance(&t, &ctx(), &id, false).expect("advance is not refused by a hold");
    update(&mut t, &change);
    let task = find(&t, "a");
    assert!(
        task.file
            .as_ref()
            .unwrap()
            .todos
            .iter()
            .all(|todo| todo.state == TodoState::Done)
    );
    assert_eq!(task.done_hold.as_deref(), Some(HOLD));
    assert_ne!(state_of(&t, "a"), State::Done);
    // The written file carries nothing of the hold.
    let written = serialize_task(change.file().unwrap()).unwrap();
    assert!(!written.contains("not done"), "{written}");
    assert!(!written.contains("hold"), "{written}");
}

#[test]
fn ship_waits_sees_a_held_parent_as_not_done() {
    let t = tree(vec![
        leaf("a", Status::Done),
        done(stacked_on(pr_ship("b"), "a", UnblockPoint::Review), &[0, 1]),
    ]);
    let b = id_of(&t, "b");
    assert!(advance(&t, &ctx(), &b, false).is_ok());

    let mut held = t;
    hold(&mut held, &["a"]);
    assert_eq!(
        advance(&held, &ctx(), &b, false).unwrap_err(),
        "b cannot ship before a is Done (it is In Progress); use task: advance override"
    );
    assert!(todo_skip(&held, &ctx(), &b, Some(2)).is_err());
    let place = Place::locate(&held, &b).unwrap();
    let file = find(&held, "b").file.clone().unwrap();
    let (parent, state) = rules::ship_waits_on(&place, &held, &file, 2).unwrap();
    assert_eq!(parent.slug, "a");
    assert_eq!(state, State::InProgress);
}

#[test]
fn every_rule_that_tests_done_treats_a_held_task_as_not_done() {
    // `a` has every todo done and is held; `c` waits on its group reaching the merged point.
    let mut a = leaf("a", Status::Open);
    file_mut(&mut a).todos = vec![todo("do it", None)];
    let mut t = tree(vec![
        group("g", vec![done(a, &[0])]),
        after(pr_ship("c"), "g", UnblockPoint::Merged),
    ]);
    assert!(!derive::blocked(find(&t, "c"), &t));
    assert!(rules::open_descendants(find(&t, "g"), &t).is_empty());
    hold(&mut t, &["a"]);
    let a = find(&t, "a");

    // The derived state, the group around it and the edge waiting on the group.
    assert_eq!(derive::state(a, &t), State::InProgress);
    assert_eq!(state_of(&t, "g"), State::InProgress);
    assert!(derive::blocked(find(&t, "c"), &t));
    // The roll-up counts it open, and the activity view does not count it finished.
    let top = &t.projects[0].task;
    assert_eq!(rollup::rollup(top, &t, &Default::default()).done, 0);
    assert!(!activity::is_closed(a, &t));
    // `task: drop` on the group lists it as open.
    let open = rules::open_descendants(find(&t, "g"), &t);
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].slug, "a");
}

#[test]
fn reopen_on_a_held_open_leaf_with_no_open_todo_points_at_todo_reopen() {
    let mut a = leaf("a", Status::Open);
    file_mut(&mut a).todos = vec![todo("do it", None)];
    let mut t = tree(vec![done(a, &[0])]);
    hold(&mut t, &["a"]);
    let id = id_of(&t, "a");
    assert_ne!(state_of(&t, "a"), State::Done);
    assert_eq!(
        reopen(&t, &ctx(), &id).unwrap_err(),
        "a is open but has no open todo; reopen one with todo: reopen"
    );
}
