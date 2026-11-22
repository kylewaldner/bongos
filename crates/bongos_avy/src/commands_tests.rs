use std::ops::Range;

use bongos_emacs::actions::UniversalArgument;
use bongos_emacs::mark::{mark_for, ring_for};
use bongos_minibuffer::{MESSAGES, minibuffer_for_window};
use bongos_window::test_support::{add_terminal, split_center};
use editor::{Anchor, Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    Action, BorrowAppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext,
    VisualTestContext,
};
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;

use crate::actions::{GotoCharTimer, GotoLine, GotoWord1};
use crate::overlay::AVY_OVERLAY_KEY;
use crate::session::is_running;
use crate::test_support::{add_text_editor, move_cursor, paint, setup_with};

const LEFT: &str = "alpha beta\nxy";
const RIGHT: &str = "gamma apple";

struct Fixture {
    workspace: Entity<Workspace>,
    left: Entity<Editor>,
    right: Entity<Editor>,
    cx: VisualTestContext,
}

/// Registers everything a window needs for the commands: the prefix argument, the handlers, the
/// echo area, and a key bound to the character jump.
fn init_all(cx: &mut gpui::App) {
    bongos_emacs::init(cx);
    crate::init(cx);
    bongos_minibuffer::init(cx);
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.minibuffer.get_or_insert_default().enabled_for =
                Some(vec![MESSAGES.to_string()]);
        });
    });
    cx.bind_keys([
        KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
        KeyBinding::new("alt-;", GotoCharTimer, Some("Editor")),
    ]);
}

/// Two editors side by side with the cursor of the left one at `cursor` and focus in it.
async fn fixture(cx: &mut TestAppContext, cursor: usize) -> Fixture {
    let (workspace, mut cx) = setup_with(cx, init_all).await;
    let (left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let left = add_text_editor(&workspace, &center[0], LEFT, window, cx);
        let right = add_text_editor(&workspace, &center[1], RIGHT, window, cx);
        move_cursor(&left, cursor, window, cx);
        window.focus(&left.focus_handle(cx), cx);
        (left, right)
    });
    paint(&mut cx);
    Fixture {
        workspace,
        left,
        right,
        cx,
    }
}

impl Fixture {
    fn dispatch(&mut self, action: impl Action) {
        self.cx.dispatch_action(action);
        self.cx.run_until_parked();
    }

    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }

    fn running(&mut self) -> bool {
        self.cx.update(|window, cx| is_running(window, cx))
    }

    /// Lets the pause after the last typed character pass, so the labels show.
    fn timeout(&mut self) {
        let timeout = self
            .cx
            .update(|_, cx| editor::EditorSettings::get_global(cx).avy_timeout);
        self.cx.executor().advance_clock(timeout);
        self.cx.run_until_parked();
    }

    fn labels(&mut self, editor: &Entity<Editor>) -> Vec<String> {
        self.cx.update(|_, cx| {
            editor
                .read(cx)
                .navigation_overlays(AVY_OVERLAY_KEY)
                .into_iter()
                .map(|overlay| overlay.label.text.to_string())
                .collect()
        })
    }

    fn selections(&mut self, editor: &Entity<Editor>) -> Vec<Range<usize>> {
        self.cx.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .all::<MultiBufferOffset>(&snapshot)
                    .into_iter()
                    .map(|selection| selection.start.0..selection.end.0)
                    .collect()
            })
        })
    }

    /// The mark and the ring (newest first) of `editor`'s buffer, as offsets.
    fn marks(&mut self, editor: &Entity<Editor>) -> (Option<usize>, Vec<usize>) {
        self.cx.update(|_, cx| {
            let offset = |anchor: Anchor, cx: &gpui::App| {
                let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
                anchor.to_offset(&snapshot).0
            };
            let mark = mark_for(editor, cx).map(|anchor| offset(anchor, cx));
            let ring = ring_for(editor, cx)
                .into_iter()
                .map(|anchor| offset(anchor, cx))
                .collect();
            (mark, ring)
        })
    }

    fn holds_focus(&mut self, editor: &Entity<Editor>) -> bool {
        self.cx
            .update(|window, cx| editor.focus_handle(cx).contains_focused(window, cx))
    }

    fn echo(&mut self) -> Option<String> {
        let strip = self
            .cx
            .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
            .expect("the window has a strip");
        strip.read_with(&self.cx, |strip, _| {
            strip.echo_line().map(|line| line.text.to_string())
        })
    }
}

