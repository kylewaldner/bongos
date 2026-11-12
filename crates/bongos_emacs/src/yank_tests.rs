use std::cell::RefCell;
use std::rc::Rc;

use bongos_emacs_model::kill_ring::{Entry, Piece};
use editor::kill_ring;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{ClipboardItem, KeyBinding, Modifiers, MouseButton, TestAppContext};
use text::ToOffset as _;
use workspace::MinibufferRouting;

use crate::actions::{KillLine, SetMarkCommand, Yank, YankPop};
use crate::apply_tests::init_test;
use crate::mark::{mark_for, push_mark, ring_for};

async fn test_context(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-y", Yank::default(), Some("Editor")),
            KeyBinding::new("alt-y", YankPop::default(), Some("Editor")),
            KeyBinding::new("ctrl-k", KillLine::default(), Some("Editor")),
            KeyBinding::new("ctrl-space", SetMarkCommand::default(), Some("Editor")),
            KeyBinding::new("ctrl-f", editor::actions::MoveRight, Some("Editor")),
            KeyBinding::new("alt-f", editor::actions::SelectRight, Some("Editor")),
            KeyBinding::new("ctrl-/", editor::actions::Undo, Some("Editor")),
        ])
    });
    cx.set_state(marked);
    cx
}

/// Pushes `entries`, oldest first, so the last one is the newest.
fn seed(cx: &mut EditorTestContext, entries: Vec<Entry>) {
    cx.update(|_, cx| {
        for entry in entries {
            kill_ring::update(cx, |ring| ring.push(entry));
        }
    });
}

fn plain(texts: &[&str]) -> Vec<Entry> {
    texts
        .iter()
        .map(|text| Entry::plain(text.to_string()))
        .collect()
}

fn piece(first_line_indent: u32) -> Piece {
    Piece {
        len: 0,
        is_entire_line: false,
        first_line_indent,
        origin: None,
    }
}

fn pieces(texts: &[&str]) -> Entry {
    let texts = texts
        .iter()
        .map(|text| text.to_string())
        .collect::<Vec<_>>();
    let pieces = texts.iter().map(|_| piece(0)).collect();
    Entry::from_pieces(&texts, pieces)
}

fn ring_texts(cx: &mut EditorTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        kill_ring::read(cx).map_or(Vec::new(), |ring| {
            (0..ring.len())
                .filter_map(|ix| ring.get(ix).map(|entry| entry.text.clone()))
                .collect()
        })
    })
}

fn clipboard_text(cx: &mut EditorTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

/// The mark and the mark ring (newest first) of the buffer, as offsets.
fn marks(cx: &mut EditorTestContext) -> (Option<usize>, Vec<usize>) {
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let offset = |anchor: multi_buffer::Anchor, cx: &gpui::App| {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(anchor).unwrap();
            text_anchor.to_offset(buffer)
        };
        let mark = mark_for(&editor, cx).map(|anchor| offset(anchor, cx));
        let ring = ring_for(&editor, cx)
            .into_iter()
            .map(|anchor| offset(anchor, cx))
            .collect();
        (mark, ring)
    })
}

fn echo(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    messages
}

fn set_user_settings(
    cx: &mut EditorTestContext,
    change: impl FnOnce(&mut settings::SettingsContent),
) {
    use gpui::BorrowAppContext as _;
    cx.update(|_, cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, change)
        });
    });
}

#[gpui::test]
async fn yank_inserts_at_point_and_sets_the_mark_at_its_start(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    seed(&mut cx, plain(&["older", "new "]));
    // An earlier mark goes to the mark ring: exactly one push.
    let editor = cx.editor.clone();
    cx.update(|window, cx| {
        let start = editor
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx)
            .anchor_before(multi_buffer::MultiBufferOffset(0));
        push_mark(&editor, Some(start), false, window, cx)
    });
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
    let messages = echo(&mut cx);

    cx.simulate_keystrokes("ctrl-y");

    cx.assert_editor_state("one new ˇtwo");
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));
    assert!(messages.borrow().is_empty());
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    assert_eq!(ring_texts(&mut cx), ["new ", "older"]);
    // The clipboard is not written.
    assert_eq!(clipboard_text(&mut cx), None);
}

#[gpui::test]
async fn yank_is_one_undo_step(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "aˇb").await;
    seed(&mut cx, plain(&["x\ny\nz"]));
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("ax\ny\nzˇb");
    cx.simulate_keystrokes("ctrl-/");
    cx.assert_editor_state("aˇb");
}

