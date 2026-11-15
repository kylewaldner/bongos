//! Tests of the starters and of `replace-string` and `replace-regexp`: the read-only refusal, the
//! collapse to one cursor, the mark at the origin, the region as the scope, `Nothing to replace`,
//! the end message, the single undo step and `C-g` in the middle of a batch. The prompts are
//! answered by typing in them; the keys are the real rows of `specific-overrides.json`.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use bongos_emacs::actions::PopMark;
use bongos_emacs::mark::mark_for;
use bongos_minibuffer::SessionView;
use editor::Editor;
use gpui::{
    Action as _, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
};
use multi_buffer::{MultiBufferOffset, ToOffset as _};
use project::Project;
use settings::KeymapFile;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::actions::{QueryReplace, ReplaceRegexp, ReplaceString};
use crate::engine::SCAN_CHUNK;
use crate::session::batch_running;

pub(crate) type Messages = Rc<RefCell<Vec<String>>>;

pub(crate) struct Scene {
    pub(crate) workspace: Entity<Workspace>,
    pub(crate) editor: Entity<Editor>,
    pub(crate) messages: Messages,
    pub(crate) cx: VisualTestContext,
}

pub(crate) async fn scene(text: &str, cx: &mut TestAppContext) -> Scene {
    scene_with(text, |_| {}, cx).await
}

/// A scene whose app also runs `init` once the query replace feature is set up.
pub(crate) async fn scene_with(
    text: &str,
    init: impl FnOnce(&mut gpui::App),
    cx: &mut TestAppContext,
) -> Scene {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_emacs::init(cx);
        crate::init(cx);
        init(cx);
        cx.default_global::<MinibufferRouting>()
            .set_routes(vec![crate::prompts::HOST.to_string()]);
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx)
                .expect("the overrides keymap loads");
        cx.bind_keys(bindings);
        app_state
    });
    let messages = Messages::default();
    cx.update(|cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let text = text.to_string();
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&text, None, false, cx)
        });
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    Scene {
        workspace,
        editor,
        messages,
        cx,
    }
}

impl Scene {
    pub(crate) fn select(&mut self, ranges: &[Range<usize>]) {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(
                    ranges
                        .iter()
                        .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end)),
                )
            })
        });
        self.cx.run_until_parked();
    }

    pub(crate) fn text(&mut self) -> String {
        self.editor
            .read_with(&self.cx, |editor, cx| editor.text(cx))
    }

    /// The selections as offsets, oldest first.
    pub(crate) fn selections(&mut self) -> Vec<Range<usize>> {
        self.editor.update(&mut self.cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            editor
                .selections
                .all::<MultiBufferOffset>(&display)
                .into_iter()
                .map(|selection| selection.start.0..selection.end.0)
                .collect()
        })
    }

    pub(crate) fn mark(&mut self) -> Option<usize> {
        let editor = self.editor.clone();
        self.cx.update(|_, cx| {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            mark_for(&editor, cx).map(|mark| mark.to_offset(&snapshot).0)
        })
    }

    pub(crate) fn prompt_open(&mut self) -> bool {
        self.workspace.read_with(&self.cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx).is_some()
        })
    }

    pub(crate) fn messages(&self) -> Vec<String> {
        self.messages.borrow().clone()
    }

    pub(crate) fn type_answer(&mut self, text: &str) {
        self.cx.simulate_input(text);
        self.cx.run_until_parked();
    }

    /// Runs `action` and answers both prompts.
    pub(crate) fn replace(&mut self, action: impl gpui::Action, from: &str, to: &str) {
        self.cx.dispatch_action(action);
        assert!(self.prompt_open(), "the FROM prompt opens");
        self.type_answer(from);
        self.cx.dispatch_action(minibuffer::Confirm);
        self.type_answer(to);
        self.cx.dispatch_action(minibuffer::Confirm);
        self.cx.run_until_parked();
    }

    pub(crate) fn undo(&mut self) {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            editor.undo(&editor::actions::Undo, window, cx)
        });
        self.cx.run_until_parked();
    }
}

#[gpui::test]
async fn a_read_only_buffer_refuses_with_a_message(cx: &mut TestAppContext) {
    let mut scene = scene("foo bar foo\n", cx).await;
    scene
        .editor
        .update(&mut scene.cx, |editor, _| editor.set_read_only(true));
    scene.cx.dispatch_action(ReplaceString);
    assert!(!scene.prompt_open(), "nothing opens");
    scene.cx.dispatch_action(QueryReplace);
    assert!(!scene.prompt_open(), "nothing opens");
    assert_eq!(
        scene.messages(),
        vec!["Buffer is read-only", "Buffer is read-only"]
    );
    assert_eq!(scene.text(), "foo bar foo\n");
}

#[gpui::test]
async fn three_cursors_collapse_to_the_primary_one(cx: &mut TestAppContext) {
    let mut scene = scene("foo foo foo foo\n", cx).await;
    // The last range is the newest selection, the primary cursor.
    scene.select(&[0..0, 12..12, 4..4]);
    assert_eq!(scene.selections().len(), 3);
    scene.cx.dispatch_action(ReplaceString);
    assert_eq!(scene.selections(), vec![4..4]);
    scene.type_answer("foo");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.type_answer("x");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    // From the primary cursor only: the first `foo` stays.
    assert_eq!(scene.text(), "foo x x x\n");
    assert_eq!(scene.selections(), vec![9..9]);
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
}

