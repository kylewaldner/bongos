use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action as _, Focusable as _, TestAppContext};

use crate::actions::{DuplicateLine, KeyboardQuit, KillWord};
use crate::apply_tests::init_test;
use crate::prefix_arg::{self, MAX_INSERT, Meaning};

fn start(cx: &mut EditorTestContext, arg: PrefixArg) {
    let editor = cx.editor.clone();
    cx.update(|window, cx| prefix_arg::start(arg, &editor, window, cx));
}

fn hand_off(cx: &mut EditorTestContext) {
    cx.update(|window, cx| prefix_arg::hand_off(window, cx));
}

/// Hands the argument off and dispatches in one step, as the interceptor and the key's binding
/// do, so the deferred drop of an unread handoff runs after the command.
fn hand_off_and_dispatch(cx: &mut EditorTestContext, action: impl gpui::Action) {
    let editor = cx.editor.clone();
    cx.update(|window, cx| {
        prefix_arg::hand_off(window, cx);
        editor.focus_handle(cx).dispatch_action(&action, window, cx)
    });
}

fn pending(cx: &mut EditorTestContext) -> Option<PrefixArg> {
    cx.update(|window, cx| prefix_arg::pending(window, cx))
}

fn handoff_is_gone(cx: &mut EditorTestContext) -> bool {
    cx.update(|window, cx| prefix_arg::take(window, cx))
        .is_none()
}

fn digit(n: u8) -> PrefixArg {
    PrefixArg::digit(n)
}

#[gpui::test]
async fn handed_off_argument_becomes_the_count_and_is_gone(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone two three four five");
    start(&mut cx, digit(3));
    assert_eq!(pending(&mut cx), Some(digit(3)));
    hand_off_and_dispatch(&mut cx, KillWord { count: None });
    assert_eq!(pending(&mut cx), None);
    cx.assert_editor_state("ˇ four five");
    assert!(handoff_is_gone(&mut cx));
}

#[gpui::test]
async fn explicit_count_wins_and_the_handoff_is_cleared(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone two three four five");
    start(&mut cx, digit(3));
    hand_off_and_dispatch(&mut cx, KillWord { count: Some(1) });
    cx.assert_editor_state("ˇ two three four five");
    assert!(handoff_is_gone(&mut cx));
}

#[gpui::test]
async fn ignored_action_runs_once_and_clears_the_handoff(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone");
    cx.update(|_, cx| {
        assert_eq!(
            prefix_arg::meaning(cx, KeyboardQuit.name()),
            Some(Meaning::Ignored)
        );
    });
    start(&mut cx, digit(5));
    hand_off_and_dispatch(&mut cx, KeyboardQuit);
    assert!(handoff_is_gone(&mut cx));
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn unhandled_handoff_is_dropped_after_dispatch(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    start(&mut cx, digit(2));
    hand_off(&mut cx);
    cx.run_until_parked();
    assert!(handoff_is_gone(&mut cx));

    // A drop scheduled for an older handoff does not clear a newer one.
    let editor = cx.editor.clone();
    cx.update(|window, cx| {
        prefix_arg::start(digit(2), &editor, window, cx);
        prefix_arg::hand_off(window, cx);
        prefix_arg::start(digit(4), &editor, window, cx);
        prefix_arg::hand_off(window, cx);
    });
    cx.run_until_parked();
    assert!(handoff_is_gone(&mut cx));
}

#[gpui::test]
async fn blur_of_the_origin_editor_clears_the_argument(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_editor(|editor, window, cx| window.focus(&editor.focus_handle(cx), cx));
    start(&mut cx, digit(3));
    assert_eq!(pending(&mut cx), Some(digit(3)));
    cx.update(|window, cx| {
        window.blur();
        window.refresh();
    });
    cx.run_until_parked();
    assert_eq!(pending(&mut cx), None);
}

#[gpui::test]
async fn cancel_clears_argument_and_handoff(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    start(&mut cx, digit(3));
    cx.update(|window, cx| prefix_arg::cancel(window, cx));
    assert_eq!(pending(&mut cx), None);
    assert!(handoff_is_gone(&mut cx));
}

#[gpui::test]
async fn inserting_counts_are_clamped(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let mut arg = PrefixArg::digit(1);
    for _ in 0..5 {
        arg.press_digit(0);
    }
    assert_eq!(arg.value(), 100_000);
    let clamped = prefix_arg::counted(&DuplicateLine { count: None }, Some(arg));
    assert_eq!(
        clamped,
        Some(DuplicateLine {
            count: Some(MAX_INSERT)
        })
    );
    // Kills keep the full value.
    let kept = prefix_arg::counted(&KillWord { count: None }, Some(arg));
    assert_eq!(
        kept,
        Some(KillWord {
            count: Some(100_000)
        })
    );
    cx.set_state("ˇone");
}

#[gpui::test]
async fn every_emacs_action_is_declared(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        let names: Vec<&str> = cx
            .all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("emacs::"))
            .collect();
        assert!(!names.is_empty());
        for name in names {
            assert!(
                matches!(
                    prefix_arg::meaning(cx, name),
                    Some(Meaning::Reads | Meaning::Ignored)
                ),
                "{name} is not declared"
            );
        }
    });
}
