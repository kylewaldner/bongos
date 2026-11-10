use editor::test::editor_test_context::EditorTestContext;
use gpui::{KeyBinding, TestAppContext};

use crate::actions::{BackwardWord, ForwardWord};
use crate::apply_tests::init_test;
use crate::last_command;

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