#[gpui::test]
async fn three_carets_get_their_own_pieces(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇa\nˇb\nˇc").await;
    seed(&mut cx, vec![pieces(&["1", "2", "3"])]);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("1ˇa\n2ˇb\n3ˇc");
    cx.simulate_keystrokes("ctrl-/");
    cx.assert_editor_state("ˇa\nˇb\nˇc");
}

#[gpui::test]
async fn a_two_line_piece_stays_whole_at_its_caret(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇa\nˇb").await;
    seed(&mut cx, vec![pieces(&["x\ny", "z"])]);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("x\nyˇa\nzˇb");
}

#[gpui::test]
async fn a_caret_count_that_does_not_match_inserts_the_whole_text_at_each(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇa\nˇb").await;
    seed(&mut cx, vec![pieces(&["1", "2", "3"])]);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("1\n2\n3ˇa\n1\n2\n3ˇb");
}

#[gpui::test]
async fn yank_pop_cycles_from_newest_to_oldest_and_wraps(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "<ˇ>").await;
    seed(&mut cx, plain(&["oldest", "middle", "newest"]));
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("<newestˇ>");
    assert_eq!(marks(&mut cx), (Some(1), vec![]));
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("<middleˇ>");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("<oldestˇ>");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("<newestˇ>");
    // A negative count goes toward newer entries.
    cx.dispatch_action(YankPop { count: Some(-1) });
    cx.assert_editor_state("<oldestˇ>");
    // The mark, the ring order and the clipboard are untouched.
    assert_eq!(marks(&mut cx), (Some(1), vec![]));
    assert_eq!(ring_texts(&mut cx), ["newest", "middle", "oldest"]);
    assert_eq!(clipboard_text(&mut cx), None);
    // Each pop is one undo step.
    cx.simulate_keystrokes("ctrl-/");
    cx.assert_editor_state("<newestˇ>");
}

#[gpui::test]
async fn yank_pop_at_three_carets_replaces_each_piece(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇa\nˇb\nˇc").await;
    seed(
        &mut cx,
        vec![pieces(&["1", "2", "3"]), pieces(&["x\ny", "", "zz"])],
    );
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("x\nyˇa\nˇb\nzzˇc");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("1ˇa\n2ˇb\n3ˇc");
}

#[gpui::test]
async fn yank_pop_after_another_command_changes_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇab").await;
    seed(&mut cx, plain(&["old", "new"]));
    let messages = echo(&mut cx);

    // Before any yank.
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("ˇab");
    assert_eq!(*messages.borrow(), ["Previous command was not a yank"]);

    cx.simulate_keystrokes("ctrl-y ctrl-f alt-y");
    cx.assert_editor_state("newaˇb");
    assert_eq!(
        *messages.borrow(),
        [
            "Previous command was not a yank",
            "Previous command was not a yank"
        ]
    );
}

#[gpui::test]
async fn a_mouse_click_after_a_yank_ends_it(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇab").await;
    seed(&mut cx, plain(&["old", "new"]));
    cx.simulate_keystrokes("ctrl-y");
    let position = cx.pixel_position("newˇab");
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
    // A click records no command, so Yank is still the last one; the yank is forgotten.
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("newˇab");
}

fn rust_block() -> (String, Entry) {
    // Killed from inside an indented block, so its first line was indented by 8.
    let text = "if a {\n            b();\n        }".to_string();
    let entry = Entry::from_pieces(&[text], vec![piece(8)]);
    ("c();".to_string(), entry)
}

async fn rust_context(cx: &mut TestAppContext) -> EditorTestContext {
    let mut cx = test_context(cx, "ˇ").await;
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(language::rust_lang()), cx));
    cx.set_state("fn f() {\n    x();\nˇ\n}\n");
    cx.run_until_parked();
    cx
}

#[gpui::test]
async fn yank_pop_replaces_an_auto_indented_block(cx: &mut TestAppContext) {
    let mut cx = rust_context(cx).await;
    let (older, block) = rust_block();
    seed(&mut cx, vec![Entry::plain(older), block]);

    cx.simulate_keystrokes("ctrl-y");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    if a {\n        b();\n    }ˇ\n}\n");

    cx.simulate_keystrokes("alt-y");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    c();ˇ\n}\n");
    cx.simulate_keystrokes("ctrl-/");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    if a {\n        b();\n    }ˇ\n}\n");
    cx.simulate_keystrokes("ctrl-/");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\nˇ\n}\n");
}

