use editor::test::editor_test_context::EditorTestContext;
use gpui::{KeyBinding, TestAppContext};

use crate::actions::{
    BackwardWord, DigitArgument, ForwardWord, KillLine, UniversalArgument, Yank, YankPop,
};
use crate::apply_tests::init_test;
use crate::last_command;
use editor::kill_ring;

fn last(cx: &mut EditorTestContext) -> Option<&'static str> {
    cx.update(|window, cx| last_command(window, cx))
}

#[gpui::test]
async fn last_command_follows_keystrokes_and_dispatches(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new(
            "alt-f",
            ForwardWord::default(),
            Some("Editor"),
        )])
    });
    cx.set_state("ˇone two");
    assert_eq!(last(&mut cx), None);

    cx.simulate_keystrokes("alt-f");
    cx.assert_editor_state("oneˇ two");
    assert_eq!(last(&mut cx), Some("emacs::ForwardWord"));

    // A key with no action clears it.
    cx.simulate_keystrokes("x");
    assert_eq!(last(&mut cx), None);

    // A dispatch by name counts, as from the palette.
    cx.dispatch_action(BackwardWord::default());
    assert_eq!(last(&mut cx), Some("emacs::BackwardWord"));
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

#[gpui::test]
async fn the_last_command_after_a_prefix_is_the_one_before_it(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("alt-f", ForwardWord::default(), Some("Editor")),
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
        ])
    });
    cx.set_state("ˇone two");
    cx.simulate_keystrokes("alt-f");
    cx.simulate_keystrokes("ctrl-u");
    assert_eq!(last(&mut cx), Some("emacs::ForwardWord"));
    cx.dispatch_action(DigitArgument { digit: 2 });
    assert_eq!(last(&mut cx), Some("emacs::ForwardWord"));
}

#[gpui::test]
async fn a_prefix_between_yank_and_yank_pop_keeps_the_chain(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-y", Yank::default(), Some("Editor")),
            KeyBinding::new("alt-y", YankPop::default(), Some("Editor")),
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
        ]);
        for text in ["oldest", "newest"] {
            kill_ring::update(cx, |ring| {
                ring.push(bongos_emacs_model::kill_ring::Entry::plain(
                    text.to_string(),
                ))
            });
        }
    });
    cx.set_state("<ˇ>");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("<newestˇ>");
    cx.simulate_keystrokes("ctrl-u");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("<oldestˇ>");
}

#[gpui::test]
async fn a_prefix_between_kills_still_joins_them(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-k", KillLine::default(), Some("Editor")),
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
        ])
    });
    cx.set_state("ˇone\ntwo\nthree");
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_keystrokes("ctrl-u");
    cx.dispatch_action(DigitArgument { digit: 2 });
    cx.simulate_keystrokes("ctrl-k");
    assert_eq!(ring_texts(&mut cx).len(), 1);
}