#[gpui::test]
async fn goto_char_timer_jumps_to_the_chosen_match(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 0).await;
    f.dispatch(GotoCharTimer);
    assert!(f.running());
    f.keys("a");
    f.timeout();
    // Every "a" but the one under the cursor: two in the left pane, three in the right.
    let (left, right) = (f.left.clone(), f.right.clone());
    assert!(!f.labels(&left).is_empty());
    assert_eq!(f.labels(&left).len() + f.labels(&right).len(), 5);
    // The nearest match is the "a" closing "alpha".
    f.keys("a");
    assert!(!f.running());
    assert_eq!(f.selections(&left), [4..4]);
    assert!(f.holds_focus(&left));
    assert_eq!(f.marks(&left), (Some(0), vec![]));
    assert!(f.labels(&left).is_empty() && f.labels(&right).is_empty());
}

#[gpui::test]
async fn goto_word_1_labels_only_word_starts(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 6).await;
    f.dispatch(GotoWord1);
    f.keys("a");
    // "alpha" and "apple" start with "a"; no other "a" is a word start.
    let (left, right) = (f.left.clone(), f.right.clone());
    assert_eq!(f.labels(&left).len(), 1);
    assert_eq!(f.labels(&right).len(), 1);
    // The nearest start is "alpha", in the same pane.
    f.keys("a");
    assert!(!f.running());
    assert_eq!(f.selections(&left), [0..0]);
    assert_eq!(f.marks(&left), (Some(6), vec![]));
}

#[gpui::test]
async fn goto_line_reaches_the_other_pane_at_column_zero(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 0).await;
    let (left, right) = (f.left.clone(), f.right.clone());
    f.cx.update(|window, cx| move_cursor(&right, 3, window, cx));
    paint(&mut f.cx);
    f.dispatch(GotoLine);
    assert!(f.running());
    let labels = f.labels(&right);
    assert_eq!(labels.len(), 1);
    f.keys(&labels[0]);
    assert!(!f.running());
    assert!(f.holds_focus(&right));
    assert!(!f.holds_focus(&left));
    assert_eq!(f.selections(&right), [0..0]);
    assert_eq!(f.marks(&left), (Some(0), vec![]));
}

#[gpui::test]
async fn a_query_without_a_match_echoes_and_changes_nothing(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 3).await;
    let (left, right) = (f.left.clone(), f.right.clone());
    f.dispatch(GotoCharTimer);
    f.keys("z q");
    f.timeout();
    assert!(!f.running());
    assert_eq!(f.echo().as_deref(), Some("No matches for 'zq'"));
    assert_eq!(f.selections(&left), [3..3]);
    assert_eq!(f.selections(&right), [0..0]);
    assert_eq!(f.marks(&left), (None, vec![]));
    assert!(f.holds_focus(&left));
    assert!(f.labels(&left).is_empty() && f.labels(&right).is_empty());
}

#[gpui::test]
async fn the_actions_do_nothing_when_the_focus_is_not_in_an_editor(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 3).await;
    let (left, right) = (f.left.clone(), f.right.clone());
    let terminal = f.cx.update(|window, cx| {
        let center = f.workspace.read(cx).center_panes();
        let terminal = add_terminal(&f.workspace, &center[1], window, cx);
        window.focus(&terminal.focus_handle(cx), cx);
        terminal
    });
    paint(&mut f.cx);
    assert!(f.cx.update(|window, cx| terminal.focus_handle(cx).contains_focused(window, cx)));
    f.dispatch(GotoCharTimer);
    assert!(!f.running());
    f.dispatch(GotoWord1);
    assert!(!f.running());
    f.dispatch(GotoLine);
    assert!(!f.running());
    assert_eq!(f.selections(&left), [3..3]);
    assert_eq!(f.selections(&right), [0..0]);
    assert_eq!(f.marks(&left), (None, vec![]));
    assert_eq!(f.echo(), None);
}

#[gpui::test]
async fn a_prefix_argument_does_not_repeat_the_jump(cx: &mut TestAppContext) {
    let mut f = fixture(cx, 0).await;
    let left = f.left.clone();
    f.keys("ctrl-u alt-;");
    assert!(f.running());
    f.keys("a");
    f.timeout();
    f.keys("a");
    assert!(!f.running());
    // One jump: the mark is the start point and the ring holds no second push.
    assert_eq!(f.selections(&left), [4..4]);
    assert_eq!(f.marks(&left), (Some(0), vec![]));
    f.timeout();
    assert!(!f.running());
}
