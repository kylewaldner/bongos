use std::cell::RefCell;
use std::rc::Rc;

use bongos_emacs_model::{Caret, KilledPiece, Plan};
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use settings::SettingsStore;

use crate::actions::ForwardWord;
use crate::apply::{self, Command};
use crate::kill_sink::{Kill, set_kill_sink};

pub(crate) fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init(cx);
    });
}

/// Replaces the kill sink with one that keeps every kill, and returns them.
pub(crate) fn capture_kills(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<Kill>>> {
    let kills = Rc::new(RefCell::new(Vec::new()));
    let sink = kills.clone();
    cx.update(|_, cx| set_kill_sink(cx, move |kill, _, _| sink.borrow_mut().push(kill)));
    kills
}

/// Runs `plan` (computed from the carets) through `apply::run` as `command`.
pub(crate) fn run_plan(
    cx: &mut EditorTestContext,
    command: Command,
    plan: impl FnOnce(&[Caret]) -> Plan,
) -> bool {
    let editor = cx.editor.clone();
    cx.update(|window, cx| apply::run(&editor, window, cx, command, |_, carets| plan(carets)))
}

/// Inserts `insert` at every caret, the caret after it.
fn insert_at_carets(carets: &[Caret], insert: &str) -> Plan {
    let mut plan = Plan::default();
    for (ix, caret) in carets.iter().enumerate() {
        plan.edits
            .push((caret.head..caret.head, insert.to_string()));
        plan.carets
            .push(Caret::point(caret.head + insert.len() * (ix + 1)));
    }
    plan
}

/// Kills each caret's region.
fn kill_regions(carets: &[Caret]) -> Plan {
    let mut plan = Plan::default();
    let mut removed = 0;
    for caret in carets {
        let range = caret.region();
        plan.edits.push((range.clone(), String::new()));
        plan.carets.push(Caret::point(range.start - removed));
        removed += range.len();
    }
    plan
}

#[gpui::test]
async fn three_carets_are_one_undo_step(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("aˇ\nbˇ\ncˇ\n");

    assert!(run_plan(
        &mut cx,
        Command::edit("test::InsertX"),
        |carets| { insert_at_carets(carets, "X") }
    ));
    cx.assert_editor_state("aXˇ\nbXˇ\ncXˇ\n");
    assert!(run_plan(
        &mut cx,
        Command::edit("test::InsertY"),
        |carets| { insert_at_carets(carets, "Y") }
    ));
    cx.assert_editor_state("aXYˇ\nbXYˇ\ncXYˇ\n");

    cx.dispatch_action(editor::actions::Undo);
    assert_eq!(cx.buffer_text(), "aX\nbX\ncX\n");
    cx.dispatch_action(editor::actions::Undo);
    assert_eq!(cx.buffer_text(), "a\nb\nc\n");
    cx.dispatch_action(editor::actions::Redo);
    assert_eq!(cx.buffer_text(), "aX\nbX\ncX\n");
}

#[gpui::test]
async fn read_only_buffer_is_unchanged_and_nothing_is_killed(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));

    let applied = run_plan(&mut cx, Command::edit("test::Kill"), |carets| Plan {
        killed: vec![KilledPiece {
            text: "two".into(),
            backward: false,
            source_start: None,
            whole_line: false,
        }],
        ..kill_regions(carets)
    });

    assert!(!applied);
    assert!(kills.borrow().is_empty());
    cx.assert_editor_state("one «twoˇ» three");

    // Motions still run in a read-only buffer.
    cx.set_state("ˇone two");
    cx.dispatch_action(ForwardWord::default());
    cx.assert_editor_state("oneˇ two");
}

