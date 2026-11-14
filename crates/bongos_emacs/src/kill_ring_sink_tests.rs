use std::collections::BTreeSet;

use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorMode, kill_ring};
use gpui::{
    Action, AppContext as _, ClipboardItem, KeyBinding, Modifiers, MouseButton, TestAppContext,
};
use multi_buffer::MultiBuffer;
use std::cell::Cell;
use std::rc::Rc;
use workspace::MinibufferRouting;

use crate::actions::{
    AppendNextKill, BackwardKillWord, CopyRegionOrLine, KillLine, KillRegionOrLine, KillWholeLine,
    KillWord, ZapBackToChar, ZapBackUpToChar, ZapToChar, ZapUpToChar,
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
            KeyBinding::new("ctrl-alt-k", KillWholeLine::default(), Some("Editor")),
            KeyBinding::new("ctrl-alt-w", AppendNextKill, Some("Editor")),
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

fn secrets(cx: &mut EditorTestContext) -> Vec<bool> {
    cx.update(|_, cx| {
        kill_ring::read(cx).map_or(Vec::new(), |ring| {
            (0..ring.len())
                .filter_map(|ix| ring.get(ix).map(|entry| entry.secret))
                .collect()
        })
    })
}

#[gpui::test]
async fn an_outside_copy_between_two_kill_lines_splits_the_sequence(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");
    cx.simulate_keystrokes("ctrl-k");
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("outside".into())));
    cx.simulate_keystrokes("ctrl-k");

    // The second kill is not appended to the outside text nor to the first kill.
    cx.assert_editor_state("ˇthree");
    assert_eq!(ring_texts(&mut cx), ["two\n", "outside", "one\n"]);
    assert_eq!(secrets(&mut cx), [false, false, false]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("two\n"));

    // With no outside copy the sequence goes on and joins.
    cx.simulate_keystrokes("ctrl-k");
    assert_eq!(ring_texts(&mut cx), ["two\nthree", "outside", "one\n"]);
}

#[gpui::test]
async fn a_concealed_outside_copy_is_secret_but_the_kill_after_it_is_not(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");
    cx.simulate_keystrokes("ctrl-k");
    cx.write_concealed_to_clipboard(ClipboardItem::new_string("hunter2".into()));
    cx.simulate_keystrokes("ctrl-k");

    assert_eq!(ring_texts(&mut cx), ["two\n", "hunter2", "one\n"]);
    assert_eq!(secrets(&mut cx), [false, true, false]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("two\n"));

    // The next kill joins the unmarked one and nothing becomes secret.
    cx.simulate_keystrokes("ctrl-k");
    assert_eq!(ring_texts(&mut cx), ["two\nthree", "hunter2", "one\n"]);
    assert_eq!(secrets(&mut cx), [false, true, false]);
}

#[gpui::test]
async fn clipboard_text_equal_to_the_newest_entry_is_not_pushed_before_a_kill(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");
    cx.simulate_keystrokes("ctrl-k");
    // Another app copied the same text the ring already has.
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("one\n".into())));
    cx.simulate_keystrokes("ctrl-k");

    // Not captured a second time, so the kill still joins the first one.
    assert_eq!(ring_texts(&mut cx), ["one\ntwo\n"]);
}

