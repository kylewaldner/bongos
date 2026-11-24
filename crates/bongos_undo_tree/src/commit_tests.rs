use std::{ops::Range, time::Duration};

use editor::MultiBufferOffset;
use gpui::TestAppContext;
use language::Capability;
use text::UndoNodeId;

use crate::{
    adapter::shape_of,
    commit::NOTHING_COMMITTED_MESSAGE,
    panel::UndoTreeClock,
    preview::BUFFER_CHANGED_MESSAGE,
    test_support::{Fixture, Setup, branch_history, setup},
};

fn selections(fixture: &mut Fixture) -> Vec<Range<MultiBufferOffset>> {
    fixture.cx.update_editor(|editor, _, cx| {
        editor
            .selections
            .ranges::<MultiBufferOffset>(&editor.display_snapshot(cx))
    })
}

/// How many states the real buffer's tree has, the state it is in and that state's parent.
fn tree_state(setup: &mut Setup) -> (usize, UndoNodeId, Option<UndoNodeId>) {
    setup
        .fixture
        .buffer
        .read_with(&setup.fixture.cx.cx.cx, |buffer, _| {
            let tree = buffer.undo_tree();
            let current = tree.current();
            (tree.len(), current, tree.parent(current))
        })
}

/// Every state in the real buffer's tree.
fn all_nodes(setup: &mut Setup) -> Vec<UndoNodeId> {
    let now = setup.fixture.cx.update(|_, cx| UndoTreeClock::now(cx));
    setup
        .fixture
        .buffer
        .read_with(&setup.fixture.cx.cx.cx, |buffer, _| {
            let tree = buffer.undo_tree();
            let shape = shape_of(tree, tree.current(), now);
            (0..shape.shape.nodes.len())
                .filter_map(|index| shape.id(index))
                .collect()
        })
}

#[gpui::test]
async fn enter_commits_the_node_text_and_selections_as_one_undoable_step(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let two = setup.fixture.type_step("two ");
    let two_selections = selections(&mut setup.fixture);
    setup.fixture.undo();
    let three = setup.fixture.type_step("three ");
    let entry_selections = selections(&mut setup.fixture);
    let (len, _, _) = tree_state(&mut setup);
    let buffer = setup.fixture.buffer.clone();

    let panel = setup.vundo();
    setup.keys("p");
    assert_eq!(setup.cursor_node(&panel), Some(two));
    setup.keys("enter");

    setup.assert_closed(&buffer);
    assert_eq!(setup.fixture.text(), "one two ");
    assert_eq!(selections(&mut setup.fixture), two_selections);
    let (committed_len, committed, parent) = tree_state(&mut setup);
    assert_eq!(committed_len, len + 1, "the commit is one new state");
    assert_eq!(
        parent,
        Some(three),
        "the commit is a child of the entry state"
    );
    assert_ne!(committed, two);

    setup.fixture.undo();
    assert_eq!(setup.fixture.text(), "one three ");
    assert_eq!(setup.fixture.current(), three);
    assert_eq!(selections(&mut setup.fixture), entry_selections);
    assert_eq!(tree_state(&mut setup).0, len + 1);
}

#[gpui::test]
async fn a_commit_within_the_group_interval_is_not_merged_into_the_entry_node(
    cx: &mut TestAppContext,
) {
    let mut setup = setup(cx).await;
    let one = setup.fixture.type_step("one ");
    let two = setup.fixture.type_step("two ");
    setup
        .fixture
        .buffer
        .update(&mut setup.fixture.cx.cx.cx, |buffer, _| {
            buffer.set_group_interval(Duration::from_millis(300))
        });
    let two_at = setup.last_edit_at(two);
    setup.clock.set(two_at + Duration::from_millis(100));
    let (len, _, _) = tree_state(&mut setup);

    setup.vundo();
    setup.keys("b");
    setup.keys("enter");

    assert_eq!(setup.fixture.text(), "one ");
    let (committed_len, committed, parent) = tree_state(&mut setup);
    assert_eq!(committed_len, len + 1);
    assert_eq!(
        parent,
        Some(two),
        "the commit is not merged into the entry state"
    );
    assert_ne!(committed, one);

    setup.fixture.undo();
    assert_eq!(setup.fixture.text(), "one two ");
    assert_eq!(setup.fixture.current(), two);
}

#[gpui::test]
async fn every_earlier_node_stays_in_the_tree_after_a_commit(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    branch_history(&mut setup.fixture);
    let before = all_nodes(&mut setup);
    assert_eq!(before.len(), 4);

    setup.vundo();
    setup.keys("b b");
    setup.keys("enter");

    assert_eq!(setup.fixture.text(), "");
    let after = all_nodes(&mut setup);
    assert_eq!(after.len(), before.len() + 1);
    for node in &before {
        assert!(after.contains(node), "{node:?} is still in the tree");
    }
}

#[gpui::test]
async fn enter_on_the_entry_node_changes_nothing(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [_, _, three] = branch_history(&mut setup.fixture);
    let buffer = setup.fixture.buffer.clone();
    let version = buffer.read_with(&setup.fixture.cx.cx.cx, |buffer, _| buffer.version());
    let before = tree_state(&mut setup);

    let panel = setup.vundo();
    assert_eq!(setup.cursor_node(&panel), Some(three));
    setup.keys("enter");

    setup.assert_closed(&buffer);
    assert_eq!(setup.fixture.text(), "one three ");
    assert_eq!(tree_state(&mut setup), before);
    assert_eq!(
        buffer.read_with(&setup.fixture.cx.cx.cx, |buffer, _| buffer.version()),
        version
    );
}

#[gpui::test]
async fn a_buffer_change_before_the_diff_resolves_commits_nothing(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [_, _, three] = branch_history(&mut setup.fixture);
    let buffer = setup.fixture.buffer.clone();
    let (len, _, _) = tree_state(&mut setup);

    let panel = setup.vundo();
    setup.keys("p");
    panel.update_in(&mut setup.fixture.cx.cx.cx, |panel, window, cx| {
        panel.confirm(window, cx)
    });
    assert!(panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.is_committing()));
    assert_eq!(
        setup.capability(&buffer),
        Capability::ReadWrite,
        "the capability is back before the diff runs"
    );

    buffer.update(&mut setup.fixture.cx.cx.cx, |buffer, cx| {
        buffer.edit([(0..0, "outside ")], None, cx)
    });
    setup.fixture.cx.run_until_parked();

    assert_eq!(
        setup.messages.borrow().as_slice(),
        [NOTHING_COMMITTED_MESSAGE]
    );
    assert!(
        !setup
            .messages
            .borrow()
            .iter()
            .any(|message| message == BUFFER_CHANGED_MESSAGE)
    );
    setup.assert_closed(&buffer);
    assert_eq!(setup.fixture.text(), "outside one three ");
    let (edited_len, _, parent) = tree_state(&mut setup);
    assert_eq!(edited_len, len + 1, "only the outside edit is a new state");
    assert_eq!(parent, Some(three));
}
