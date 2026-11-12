use std::collections::BTreeSet;

use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorMode, kill_ring};
use gpui::{
    Action, AppContext as _, ClipboardItem, KeyBinding, Modifiers, MouseButton, TestAppContext,
};
use multi_buffer::MultiBuffer;

use crate::actions::{
    BackwardKillWord, CopyRegionOrLine, KillLine, KillRegionOrLine, KillWord, ZapBackToChar,
    ZapBackUpToChar, ZapToChar, ZapUpToChar,
};
use crate::apply_tests::{capture_kills, init_test};
use crate::kill_ring_sink::KILL_COMMANDS;
use crate::{kill, last_command};

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-k", KillLine::default(), Some("Editor")),
            KeyBinding::new("ctrl-w", KillRegionOrLine::default(), Some("Editor")),
            KeyBinding::new("alt-w", CopyRegionOrLine::default(), Some("Editor")),
            KeyBinding::new("alt-d", KillWord::default(), Some("Editor")),
            KeyBinding::new("alt-backspace", BackwardKillWord::default(), Some("Editor")),
            KeyBinding::new("ctrl-f", editor::actions::MoveRight, Some("Editor")),
        ])
    });
    cx
}

/// The ring's texts, newest first.
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

#[gpui::test]
async fn consecutive_kill_lines_join_one_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");

    cx.simulate_keystrokes("ctrl-k ctrl-k ctrl-k");

    // At a line start C-k takes the newline too.
    cx.assert_editor_state("ˇ");
    assert_eq!(ring_texts(&mut cx), ["one\ntwo\nthree"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("one\ntwo\nthree"));
}

#[gpui::test]
async fn a_motion_between_kills_starts_a_new_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two");

    cx.simulate_keystrokes("alt-d ctrl-f alt-d");

    cx.assert_editor_state(" ˇ");
    assert_eq!(ring_texts(&mut cx), ["two", "one"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("two"));
}

#[gpui::test]
async fn a_mouse_click_between_kills_starts_a_new_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("oˇne\ntwo\nthree");

    cx.simulate_keystrokes("ctrl-k");
    cx.assert_editor_state("oˇ\ntwo\nthree");
    let position = cx.pixel_position("o\ntˇwo\nthree");
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("ctrl-k");

    cx.assert_editor_state("o\ntˇ\nthree");
    assert_eq!(ring_texts(&mut cx), ["wo", "ne"]);
}

#[gpui::test]
async fn carets_moved_without_a_command_start_a_new_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two");
    cx.simulate_keystrokes("alt-d");
    // A jump made without a key (a panel button, a test) records no command.
    cx.set_selections_state(" twoˇ");
    cx.simulate_keystrokes("alt-backspace");
    cx.assert_editor_state(" ˇ");
    assert_eq!(ring_texts(&mut cx), ["two", "one"]);

    // Carets left where the kill put them keep the sequence.
    cx.simulate_keystrokes("alt-backspace");
    assert_eq!(ring_texts(&mut cx), [" two", "one"]);
}

#[gpui::test]
async fn a_copy_appends_to_a_kill_but_a_kill_after_a_copy_starts_anew(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two\nthree");

    cx.simulate_keystrokes("alt-d alt-w");
    assert_eq!(ring_texts(&mut cx), ["one two\n"]);

    cx.simulate_keystrokes("ctrl-w");
    cx.assert_editor_state("ˇthree");
    assert_eq!(ring_texts(&mut cx), [" two\n", "one two\n"]);
}

#[gpui::test]
async fn a_kill_in_another_editor_starts_a_new_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two");
    cx.simulate_keystrokes("alt-d");

    let buffer = cx.multibuffer(|multibuffer, _| multibuffer.as_singleton().unwrap());
    let second = cx.update(|window, cx| {
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx))
    });
    // The same command right after, as a key in the second editor would run it.
    cx.update(|window, cx| {
        assert_eq!(last_command(window, cx), Some("emacs::KillWord"));
        kill::kill_line_command(&KillLine::default(), &second, window, cx);
        last_command::record(window, Some("emacs::KillLine"), cx);
    });

    assert_eq!(ring_texts(&mut cx), [" two", "one"]);
}

