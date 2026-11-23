use gpui::{Entity, TestAppContext};
use language::unified_diff;

use crate::{
    panel::UndoTreePanel,
    test_support::{Fixture, Setup, setup},
};

/// The texts of a history that types `one`, `two`, undoes `two` and types `three`, one line
/// each: the texts after `one`, after `two` and after `three`, the state the buffer ends in.
fn line_history(fixture: &mut Fixture) -> [String; 3] {
    fixture.type_step("one\n");
    let one = fixture.text();
    fixture.type_step("two\n");
    let two = fixture.text();
    fixture.undo();
    fixture.type_step("three\n");
    let three = fixture.text();
    assert_ne!(one, two);
    assert_ne!(two, three);
    [one, two, three]
}

fn diff_text(setup: &mut Setup, panel: &Entity<UndoTreePanel>) -> Option<String> {
    panel.read_with(&setup.fixture.cx.cx.cx, |panel, cx| panel.diff_text(cx))
}

#[gpui::test]
async fn d_with_a_mark_diffs_the_marked_state_to_the_cursor(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [one, two, three] = line_history(&mut setup.fixture);
    let panel = setup.vundo();
    assert_eq!(diff_text(&mut setup, &panel), None);

    setup.keys("b m f p");
    assert_eq!(setup.preview_text(&panel), two);
    setup.keys("d");
    let shown = diff_text(&mut setup, &panel).expect("d shows a diff");
    assert_eq!(shown, unified_diff(&one, &two));
    assert!(shown.contains("+two"), "{shown}");
    assert_eq!(setup.preview_text(&panel), two);
    assert_eq!(setup.fixture.text(), three);
    assert_eq!(setup.panels().len(), 1);
}

#[gpui::test]
async fn d_without_a_mark_diffs_the_entry_state_to_the_cursor(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [_, two, three] = line_history(&mut setup.fixture);
    let panel = setup.vundo();

    setup.keys("p d");
    let shown = diff_text(&mut setup, &panel).expect("d shows a diff");
    assert_eq!(shown, unified_diff(&three, &two));
    assert!(
        shown.contains("-three") && shown.contains("+two"),
        "{shown}"
    );
    assert_eq!(setup.fixture.text(), three);
}

#[gpui::test]
async fn the_diff_view_refreshes_on_the_next_d(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [one, two, three] = line_history(&mut setup.fixture);
    let panel = setup.vundo();

    setup.keys("p d");
    let first = unified_diff(&three, &two);
    assert_eq!(diff_text(&mut setup, &panel), Some(first.clone()));

    setup.keys("b");
    assert_eq!(setup.preview_text(&panel), one);
    assert_eq!(
        diff_text(&mut setup, &panel),
        Some(first.clone()),
        "moving does not recompute the diff"
    );

    setup.keys("d");
    let second = unified_diff(&three, &one);
    assert_ne!(first, second);
    assert_eq!(diff_text(&mut setup, &panel), Some(second));
    assert_eq!(setup.fixture.text(), three);
}
