use bongos_emacs_model::{Caret, KilledPiece, Plan};
use editor::ClipboardSelection;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{ClipboardEntry, ClipboardItem, TestAppContext};

use crate::apply::Command;
use crate::apply_tests::{capture_kills, init_test, run_plan};
use crate::kill_sink::{KillDirection, KillKind};

fn clipboard(cx: &mut EditorTestContext) -> (String, Vec<ClipboardSelection>) {
    let item = cx
        .update(|_, cx| cx.read_from_clipboard())
        .expect("the clipboard has an item");
    let Some(ClipboardEntry::String(entry)) = item.entries().first() else {
        panic!("the clipboard holds a string");
    };
    let metadata = entry
        .metadata_json::<Vec<ClipboardSelection>>()
        .unwrap_or_default();
    (entry.text().clone(), metadata)
}

/// Deletes each caret's line (with its newline) and kills it as a whole line.
fn kill_lines(carets: &[Caret], lines: &[&str]) -> Plan {
    let mut plan = Plan::default();
    let mut start = 0;
    for line in lines {
        plan.edits.push((start..start + line.len(), String::new()));
        plan.killed.push(KilledPiece {
            text: line.to_string(),
            backward: false,
        });
        start += line.len();
    }
    plan.carets = vec![Caret::point(0)];
    assert_eq!(carets.len(), lines.len());
    plan
}

#[gpui::test]
async fn default_sink_writes_text_and_whole_line_metadata(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone\nˇtwo\nthree");

    assert!(run_plan(
        &mut cx,
        Command::edit("emacs::KillRegionOrLine").with_whole_lines(),
        |carets| kill_lines(carets, &["one\n", "two\n"]),
    ));

    cx.assert_editor_state("ˇthree");
    let (text, metadata) = clipboard(&mut cx);
    assert_eq!(text, "one\ntwo\n");
    assert_eq!(metadata.len(), 2);
    assert!(metadata.iter().all(|selection| selection.is_entire_line));
    assert_eq!(
        metadata.iter().map(|s| s.len).collect::<Vec<_>>(),
        vec![4, 4]
    );

    // Paste at one caret puts both lines back as lines, above the caret's line.
    cx.set_state("thrˇee");
    cx.dispatch_action(editor::actions::Paste);
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree");
}

#[gpui::test]
async fn default_sink_gives_each_caret_its_piece_back(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("«abˇ» x «cdˇ» y «efˇ»");

    assert!(run_plan(
        &mut cx,
        Command::edit("emacs::KillWord"),
        |carets| {
            let mut plan = Plan::default();
            let mut removed = 0;
            for caret in carets {
                let range = caret.region();
                plan.edits.push((range.clone(), String::new()));
                plan.carets.push(Caret::point(range.start - removed));
                removed += range.len();
            }
            plan.killed = ["ab", "cd", "ef"]
                .iter()
                .map(|text| KilledPiece {
                    text: text.to_string(),
                    backward: false,
                })
                .collect();
            plan
        }
    ));

    cx.assert_editor_state("ˇ x ˇ y ˇ");
    let (text, metadata) = clipboard(&mut cx);
    assert_eq!(text, "ab\ncd\nef");
    assert!(metadata.iter().all(|selection| !selection.is_entire_line));

    cx.dispatch_action(editor::actions::Paste);
    cx.assert_editor_state("abˇ x cdˇ y efˇ");
}

#[gpui::test]
async fn a_replaced_sink_receives_the_kill_instead_of_the_clipboard(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("before".into())));
    cx.set_state("one «twoˇ» three");

    assert!(run_plan(
        &mut cx,
        Command::edit("emacs::KillWord"),
        |carets| {
            let range = carets[0].region();
            Plan {
                edits: vec![(range.clone(), String::new())],
                carets: vec![Caret::point(range.start)],
                killed: vec![KilledPiece {
                    text: "two".into(),
                    backward: true,
                }],
            }
        }
    ));

    cx.assert_editor_state("one ˇ three");
    assert_eq!(clipboard(&mut cx).0, "before");
    let kills = kills.borrow();
    assert_eq!(kills.len(), 1);
    assert_eq!(kills[0].pieces, vec!["two".to_string()]);
    assert_eq!(kills[0].kind, KillKind::Kill);
    assert_eq!(kills[0].direction, KillDirection::Backward);
    assert_eq!(kills[0].command, "emacs::KillWord");
    assert_eq!(kills[0].metadata.len(), 1);
}

#[gpui::test]
async fn a_copy_runs_in_a_read_only_buffer(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));

    assert!(run_plan(
        &mut cx,
        Command::copy("emacs::CopyRegionOrLine"),
        |carets| Plan {
            edits: Vec::new(),
            carets: carets.to_vec(),
            killed: vec![KilledPiece {
                text: "two".into(),
                backward: false,
            }],
        }
    ));

    cx.assert_editor_state("one «twoˇ» three");
    let kills = kills.borrow();
    assert_eq!(kills.len(), 1);
    assert_eq!(kills[0].kind, KillKind::Copy);
}