#[gpui::test]
async fn ignored_clipboard_text_is_skipped_before_a_kill(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");
    cx.simulate_keystrokes("ctrl-k");
    cx.update(|_, cx| {
        kill_ring::ignore_clipboard_text("private", cx);
        cx.write_to_clipboard(ClipboardItem::new_string("private".into()));
    });
    cx.simulate_keystrokes("ctrl-k");

    assert_eq!(ring_texts(&mut cx), ["one\ntwo\n"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("one\ntwo\n"));
}

#[gpui::test]
async fn an_editor_copy_from_outside_keeps_its_pieces_before_a_kill(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("«one ˇ»two «three ˇ»four");
    cx.simulate_keystrokes("alt-w");
    let copied = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    // The ring forgets it; the clipboard item, as another window would write it, comes back.
    cx.update(|_, cx| {
        kill_ring::update(cx, |ring| *ring = Default::default());
        cx.write_to_clipboard(copied);
    });
    cx.set_state("ˇfive");
    cx.simulate_keystrokes("ctrl-k");

    assert_eq!(ring_texts(&mut cx), ["five", "one \nthree "]);
    let pieces = cx.update(|_, cx| {
        let ring = kill_ring::read(cx).unwrap();
        let entry = ring.get(1).unwrap();
        entry
            .piece_texts()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    assert_eq!(pieces, ["one ", "three "]);
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

#[gpui::test]
async fn a_kill_whole_line_joins_the_kill_sequence(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone\ntwo\nthree");

    cx.simulate_keystrokes("ctrl-alt-k ctrl-alt-k");

    cx.assert_editor_state("ˇthree");
    assert_eq!(ring_texts(&mut cx), ["one\ntwo\n"]);
}

#[gpui::test]
async fn append_next_kill_joins_a_region_kill_and_a_line_kill(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("«one twoˇ» three\nfour");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));

    cx.simulate_keystrokes("ctrl-w ctrl-alt-w ctrl-k");

    // C-k at a line start takes the newline too.
    cx.assert_editor_state("ˇfour");
    assert_eq!(ring_texts(&mut cx), ["one two three\n"]);
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("one two three\n"));
}

#[gpui::test]
async fn append_next_kill_keeps_a_word_kill_sequence_joined(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two three");

    cx.simulate_keystrokes("alt-d ctrl-alt-w alt-d");

    assert_eq!(ring_texts(&mut cx), ["one two"]);
}

#[gpui::test]
async fn append_next_kill_then_a_motion_does_not_join(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two three");

    cx.simulate_keystrokes("alt-d ctrl-alt-w ctrl-f ctrl-k");

    cx.assert_editor_state(" ˇ");
    assert_eq!(ring_texts(&mut cx), ["two three", "one"]);
}

#[gpui::test]
async fn append_next_kill_twice_echoes_once_and_still_joins(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let echoes = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        let echoes = echoes.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                assert_eq!(text, "If the next command is a kill, it will append");
                echoes.set(echoes.get() + 1);
                true
            })));
    });
    cx.set_state("ˇone two three");

    cx.simulate_keystrokes("alt-d ctrl-alt-w ctrl-alt-w alt-d");

    assert_eq!(echoes.get(), 1);
    assert_eq!(ring_texts(&mut cx), ["one two"]);
}

#[gpui::test]
async fn append_next_kill_joins_a_kill_in_another_editor(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇone two");
    cx.simulate_keystrokes("alt-d");

    let buffer = cx.multibuffer(|multibuffer, _| multibuffer.as_singleton().unwrap());
    let second = cx.update(|window, cx| {
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx))
    });
    cx.simulate_keystrokes("ctrl-alt-w");
    cx.update(|window, cx| {
        kill::kill_line_command(&KillLine::default(), &second, window, cx);
        last_command::record(window, Some("emacs::KillLine"), cx);
    });

    assert_eq!(ring_texts(&mut cx), ["one two"]);
}

#[gpui::test]
async fn append_next_kill_after_a_copy_appends_to_the_copied_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("«oneˇ» two\nthree");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));

    cx.simulate_keystrokes("alt-w ctrl-alt-w ctrl-k");

    assert_eq!(ring_texts(&mut cx), ["one two"]);
}

#[gpui::test]
async fn append_next_kill_after_a_click_appends_to_the_newest_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("oˇne\ntwo\nthree");

    cx.simulate_keystrokes("ctrl-k");
    let position = cx.pixel_position("o\ntˇwo\nthree");
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("ctrl-alt-w ctrl-k");

    assert_eq!(ring_texts(&mut cx), ["newo"]);
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