/// With the parse in the background, auto-indent lands after the command finished; the anchors
/// still cover it and undo still takes it with the paste.
#[gpui::test]
async fn yank_pop_replaces_a_block_indented_after_the_command(cx: &mut TestAppContext) {
    let mut cx = rust_context(cx).await;
    cx.update_buffer(|buffer, _| buffer.set_sync_parse_timeout(None));
    let (older, block) = rust_block();
    seed(&mut cx, vec![Entry::plain(older), block]);

    cx.simulate_keystrokes("ctrl-y");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    if a {\n        b();\n    }ˇ\n}\n");

    cx.simulate_keystrokes("alt-y");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    c();ˇ\n}\n");
    cx.simulate_keystrokes("ctrl-/");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\n    if a {\n        b();\n    }ˇ\n}\n");
    cx.simulate_keystrokes("ctrl-/");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\nˇ\n}\n");
}

#[gpui::test]
async fn yank_pop_replaces_a_block_pasted_without_auto_indent(cx: &mut TestAppContext) {
    let mut cx = rust_context(cx).await;
    set_user_settings(&mut cx, |content| {
        content.project.all_languages.defaults.auto_indent_on_paste = Some(false);
    });
    let (older, block) = rust_block();
    seed(&mut cx, vec![Entry::plain(older), block]);

    cx.simulate_keystrokes("ctrl-y");
    cx.run_until_parked();
    cx.assert_editor_state("fn f() {\n    x();\nif a {\n            b();\n        }ˇ\n}\n");

    cx.simulate_keystrokes("alt-y");
    cx.run_until_parked();
    // Plain text is always auto-indented, as `editor::Paste` does with it.
    cx.assert_editor_state("fn f() {\n    x();\n    c();ˇ\n}\n");
}

#[gpui::test]
async fn yank_with_a_count_picks_an_older_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇ").await;
    seed(&mut cx, plain(&["oldest", "middle", "newest"]));

    cx.dispatch_action(Yank { count: Some(2) });
    cx.assert_editor_state("middleˇ");
    // M-y continues from the entry the yank took.
    cx.dispatch_action(YankPop { count: None });
    cx.assert_editor_state("oldestˇ");

    cx.set_state("ˇ");
    cx.dispatch_action(Yank { count: Some(0) });
    cx.assert_editor_state("oldestˇ");

    cx.set_state("ˇ");
    cx.dispatch_action(Yank { count: Some(-1) });
    cx.assert_editor_state("middleˇ");
    assert_eq!(ring_texts(&mut cx), ["newest", "middle", "oldest"]);
}

#[gpui::test]
async fn yank_in_a_read_only_buffer_does_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "aˇb").await;
    seed(&mut cx, plain(&["x"]));
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    let messages = echo(&mut cx);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("aˇb");
    assert_eq!(*messages.borrow(), ["Buffer is read-only"]);
    assert_eq!(marks(&mut cx), (None, vec![]));
}

#[gpui::test]
async fn yank_with_an_empty_ring_changes_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "aˇb").await;
    let messages = echo(&mut cx);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("aˇb");
    assert_eq!(*messages.borrow(), ["Kill ring is empty"]);
    assert_eq!(marks(&mut cx), (None, vec![]));
}

#[gpui::test]
async fn yank_captures_text_copied_outside(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇ").await;
    seed(&mut cx, plain(&["ring"]));
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("outside".into())));
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("outsideˇ");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("ringˇ");
}

#[gpui::test]
async fn yank_deactivates_an_active_region_by_default(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two").await;
    seed(&mut cx, plain(&["X"]));
    cx.simulate_keystrokes("ctrl-space alt-f alt-f alt-f");
    cx.assert_editor_state("«oneˇ» two");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("oneXˇ two");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    // The C-SPC mark went to the ring; the new mark is the insert start.
    assert_eq!(marks(&mut cx), (Some(3), vec![0]));
}

#[gpui::test]
async fn yank_replaces_the_region_with_delete_selection(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two").await;
    set_user_settings(&mut cx, |content| {
        content.emacs.get_or_insert_default().delete_selection = Some(true);
    });
    seed(&mut cx, plain(&["older", "X"]));
    cx.simulate_keystrokes("ctrl-space alt-f alt-f alt-f");
    cx.assert_editor_state("«oneˇ» two");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("Xˇ two");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("olderˇ two");
    cx.simulate_keystrokes("ctrl-/");
    cx.assert_editor_state("Xˇ two");
}