#[gpui::test]
async fn popping_the_mark_after_replace_string_returns_to_the_origin(cx: &mut TestAppContext) {
    let mut scene = scene("a foo b foo c\n", cx).await;
    scene.select(&[1..1]);
    scene.replace(ReplaceString, "foo", "quux");
    assert_eq!(scene.text(), "a quux b quux c\n");
    // The cursor is at the end of the last replacement.
    assert_eq!(scene.selections(), vec![13..13]);
    assert_eq!(scene.mark(), Some(1));
    scene.cx.dispatch_action(PopMark);
    assert_eq!(scene.selections(), vec![1..1]);
}

#[gpui::test]
async fn a_region_over_lines_bounds_replace_string(cx: &mut TestAppContext) {
    let original = "foo\nfoo\nfoo\nfoo\n";
    let mut scene = scene(original, cx).await;
    // The second and third lines.
    scene.select(&[4..11]);
    scene.replace(ReplaceString, "foo", "x");
    assert_eq!(scene.text(), "foo\nx\nx\nfoo\n");
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
    // The region is deactivated: one empty cursor at the end of the last replacement.
    assert_eq!(scene.selections(), vec![7..7]);
    scene.editor.read_with(&scene.cx, |editor, _| {
        assert!(!editor.selection_mark_mode())
    });
    // One undo step.
    scene.undo();
    assert_eq!(scene.text(), original);
}

#[gpui::test]
async fn replace_regexp_expands_groups(cx: &mut TestAppContext) {
    let mut scene = scene("a1 b2 c3\n", cx).await;
    scene.select(&[0..0]);
    scene.replace(ReplaceRegexp, r"(\w)(\d)", r"\2\1");
    assert_eq!(scene.text(), "1a 2b 3c\n");
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
}

#[gpui::test]
async fn no_match_says_nothing_to_replace_and_pushes_no_mark(cx: &mut TestAppContext) {
    let mut scene = scene("alpha beta\n", cx).await;
    scene.select(&[2..2]);
    scene.replace(ReplaceString, "zzz", "y");
    assert_eq!(scene.text(), "alpha beta\n");
    assert_eq!(scene.messages(), vec!["Nothing to replace"]);
    assert_eq!(scene.mark(), None);
}

#[gpui::test]
async fn query_replace_opens_the_decision_view_on_the_first_match(cx: &mut TestAppContext) {
    let mut scene = scene("x foo foo\n", cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, "foo", "bar");
    assert_eq!(scene.text(), "x foo foo\n");
    // The cursor is at the end of the match, with no selection.
    assert_eq!(scene.selections(), vec![5..5]);
    assert_eq!(scene.mark(), Some(0));
    assert!(scene.workspace.read_with(&scene.cx, |workspace, cx| {
        workspace
            .active_modal::<crate::view::QueryReplaceView>(cx)
            .is_some()
    }));
}

#[gpui::test]
async fn ctrl_g_interrupts_a_batch_and_keeps_the_completed_chunks(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + SCAN_CHUNK / 2;
    let original = "foo\n".repeat(lines);
    let mut scene = scene(&original, cx).await;
    scene.select(&[0..0]);
    scene.cx.dispatch_action(ReplaceString);
    scene.type_answer("foo");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.type_answer("bar");
    // Confirms without running the batch, then runs it one task at a time until the first chunk
    // is in.
    scene
        .cx
        .update(|window, cx| window.dispatch_action(minibuffer::Confirm.boxed_clone(), cx));
    let mut ticks = 0;
    while scene.text().matches("bar").count() < SCAN_CHUNK {
        assert!(ticks < 1_000, "the first chunk never landed");
        scene.cx.executor().tick();
        ticks += 1;
    }
    assert_eq!(scene.text().matches("bar").count(), SCAN_CHUNK);
    scene
        .editor
        .read_with(&scene.cx, |editor, _| assert!(batch_running(editor)));

    scene.cx.simulate_keystrokes("ctrl-g");
    scene.cx.run_until_parked();
    let text = scene.text();
    assert_eq!(text.matches("bar").count(), SCAN_CHUNK);
    assert_eq!(text.matches("foo").count(), lines - SCAN_CHUNK);
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {SCAN_CHUNK} occurrences (interrupted)")]
    );
    scene
        .editor
        .read_with(&scene.cx, |editor, _| assert!(!batch_running(editor)));
    // The cursor is at the end of the last replacement.
    assert_eq!(
        scene.selections(),
        vec![4 * SCAN_CHUNK - 1..4 * SCAN_CHUNK - 1]
    );
    scene.undo();
    assert_eq!(scene.text(), original);
}

#[gpui::test]
async fn a_whole_batch_is_one_undo_step(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + 7;
    let original = "foo\n".repeat(lines);
    let mut scene = scene(&original, cx).await;
    scene.select(&[0..0]);
    scene.replace(ReplaceString, "foo", "ba");
    assert_eq!(scene.text(), "ba\n".repeat(lines));
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {lines} occurrences")]
    );
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn starters_are_visible_in_the_command_palette(cx: &mut TestAppContext) {
    let scene = scene("", cx).await;
    scene.cx.update(|cx| {
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx)
            .expect("the command palette filter is set");
        assert!(!filter.is_hidden(&ReplaceString));
        assert!(!filter.is_hidden(&ReplaceRegexp));
        assert!(!filter.is_hidden(&QueryReplace));
    });
}
