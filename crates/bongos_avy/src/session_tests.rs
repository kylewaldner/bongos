use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use bongos_window::test_support::split_center;
use editor::{Editor, EditorSettings, MultiBufferOffset, ToOffset as _};
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use settings::Settings as _;
use workspace::Workspace;

use crate::overlay::AVY_OVERLAY_KEY;
use crate::session::{Outcome, cancel, init, is_running, start};
use crate::test_support::{add_text_editor, move_cursor, paint, setup};
use crate::visible::Mode;

/// The left pane's text: the line with matches, then enough empty lines to scroll.
fn left_text() -> String {
    format!("alpha beta\nxy{}", "\n".repeat(80))
}
const RIGHT: &str = "gamma";

type Log = Rc<RefCell<Vec<Outcome>>>;

struct Fixture<'a> {
    workspace: Entity<Workspace>,
    left: Entity<Editor>,
    right: Entity<Editor>,
    cx: &'a mut VisualTestContext,
}

/// Two editors side by side, focus and cursor at the start of the left one.
async fn fixture(cx: &mut TestAppContext) -> Fixture<'_> {
    let (workspace, cx) = setup(cx).await;
    let (left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let left = add_text_editor(&workspace, &center[0], &left_text(), window, cx);
        let right = add_text_editor(&workspace, &center[1], RIGHT, window, cx);
        move_cursor(&left, 0, window, cx);
        window.focus(&left.focus_handle(cx), cx);
        (left, right)
    });
    paint(cx);
    Fixture {
        workspace,
        left,
        right,
        cx,
    }
}

impl Fixture<'_> {
    /// Starts a jump of `mode` from the left editor, recording its outcome.
    fn start(&mut self, mode: Mode) -> Log {
        let log = Log::default();
        let recorded = log.clone();
        let origin = self.left.clone();
        let workspace = self.workspace.clone();
        self.cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                start(mode, origin, workspace, window, cx, move |outcome, _, _| {
                    recorded.borrow_mut().push(outcome)
                });
            })
        });
        log
    }

    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
    }

    fn running(&mut self) -> bool {
        self.cx.update(|window, cx| is_running(window, cx))
    }

    /// The label texts shown on `editor`.
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

    fn label_count(&mut self) -> usize {
        let (left, right) = (self.left.clone(), self.right.clone());
        self.labels(&left).len() + self.labels(&right).len()
    }

    /// The text, selections, scroll position and focus of every pane.
    fn state(&mut self) -> Vec<(String, Vec<Range<usize>>, gpui::Point<f64>, bool)> {
        let editors = [self.left.clone(), self.right.clone()];
        self.cx.update(|window, cx| {
            editors
                .iter()
                .map(|editor| {
                    let focused = editor.focus_handle(cx).is_focused(window);
                    editor.update(cx, |editor, cx| {
                        let snapshot = editor.display_snapshot(cx);
                        let selections = editor
                            .selections
                            .all::<MultiBufferOffset>(&snapshot)
                            .into_iter()
                            .map(|selection| selection.start.0..selection.end.0)
                            .collect();
                        let scroll = editor.scroll_position(cx);
                        (editor.text(cx), selections, scroll, focused)
                    })
                })
                .collect()
        })
    }

    fn timeout(&mut self) -> Duration {
        self.cx
            .update(|_, cx| EditorSettings::get_global(cx).avy_timeout)
    }

    fn advance(&mut self, by: Duration) {
        self.cx.executor().advance_clock(by);
        self.cx.run_until_parked();
    }

    /// Asserts the jump ended by a cancel and left nothing behind.
    fn assert_cancelled(&mut self, log: &Log) {
        assert!(!self.running());
        assert_eq!(self.label_count(), 0);
        assert!(log.borrow().is_empty());
    }
}

/// Where `candidate` starts in its editor's buffer, and that editor.
fn position(outcome: &Outcome, cx: &mut VisualTestContext) -> (Entity<Editor>, usize) {
    let Outcome::Jump(candidate) = outcome else {
        panic!("expected a jump, got {outcome:?}");
    };
    let editor = candidate.editor.upgrade().expect("the editor is open");
    let offset = cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        candidate.range.start.to_offset(&snapshot).0
    });
    (editor, offset)
}

