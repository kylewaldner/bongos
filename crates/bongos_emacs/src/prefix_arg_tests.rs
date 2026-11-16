use bongos_emacs_model::prefix_arg::PrefixArg;
use bongos_minibuffer::{KEY_HINT, minibuffer_for_window};
use editor::Editor;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{
    Action as _, AppContext as _, BorrowAppContext as _, Entity, Focusable as _, TestAppContext,
    VisualTestContext,
};
use settings::SettingsStore;

use crate::actions::{
    DigitArgument, DuplicateLine, KeyboardQuit, KillWord, NegativeArgument, UniversalArgument,
};
use crate::apply_tests::init_test;
use crate::prefix_arg::{self, MAX_INSERT, Meaning};
use crate::quit_tests::workspace_window_with;

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
        assert!(matches!(
            prefix_arg::meaning(cx, KeyboardQuit.name()),
            Some(Meaning::Ignored)
        ));
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
    let (_workspace, editor, mut cx) = crate::quit_tests::workspace_window(cx).await;
    cx.update(|window, cx| prefix_arg::start(digit(3), &editor, window, cx));
    assert_eq!(
        cx.update(|window, cx| prefix_arg::pending(window, cx)),
        Some(digit(3))
    );
    cx.update(|window, _| window.blur());
    cx.run_until_parked();
    assert_eq!(
        cx.update(|window, cx| prefix_arg::pending(window, cx)),
        None
    );
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

/// Routes the prefix echo to the strip of each window.
pub(crate) fn route_echo(cx: &mut gpui::App) {
    bongos_minibuffer::init(cx);
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.minibuffer.get_or_insert_default().enabled_for =
                Some(vec![KEY_HINT.to_string()]);
        });
    });
}

/// A workspace window whose strip shows the prefix echo, with the editor focused.
async fn echo_window(cx: &mut TestAppContext) -> (Entity<Editor>, VisualTestContext) {
    let (_workspace, editor, cx) = workspace_window_with(cx, route_echo).await;
    (editor, cx)
}

pub(crate) fn echo(cx: &mut VisualTestContext) -> Option<String> {
    cx.run_until_parked();
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    strip.read_with(cx, |strip, _| {
        strip.hint_text().map(|text| text.to_string())
    })
}

fn pending_in(cx: &mut VisualTestContext) -> Option<PrefixArg> {
    cx.update(|window, cx| prefix_arg::pending(window, cx))
}

#[gpui::test]
async fn two_universal_arguments_make_sixteen_and_echo_it(cx: &mut TestAppContext) {
    let (_editor, mut cx) = echo_window(cx).await;
    cx.dispatch_action(UniversalArgument);
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 4"));
    cx.dispatch_action(UniversalArgument);
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 16"));
    assert_eq!(pending_in(&mut cx).map(|arg| arg.value()), Some(16));
}

#[gpui::test]
async fn digit_and_negative_arguments_start_and_echo(cx: &mut TestAppContext) {
    let (_editor, mut cx) = echo_window(cx).await;
    cx.dispatch_action(DigitArgument { digit: 3 });
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 3"));
    cx.dispatch_action(DigitArgument { digit: 5 });
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 35"));
    cx.update(|window, cx| prefix_arg::cancel(window, cx));
    assert_eq!(echo(&mut cx), None);
    cx.dispatch_action(NegativeArgument);
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u -"));
    assert_eq!(pending_in(&mut cx).map(|arg| arg.value()), Some(-1));
}

#[gpui::test]
async fn quit_clears_the_argument_and_the_echo_and_edits_nothing(cx: &mut TestAppContext) {
    let (editor, mut cx) = echo_window(cx).await;
    cx.dispatch_action(UniversalArgument);
    assert_eq!(echo(&mut cx).as_deref(), Some("C-u 4"));
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(pending_in(&mut cx), None);
    assert_eq!(echo(&mut cx), None);
    editor.read_with(&cx, |editor, cx| assert_eq!(editor.text(cx), "one two"));
}

#[gpui::test]
async fn a_single_line_editor_propagates_and_sets_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx) = {
        let (workspace, _editor, cx) = workspace_window_with(cx, |_| {}).await;
        (workspace, cx)
    };
    let reached = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = reached.clone();
    workspace.update_in(&mut cx, |workspace, window, cx| {
        let single = cx.new(|cx| Editor::single_line(window, cx));
        workspace.add_item_to_active_pane(Box::new(single.clone()), None, true, window, cx);
        window.focus(&single.focus_handle(cx), cx);
        workspace.register_action(move |_, _: &UniversalArgument, _, _| seen.set(seen.get() + 1));
    });
    cx.run_until_parked();
    cx.dispatch_action(UniversalArgument);
    assert_eq!(reached.get(), 1);
    assert_eq!(pending_in(&mut cx), None);
}