#[gpui::test]
async fn backward_word_kills_prepend(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("one two threeˇ");

    cx.simulate_keystrokes("alt-backspace alt-backspace");

    cx.assert_editor_state("one ˇ");
    assert_eq!(ring_texts(&mut cx), ["two three"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("two three"));
}

#[gpui::test]
async fn three_carets_killing_twice_keep_three_pieces(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two\nˇthree four\nˇfive six");

    cx.simulate_keystrokes("alt-d alt-d");

    cx.assert_editor_state("ˇ\nˇ\nˇ");
    let pieces = cx.update(|_, cx| {
        let entry = kill_ring::read(cx).and_then(|ring| ring.newest()).unwrap();
        entry
            .piece_texts()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    assert_eq!(pieces, ["one two", "three four", "five six"]);
    assert_eq!(ring_texts(&mut cx).len(), 1);

    // Paste gives each caret its piece back.
    cx.dispatch_action(editor::actions::Paste);
    cx.assert_editor_state("one twoˇ\nthree fourˇ\nfive sixˇ");
}

#[gpui::test]
async fn the_ring_keeps_the_newest_sixty_kills(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    for ix in 0..61 {
        cx.set_state(&format!("ˇkill{ix}"));
        cx.simulate_keystrokes("ctrl-k ctrl-f");
    }

    let texts = ring_texts(&mut cx);
    assert_eq!(texts.len(), 60);
    assert_eq!(texts.first().map(String::as_str), Some("kill60"));
    assert_eq!(texts.last().map(String::as_str), Some("kill1"));
}

#[gpui::test]
async fn outside_clipboard_text_becomes_its_own_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two");
    cx.simulate_keystrokes("alt-d");
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("outside".into())));
    cx.simulate_keystrokes("alt-d");

    assert_eq!(ring_texts(&mut cx), [" two", "outside", "one"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some(" two"));
}

#[gpui::test]
async fn a_masked_editor_never_kills_or_copies(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("kept".into())));
    cx.update_editor(|editor, _, cx| editor.set_masked(true, cx));
    cx.set_state("secret ˇvalue\nmore");

    cx.simulate_keystrokes("ctrl-k ctrl-w alt-w alt-d");
    cx.assert_editor_state("secret ˇvalue\nmore");

    // With an active region too.
    cx.set_state("«secretˇ» value");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.simulate_keystrokes("alt-w ctrl-w");
    cx.assert_editor_state("«secretˇ» value");

    assert_eq!(ring_texts(&mut cx), Vec::<String>::new());
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("kept"));

    // Motions still run.
    cx.set_state("ˇab");
    cx.simulate_keystrokes("ctrl-f");
    cx.assert_editor_state("aˇb");
}

/// Every `emacs` action that sends a kill is in `KILL_COMMANDS`, and every member sends one.
#[gpui::test]
async fn the_kill_command_set_matches_the_commands_that_kill(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);
    let state = "one tˇwo three\nfour five\nsix";

    let names = cx.update(|_, cx| {
        cx.all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("emacs::"))
            .collect::<Vec<_>>()
    });
    let mut actions = names
        .iter()
        .filter_map(|name| cx.update(|_, cx| cx.build_action(name, None).ok()))
        .collect::<Vec<_>>();
    // The zaps ask for their character when the action has none.
    actions.push(ZapToChar::with('e').boxed_clone());
    actions.push(ZapUpToChar::with('e').boxed_clone());
    actions.push(ZapBackToChar::with('o').boxed_clone());
    actions.push(ZapBackUpToChar::with('o').boxed_clone());

    for action in actions {
        cx.set_state(state);
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
    }

    let sent = kills
        .borrow()
        .iter()
        .map(|kill| kill.command)
        .collect::<BTreeSet<_>>();
    let expected = KILL_COMMANDS.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(sent, expected);
}

trait WithChar {
    fn with(char: char) -> Self;
}

macro_rules! with_char {
    ($($action:ident),*) => {
        $(impl WithChar for $action {
            fn with(char: char) -> Self {
                $action {
                    count: None,
                    char: Some(char),
                }
            }
        })*
    };
}

with_char!(ZapToChar, ZapUpToChar, ZapBackToChar, ZapBackUpToChar);