#[gpui::test]
async fn typed_chars_are_not_inserted_and_labels_wait_for_the_timer(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let before = f.state();
    let log = f.start(Mode::CharTimer);
    assert!(f.running());
    f.keys("a");
    assert_eq!(f.state(), before);
    assert_eq!(f.label_count(), 0);

    let timeout = f.timeout();
    f.advance(timeout - Duration::from_millis(1));
    assert_eq!(f.label_count(), 0);
    f.advance(Duration::from_millis(1));
    // "a" in "alpha beta" (not the one under the cursor) and twice in "gamma".
    assert_eq!(f.label_count(), 4);
    assert_eq!(f.state(), before);
    assert!(log.borrow().is_empty());
    assert!(f.running());
}

#[gpui::test]
async fn enter_shows_the_labels_at_once(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let _log = f.start(Mode::CharTimer);
    f.keys("a enter");
    let mut all = {
        let (left, right) = (f.left.clone(), f.right.clone());
        let mut all = f.labels(&left);
        all.extend(f.labels(&right));
        all
    };
    all.sort();
    assert_eq!(all, ["a", "d", "f", "s"]);
}

#[gpui::test]
async fn backspace_filters_again(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let _log = f.start(Mode::CharTimer);
    f.keys("a m enter");
    // Only "gamma" has "am".
    let (left, right) = (f.left.clone(), f.right.clone());
    assert_eq!(f.labels(&left), Vec::<String>::new());
    assert_eq!(f.labels(&right), ["a"]);

    f.keys("backspace");
    assert_eq!(f.labels(&left).len(), 2);
    assert_eq!(f.labels(&right).len(), 2);
    assert!(f.running());
}

#[gpui::test]
async fn typing_a_label_jumps_to_its_candidate(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let before = f.state();
    let log = f.start(Mode::CharTimer);
    f.keys("a enter a");
    assert!(!f.running());
    assert_eq!(f.label_count(), 0);
    let outcomes = log.borrow().clone();
    assert_eq!(outcomes.len(), 1);
    // The nearest match is the "a" closing "alpha", on the cursor's line.
    let (editor, offset) = position(&outcomes[0], f.cx);
    assert_eq!(editor, f.left);
    assert_eq!(offset, 4);
    // The session itself moves nothing; the jump is the caller's.
    assert_eq!(f.state(), before);
}

#[gpui::test]
async fn ctrl_g_escape_and_other_keys_cancel_and_change_nothing(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    // Scroll and select first, so a cancel that reset them would show.
    let (left, right) = (f.left.clone(), f.right.clone());
    f.cx.update(|window, cx| {
        move_cursor(&left, 6, window, cx);
        move_cursor(&right, 2, window, cx);
        right.update(cx, |editor, cx| {
            editor.set_scroll_position(gpui::point(0., 0.), window, cx)
        });
    });
    paint(f.cx);
    let before = f.state();

    for key in ["ctrl-g", "escape", "ctrl-f"] {
        // While reading.
        let log = f.start(Mode::CharTimer);
        f.keys("a");
        f.keys(key);
        f.assert_cancelled(&log);
        assert_eq!(f.state(), before, "{key} while reading");

        // While labels show.
        let log = f.start(Mode::CharTimer);
        f.keys("a enter");
        assert!(f.label_count() > 0);
        f.keys(key);
        paint(f.cx);
        f.assert_cancelled(&log);
        assert_eq!(f.state(), before, "{key} with labels");

        // No timer of the ended jump brings its labels back.
        let timeout = f.timeout();
        f.advance(timeout * 2);
        assert_eq!(f.label_count(), 0);
        assert!(log.borrow().is_empty());
    }

    // The C-g hook cancels too, and does nothing with no jump running.
    f.cx.update(|_, cx| {
        init(cx);
        init(cx);
    });
    let log = f.start(Mode::CharTimer);
    f.keys("a enter");
    f.cx.update(|window, cx| bongos_emacs::quit::run_hooks_once(window, cx));
    f.cx.run_until_parked();
    f.assert_cancelled(&log);
    assert_eq!(f.state(), before);
    f.cx.update(|window, cx| bongos_emacs::quit::run_hooks_once(window, cx));
    f.cx.run_until_parked();
    assert!(!f.cx.update(|window, cx| cancel(window, cx)));
    assert_eq!(f.state(), before);

    // Nothing is left listening: the next key reaches the focused buffer as usual.
    f.keys("z");
    assert_eq!(
        f.state()[0].0,
        format!("alpha zbeta\nxy{}", "\n".repeat(80))
    );
}