#[gpui::test]
async fn the_kill_is_saved_before_the_text_is_deleted(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let editor = cx.editor.clone();
    let sink_seen = seen.clone();
    cx.update(|_, cx| {
        set_kill_sink(cx, move |kill, _, cx| {
            let text = editor.read(cx).buffer().read(cx).snapshot(cx).text();
            sink_seen.borrow_mut().push((kill.pieces, text));
        })
    });
    cx.set_state("one «twoˇ» three «fourˇ»");

    assert!(run_plan(&mut cx, Command::edit("test::Kill"), |carets| {
        Plan {
            killed: vec![
                KilledPiece {
                    text: "two".into(),
                    backward: false,
                    source_start: None,
                    whole_line: false,
                },
                KilledPiece {
                    text: "four".into(),
                    backward: false,
                    source_start: None,
                    whole_line: false,
                },
            ],
            ..kill_regions(carets)
        }
    }));

    cx.assert_editor_state("one ˇ three ˇ");
    assert_eq!(
        *seen.borrow(),
        vec![(
            vec!["two".to_string(), "four".to_string()],
            "one two three four".to_string()
        )]
    );
}

#[gpui::test]
async fn an_empty_plan_changes_nothing_and_keeps_the_region(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));

    assert!(!run_plan(&mut cx, Command::edit("test::Zap"), |_| {
        Plan::default()
    }));

    cx.assert_editor_state("one «twoˇ» three");
    assert!(kills.borrow().is_empty());
    cx.update_editor(|editor, _, _| assert!(editor.selection_mark_mode()));
}

#[gpui::test]
async fn an_edit_ends_mark_mode_and_a_motion_does_not(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone two");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));

    cx.dispatch_action(ForwardWord {
        count: None,
        select: true,
    });
    cx.assert_editor_state("«oneˇ» two");
    cx.update_editor(|editor, _, _| assert!(editor.selection_mark_mode()));

    assert!(run_plan(&mut cx, Command::edit("test::Kill"), |carets| {
        Plan {
            killed: vec![KilledPiece {
                text: "one".into(),
                backward: false,
                source_start: None,
                whole_line: false,
            }],
            ..kill_regions(carets)
        }
    }));
    cx.assert_editor_state("ˇ two");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn typing_before_a_command_is_a_separate_undo_step(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇ");
    // Tests do not group transactions unless asked; the real interval merges quick edits.
    cx.update_buffer(|buffer, _| buffer.set_group_interval(std::time::Duration::from_secs(60)));

    cx.simulate_input("a");
    cx.assert_editor_state("aˇ");
    assert!(run_plan(
        &mut cx,
        Command::edit("test::InsertX"),
        |carets| { insert_at_carets(carets, "X") }
    ));
    cx.assert_editor_state("aXˇ");

    // Undo removes only the command's effect; the typed text stays.
    cx.dispatch_action(editor::actions::Undo);
    assert_eq!(cx.buffer_text(), "a");
    cx.dispatch_action(editor::actions::Undo);
    assert_eq!(cx.buffer_text(), "");
}

#[gpui::test]
async fn whole_line_labels_are_per_piece_not_per_command(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("«one\nˇ»two\nthree");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    // A second caret kills the line "three" (no region) in a command that takes whole lines.
    assert!(run_plan(
        &mut cx,
        Command::edit("test::Kill").with_whole_lines(),
        |_| Plan {
            edits: vec![(0..4, String::new()), (8..13, String::new())],
            carets: vec![Caret::point(0), Caret::point(4)],
            killed: vec![
                KilledPiece {
                    text: "one\n".into(),
                    backward: false,
                    source_start: None,
                    whole_line: false,
                },
                KilledPiece {
                    text: "three\n".into(),
                    backward: false,
                    source_start: None,
                    whole_line: true,
                },
            ],
        }
    ));
    let kills = kills.borrow();
    let labels: Vec<bool> = kills[0]
        .metadata
        .iter()
        .map(|selection| selection.is_entire_line)
        .collect();
    // Only the first caret had a region, so only the second piece is a whole line.
    assert_eq!(labels, vec![false, true]);
}