#[gpui::test]
async fn focus_out_scrolling_and_deactivation_cancel(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let (left, right) = (f.left.clone(), f.right.clone());

    let log = f.start(Mode::CharTimer);
    f.keys("a enter");
    assert!(f.label_count() > 0);
    f.cx.update(|window, cx| window.focus(&right.focus_handle(cx), cx));
    f.cx.run_until_parked();
    f.assert_cancelled(&log);

    f.cx.update(|window, cx| window.focus(&left.focus_handle(cx), cx));
    f.cx.run_until_parked();
    let log = f.start(Mode::CharTimer);
    f.keys("a enter");
    assert!(f.label_count() > 0);
    f.cx.update(|window, cx| {
        left.update(cx, |editor, cx| {
            editor.set_scroll_position(gpui::point(0., 3.), window, cx)
        })
    });
    f.cx.run_until_parked();
    f.assert_cancelled(&log);

    let log = f.start(Mode::CharTimer);
    f.keys("a");
    f.cx.deactivate_window();
    f.cx.run_until_parked();
    f.assert_cancelled(&log);
}

#[gpui::test]
async fn an_edit_to_a_labelled_buffer_cancels(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let right = f.right.clone();
    let log = f.start(Mode::CharTimer);
    f.keys("a enter");
    assert!(!f.labels(&right).is_empty());
    f.cx.update(|_, cx| {
        right.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(0), "x")],
                    None,
                    cx,
                )
            })
        })
    });
    f.cx.run_until_parked();
    f.assert_cancelled(&log);
}

#[gpui::test]
async fn a_timer_firing_after_a_cancel_does_nothing(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let before = f.state();
    let log = f.start(Mode::CharTimer);
    f.keys("a");
    f.keys("escape");
    let timeout = f.timeout();
    f.advance(timeout * 2);
    f.assert_cancelled(&log);
    assert_eq!(f.state(), before);

    // A new start replaces a running jump, whose timer then does nothing either.
    let first = f.start(Mode::CharTimer);
    f.keys("a");
    let second = f.start(Mode::CharTimer);
    assert!(f.running());
    f.advance(timeout * 2);
    assert_eq!(f.label_count(), 0);
    f.keys("a enter");
    assert_eq!(f.label_count(), 4);
    f.keys("a");
    assert!(first.borrow().is_empty());
    assert_eq!(second.borrow().len(), 1);
}

#[gpui::test]
async fn a_new_start_clears_the_labels_of_the_first(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let first = f.start(Mode::CharTimer);
    f.keys("a enter");
    assert_eq!(f.label_count(), 4);
    let _second = f.start(Mode::CharTimer);
    assert_eq!(f.label_count(), 0);
    assert!(f.running());
    assert!(first.borrow().is_empty());
}

#[gpui::test]
async fn no_visible_editor_starts_nothing(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let called = Rc::new(RefCell::new(false));
    let flag = called.clone();
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let editor = cx.new(|cx| Editor::single_line(window, cx));
            start(Mode::Line, editor, workspace, window, cx, move |_, _, _| {
                *flag.borrow_mut() = true
            });
        })
    });
    assert!(!cx.update(|window, cx| is_running(window, cx)));
    cx.simulate_keystrokes("a");
    assert!(!*called.borrow());
}

#[gpui::test]
async fn no_candidates_reports_the_query(cx: &mut TestAppContext) {
    let mut f = fixture(cx).await;
    let before = f.state();
    let log = f.start(Mode::CharTimer);
    f.keys("q enter");
    assert!(!f.running());
    assert_eq!(f.label_count(), 0);
    assert!(matches!(&log.borrow()[..], [Outcome::NoMatches(query)] if query == "q"));
    assert_eq!(f.state(), before);
}
