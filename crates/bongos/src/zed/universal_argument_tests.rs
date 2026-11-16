//! The prefix argument through the keys of the app keymap: each test types a prefix (C-u, a digit
//! key or the minus key) and then a key of the Linux Emacs base keymap, in an editor with the whole
//! app keymap loaded, and asserts what the editor ends up as. Every key goes through
//! `simulate_keystrokes`, so it passes the keystroke interceptor as a typed key does.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_emacs::mark::{push_mark, ring_for};
use bongos_emacs::prefix_arg;
use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorEvent, MultiBufferOffset, kill_ring};
use gpui::{
    App, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::{KeybindSource, KeymapFile};
use workspace::{AppState, MultiWorkspace, Workspace};

use super::tests::init_keymap_test;

/// Loads the keymaps the app loads on Linux with the Emacs base: the defaults, the Emacs base and
/// the specific overrides.
fn load_app_keymap(cx: &mut App) {
    let mut all = Vec::new();
    for (path, source) in [
        ("keymaps/default-linux.json", KeybindSource::Default),
        ("keymaps/linux/emacs.json", KeybindSource::Base),
        ("keymaps/specific-overrides.json", KeybindSource::Default),
    ] {
        all.extend(
            KeymapFile::load_asset(path, Some(source), cx)
                .unwrap_or_else(|error| panic!("{path}: {error}")),
        );
    }
    cx.bind_keys(all);
}

/// Starts the app: its actions and the app keymap.
fn setup(cx: &mut TestAppContext) -> Arc<AppState> {
    let app_state = init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_emacs::init(cx);
        load_app_keymap(cx);
    });
    app_state
}

/// An editor on `marked` (`ˇ` is the caret) with the app keymap loaded.
async fn editor_with(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    setup(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state(marked);
    cx
}

/// A workspace with an editor on `text`, caret at `caret`, focused, with the app keymap loaded.
async fn workspace_editor(
    text: &str,
    caret: usize,
    cx: &mut TestAppContext,
) -> (Entity<Editor>, VisualTestContext) {
    let app_state = setup(cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace: Entity<Workspace> = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(text, None, false, cx)
        });
        buffer.update(cx, |buffer, _| buffer.set_group_interval(Duration::ZERO));
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(caret)..MultiBufferOffset(caret)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (editor, cx)
}

fn text_of(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.text(cx))
}

/// The newest kill ring entries as text, newest first.
fn ring_texts(cx: &mut EditorTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        kill_ring::read(cx).map_or(Vec::new(), |ring| {
            (0..ring.len())
                .filter_map(|ix| ring.get(ix).map(|entry| entry.text.clone()))
                .collect()
        })
    })
}

/// `lines` lines, "line 0", "line 1" and so on, with the caret at the start of line `caret_row`.
fn numbered(lines: usize, caret_row: usize) -> String {
    (0..lines)
        .map(|row| {
            let mark = if row == caret_row { "ˇ" } else { "" };
            format!("{mark}line {row}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn argument_pending(cx: &mut EditorTestContext) -> bool {
    cx.update(|window, cx| prefix_arg::pending(window, cx).is_some())
}

/// The caret row, and its row inside the window.
fn rows(cx: &mut EditorTestContext) -> (u32, u32) {
    cx.update_editor(|editor, _window, cx| {
        let display = editor.display_snapshot(cx);
        let row = editor.selections.newest_display(&display).head().row().0;
        let top = editor.scroll_position(cx).y as u32;
        (row, row - top)
    })
}

#[gpui::test]
async fn the_argument_moves_forward_char_by_four_or_by_the_count_and_a_negative_one_goes_back(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, "ˇabcdefghij").await;
    cx.simulate_keystrokes("ctrl-u ctrl-f");
    cx.assert_editor_state("abcdˇefghij");
    assert!(!argument_pending(&mut cx));

    cx.set_state("ˇabcdefghij");
    cx.simulate_keystrokes("alt-3 ctrl-f");
    cx.assert_editor_state("abcˇdefghij");

    cx.set_state("abcdˇefghij");
    cx.simulate_keystrokes("alt-- ctrl-f");
    cx.assert_editor_state("abcˇdefghij");

    // A minus after C-u makes the argument -4 (the model's choice; Emacs reads it as -1).
    cx.set_state("abcdˇefghij");
    cx.simulate_keystrokes("ctrl-u - ctrl-f");
    cx.assert_editor_state("ˇabcdefghij");
}

#[gpui::test]
async fn typed_digits_after_the_universal_argument_move_down_that_many_lines(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, &numbered(30, 0)).await;
    cx.simulate_keystrokes("ctrl-u 1 0 ctrl-n");
    cx.assert_editor_state(&numbered(30, 10));
    assert!(!argument_pending(&mut cx));
}

#[gpui::test]
async fn a_hundred_lines_down_is_one_selection_change_in_one_go(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, &numbered(300, 0)).await;
    let changes = Rc::new(Cell::new(0usize));
    let counter = changes.clone();
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, event, _| {
            if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                counter.set(counter.get() + 1);
            }
        })
        .detach();
    });
    cx.simulate_keystrokes("ctrl-u 1 0 0 ctrl-n");
    cx.run_until_parked();
    cx.assert_editor_state(&numbered(300, 100));
    assert!(!argument_pending(&mut cx));
    assert_eq!(changes.get(), 1, "selection changes");
}

#[gpui::test]
async fn kill_line_with_a_digit_argument_kills_three_lines_as_one_entry(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa\nb\nc\nd").await;
    cx.simulate_keystrokes("alt-3 ctrl-k");
    cx.assert_editor_state("ˇd");
    assert_eq!(ring_texts(&mut cx), vec!["a\nb\nc\n".to_string()]);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("a\nb\nc\nˇd");
}

#[gpui::test]
async fn forward_word_goes_sixteen_words_with_two_universal_arguments_and_back_with_a_minus(
    cx: &mut TestAppContext,
) {
    let words = "a b c d e f g h i j k l m n o p q r s t";
    let mut cx = editor_with(cx, &format!("ˇ{words}")).await;
    cx.simulate_keystrokes("ctrl-u ctrl-u alt-f");
    cx.assert_editor_state("a b c d e f g h i j k l m n o pˇ q r s t");

    cx.set_state("a b cˇ d");
    cx.simulate_keystrokes("alt-- alt-f");
    cx.assert_editor_state("a b ˇc d");

    // A minus after C-u makes the argument -4: four words back.
    cx.set_state("a b c dˇ e");
    cx.simulate_keystrokes("ctrl-u - alt-f");
    cx.assert_editor_state("ˇa b c d e");
}

#[gpui::test]
async fn yank_with_the_universal_argument_wraps_to_the_second_newest_entry(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, "ˇone two three").await;
    cx.simulate_keystrokes("alt-d ctrl-f alt-d");
    cx.assert_editor_state(" ˇ three");
    assert_eq!(
        ring_texts(&mut cx),
        vec!["two".to_string(), "one".to_string()]
    );
    // Four, counted from the newest of two entries, wraps to the second one.
    cx.simulate_keystrokes("ctrl-u ctrl-y");
    cx.assert_editor_state(" oneˇ three");
}

#[gpui::test]
async fn yank_pop_with_a_count_steps_that_many_entries_older(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa b c").await;
    // Three kills, each apart from the last so each is its own entry: a, b, c (newest).
    cx.simulate_keystrokes("alt-d ctrl-f alt-d ctrl-f alt-d");
    assert_eq!(
        ring_texts(&mut cx),
        vec!["c".to_string(), "b".to_string(), "a".to_string()]
    );
    cx.set_state("ˇ");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("cˇ");
    cx.simulate_keystrokes("alt-2 alt-y");
    cx.assert_editor_state("aˇ");
}

#[gpui::test]
async fn the_universal_argument_with_set_mark_pops_one_mark_entry(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇ0123456789").await;
    let editor = cx.editor.clone();
    // Three marks, at 0, 3 and 6: the mark is at 6 and the ring holds 3 and 0.
    for _ in 0..3 {
        cx.update(|window, cx| push_mark(&editor, None, false, window, cx));
        cx.simulate_keystrokes("ctrl-f ctrl-f ctrl-f");
    }
    cx.assert_editor_state("012345678ˇ9");
    assert_eq!(cx.update(|_, cx| ring_for(&editor, cx).len()), 2);

    cx.simulate_keystrokes("ctrl-u ctrl-space");
    cx.assert_editor_state("012345ˇ6789");
    // One entry was popped, not four.
    assert_eq!(cx.update(|_, cx| ring_for(&editor, cx).len()), 1);
    assert!(!argument_pending(&mut cx));
}

#[gpui::test]
async fn goto_line_with_a_universal_argument_jumps_to_that_line(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, &numbered(100, 0)).await;
    cx.simulate_keystrokes("ctrl-u 4 2 alt-g");
    cx.run_until_parked();
    cx.assert_editor_state(&numbered(100, 41));
    assert!(!argument_pending(&mut cx));

    cx.set_state(&numbered(100, 0));
    cx.simulate_keystrokes("alt-7 alt-g");
    cx.run_until_parked();
    cx.assert_editor_state(&numbered(100, 6));
}

#[gpui::test]
async fn zap_to_char_with_a_count_of_two_reaches_the_second_occurrence(cx: &mut TestAppContext) {
    let (editor, mut cx) = workspace_editor("a,b,c,d", 0, cx).await;
    cx.simulate_keystrokes("alt-2 alt-z");
    cx.run_until_parked();
    cx.simulate_keystrokes(",");
    cx.run_until_parked();
    assert_eq!(text_of(&editor, &mut cx), "c,d");
}

#[gpui::test]
async fn zap_up_to_char_with_a_count_stops_before_the_nth_occurrence(cx: &mut TestAppContext) {
    let (editor, mut cx) = workspace_editor("a;b;c", 0, cx).await;
    cx.simulate_keystrokes("alt-2 alt-shift-z");
    cx.run_until_parked();
    cx.simulate_keystrokes(";");
    cx.run_until_parked();
    assert_eq!(text_of(&editor, &mut cx), ";c");
}

#[gpui::test]
async fn recenter_with_an_argument_puts_the_line_at_that_row_and_does_not_cycle(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, &numbered(100, 50)).await;
    cx.update_editor(|editor, _window, cx| editor.set_vertical_scroll_margin(2, cx));
    cx.simulate_keystrokes("ctrl-u ctrl-l");
    // The argument is 4: the caret line stands at row 4 of the window.
    assert_eq!(rows(&mut cx), (50, 4));
    assert!(!argument_pending(&mut cx));
    cx.simulate_keystrokes("alt-1 0 ctrl-l");
    assert_eq!(rows(&mut cx), (50, 10));
    // With no argument the cycle starts from the middle again (45 rows, margin 2).
    cx.simulate_keystrokes("ctrl-l");
    assert_eq!(rows(&mut cx), (50, 22));
}

#[gpui::test]
async fn transpose_chars_with_a_count_moves_the_character_that_far(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "abˇcde").await;
    cx.simulate_keystrokes("alt-2 ctrl-t");
    cx.assert_editor_state("acdbˇe");
}

#[gpui::test]
async fn transpose_lines_with_a_count_moves_the_line_past_that_many(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "a\nb\nˇc\nd\ne").await;
    cx.simulate_keystrokes("alt-2 ctrl-x ctrl-t");
    cx.assert_editor_state("a\nc\nd\nbˇ\ne");
}

#[gpui::test]
async fn transpose_words_with_a_count_moves_the_word_that_far(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "one two ˇthree four five").await;
    cx.simulate_keystrokes("alt-2 alt-t");
    cx.assert_editor_state("one three four twoˇ five");
}

#[gpui::test]
async fn kill_region_or_line_with_a_count_kills_that_many_whole_lines(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa\nb\nc").await;
    cx.simulate_keystrokes("alt-2 ctrl-w");
    cx.assert_editor_state("ˇc");
    assert_eq!(ring_texts(&mut cx), vec!["a\nb\n".to_string()]);
}

#[gpui::test]
async fn copy_region_or_line_with_a_count_copies_that_many_whole_lines(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa\nb\nc").await;
    cx.simulate_keystrokes("alt-2 alt-w");
    cx.assert_editor_state("ˇa\nb\nc");
    assert_eq!(ring_texts(&mut cx), vec!["a\nb\n".to_string()]);
}

#[gpui::test]
async fn kill_whole_line_with_a_count_kills_that_many_lines(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "a\nbˇ\nc\nd").await;
    cx.simulate_keystrokes("alt-2 ctrl-alt-k");
    cx.assert_editor_state("a\nˇd");
    assert_eq!(ring_texts(&mut cx), vec!["b\nc\n".to_string()]);
}

#[gpui::test]
async fn duplicate_line_with_a_count_adds_that_many_copies(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "aˇb\nc").await;
    cx.simulate_keystrokes("alt-3 ctrl-c y");
    cx.assert_editor_state("ab\naˇb\nab\nab\nc");
}

#[gpui::test]
async fn copy_from_above_with_a_count_copies_that_many_characters(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "abcdef\nˇ").await;
    cx.simulate_keystrokes("alt-3 ctrl-c a");
    cx.assert_editor_state("abcdef\nabcˇ");
}

#[gpui::test]
async fn delete_horizontal_space_with_any_argument_deletes_only_before_the_caret(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, "a  ˇ  b").await;
    cx.simulate_keystrokes("ctrl-backspace");
    cx.assert_editor_state("aˇb");

    cx.set_state("a  ˇ  b");
    cx.simulate_keystrokes("ctrl-u ctrl-backspace");
    cx.assert_editor_state("aˇ  b");
}

#[gpui::test]
async fn just_one_space_with_an_argument_leaves_that_many_spaces(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "a   ˇ   b").await;
    cx.simulate_keystrokes("alt-space");
    cx.assert_editor_state("a ˇb");

    cx.set_state("a   ˇ   b");
    cx.simulate_keystrokes("alt-3 alt-space");
    cx.assert_editor_state("a   ˇb");

    cx.set_state("a   ˇ   b");
    cx.simulate_keystrokes("alt-0 alt-space");
    cx.assert_editor_state("aˇb");
}

#[gpui::test]
async fn kill_word_and_backward_kill_word_with_a_count_kill_that_many_words(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, "ˇone two three four").await;
    cx.simulate_keystrokes("alt-2 alt-d");
    cx.assert_editor_state("ˇ three four");
    assert_eq!(ring_texts(&mut cx), vec!["one two".to_string()]);

    cx.set_state("one two three fourˇ");
    cx.simulate_keystrokes("alt-2 alt-backspace");
    cx.assert_editor_state("one two ˇ");
}

#[gpui::test]
async fn case_words_with_a_count_change_that_many_words(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇone two three").await;
    cx.simulate_keystrokes("alt-2 alt-u");
    cx.assert_editor_state("ONE TWOˇ three");

    cx.set_state("ˇONE TWO THREE");
    cx.simulate_keystrokes("alt-2 alt-l");
    cx.assert_editor_state("one twoˇ THREE");

    cx.set_state("ˇone two three");
    cx.simulate_keystrokes("alt-2 alt-c");
    cx.assert_editor_state("One Twoˇ three");
}

#[gpui::test]
async fn move_line_with_a_count_moves_the_line_that_many_lines(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "a\nb\nc\nˇd").await;
    cx.simulate_keystrokes("alt-2 alt-up");
    cx.assert_editor_state("a\nˇd\nb\nc");

    cx.set_state("ˇa\nb\nc\nd");
    cx.simulate_keystrokes("alt-2 alt-down");
    cx.assert_editor_state("b\nc\nˇa\nd");
}

#[gpui::test]
async fn exchange_point_and_mark_ignores_the_argument_and_runs_once(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇone two").await;
    cx.simulate_keystrokes("ctrl-space ctrl-f ctrl-f ctrl-f");
    cx.assert_editor_state("«oneˇ» two");
    // Four runs would swap the ends four times and leave them as they were.
    cx.simulate_keystrokes("ctrl-u ctrl-x ctrl-x");
    cx.assert_editor_state("«ˇone» two");
    assert!(!argument_pending(&mut cx));
}

#[gpui::test]
async fn the_starters_of_a_search_run_once_with_an_argument_pending(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇone two").await;
    cx.simulate_keystrokes("ctrl-u ctrl-s");
    cx.run_until_parked();
    assert!(!argument_pending(&mut cx));
}

#[gpui::test]
async fn a_counted_motion_between_two_kills_keeps_them_apart(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇab\ncd\nef").await;
    cx.simulate_keystrokes("ctrl-k ctrl-u 2 ctrl-f ctrl-k");
    assert_eq!(
        ring_texts(&mut cx),
        vec!["d".to_string(), "ab".to_string()]
    );
}

#[gpui::test]
async fn a_counted_key_ends_a_yank_chain(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa b").await;
    cx.simulate_keystrokes("alt-d ctrl-f alt-d");
    assert_eq!(ring_texts(&mut cx), vec!["b".to_string(), "a".to_string()]);
    cx.set_state("ˇ");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("bˇ");
    // A counted typed character is not a yank, so M-y no longer pops.
    cx.simulate_keystrokes("ctrl-u 3 x alt-y");
    cx.assert_editor_state("bxxxˇ");
}

#[gpui::test]
async fn the_case_previous_word_keys_take_a_count_and_a_negative_count(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "one two threeˇ four").await;
    cx.simulate_keystrokes("alt-2 alt-shift-u");
    cx.assert_editor_state("one TWO THREEˇ four");

    cx.set_state("ONE TWO THREEˇ FOUR");
    cx.simulate_keystrokes("alt-3 alt-shift-l");
    cx.assert_editor_state("one two threeˇ FOUR");

    cx.set_state("hELLO wORLDˇ");
    cx.simulate_keystrokes("alt-1 alt-shift-c");
    cx.assert_editor_state("hELLO Worldˇ");

    // Negative reverses: the words after point.
    cx.set_state("one ˇtwo three four");
    cx.simulate_keystrokes("alt-- 2 alt-shift-u");
    cx.assert_editor_state("one ˇTWO THREE four");
}

#[gpui::test]
async fn transpose_words_reverse_takes_a_count_and_a_negative_count(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "one two three fourˇ").await;
    cx.simulate_keystrokes("alt-2 alt-shift-t");
    cx.assert_editor_state("one fourˇ two three");

    cx.set_state("one twoˇ three four");
    cx.simulate_keystrokes("alt-- alt-shift-t");
    cx.assert_editor_state("one three twoˇ four");
}

#[gpui::test]
async fn zap_back_to_char_with_a_count_reaches_the_nth_occurrence(cx: &mut TestAppContext) {
    let (editor, mut cx) = workspace_editor("a,b,c,d", 7, cx).await;
    cx.simulate_keystrokes("alt-2 ctrl-alt-z");
    cx.run_until_parked();
    cx.simulate_keystrokes(",");
    cx.run_until_parked();
    assert_eq!(text_of(&editor, &mut cx), "a,b");
}

#[gpui::test]
async fn zap_back_up_to_char_with_a_count_stops_after_the_nth_occurrence(
    cx: &mut TestAppContext,
) {
    let (editor, mut cx) = workspace_editor("a,b,c,d", 7, cx).await;
    cx.simulate_keystrokes("alt-2 ctrl-alt-shift-z");
    cx.run_until_parked();
    cx.simulate_keystrokes(",");
    cx.run_until_parked();
    assert_eq!(text_of(&editor, &mut cx), "a,b,");
}

/// Runs `keys` alone and then after C-u, from `marked`, and asserts the argument changed nothing
/// and none is left pending.
async fn assert_ignores_the_argument(cx: &mut TestAppContext, marked: &str, keys: &str) {
    let mut cx = editor_with(cx, marked).await;
    cx.simulate_keystrokes(keys);
    let plain = cx.editor_state();
    cx.set_state(marked);
    cx.simulate_keystrokes(&format!("ctrl-u {keys}"));
    assert_eq!(cx.editor_state(), plain, "{keys}");
    assert!(!argument_pending(&mut cx), "{keys}");
}

#[gpui::test]
async fn duplicate_and_comment_ignores_the_argument(cx: &mut TestAppContext) {
    assert_ignores_the_argument(cx, "aˇb\nc", "ctrl-c c").await;
}

#[gpui::test]
async fn comment_dwim_ignores_the_argument(cx: &mut TestAppContext) {
    assert_ignores_the_argument(cx, "aˇb\nc", "ctrl-c ;").await;
}

#[gpui::test]
async fn delete_blank_lines_ignores_the_argument(cx: &mut TestAppContext) {
    assert_ignores_the_argument(cx, "a\n\nˇ\n\nb", "ctrl-x ctrl-o").await;
}

#[gpui::test]
async fn reposition_ignores_the_argument(cx: &mut TestAppContext) {
    assert_ignores_the_argument(cx, &numbered(100, 50), "alt-r").await;
}

#[gpui::test]
async fn pop_global_mark_ignores_the_argument(cx: &mut TestAppContext) {
    assert_ignores_the_argument(cx, "aˇb\nc", "ctrl-x ctrl-.").await;
}

#[gpui::test]
async fn delete_char_and_delete_backward_char_take_a_count(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇabcdef").await;
    cx.simulate_keystrokes("alt-3 ctrl-d");
    cx.assert_editor_state("ˇdef");

    cx.set_state("abcˇdef");
    cx.simulate_keystrokes("alt-3 backspace");
    cx.assert_editor_state("ˇdef");
}

#[gpui::test]
async fn newline_inserts_that_many_newlines(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "ˇa").await;
    cx.simulate_keystrokes("alt-3 enter");
    cx.assert_editor_state("\n\n\nˇa");
}

#[gpui::test]
async fn open_line_inserts_that_many_newlines_and_leaves_point_before_them(
    cx: &mut TestAppContext,
) {
    let mut cx = editor_with(cx, "fooˇbar").await;
    cx.simulate_keystrokes("alt-2 ctrl-o");
    cx.assert_editor_state("fooˇ\n\nbar");
}

#[gpui::test]
async fn kill_line_with_a_zero_or_negative_argument_kills_backward(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "abˇcd\nef").await;
    cx.simulate_keystrokes("alt-0 ctrl-k");
    cx.assert_editor_state("ˇcd\nef");
    assert_eq!(ring_texts(&mut cx), vec!["ab".to_string()]);

    cx.set_state("aa\nbˇb\ncc");
    cx.simulate_keystrokes("alt-- ctrl-k");
    cx.assert_editor_state("ˇb\ncc");
}

#[gpui::test]
async fn kill_whole_line_with_a_negative_argument_kills_the_lines_above(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "aa\nbb\ncˇc\ndd").await;
    cx.simulate_keystrokes("alt-- 2 ctrl-alt-k");
    cx.assert_editor_state("aa\nˇdd");
    assert_eq!(ring_texts(&mut cx), vec!["bb\ncc\n".to_string()]);
}

#[gpui::test]
async fn the_transposes_take_a_negative_argument(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, "abˇcd").await;
    cx.simulate_keystrokes("alt-- ctrl-t");
    cx.assert_editor_state("bˇacd");

    cx.set_state("one\ntwo\nthreeˇ\nfour");
    cx.simulate_keystrokes("alt-- ctrl-x ctrl-t");
    cx.assert_editor_state("twoˇ\none\nthree\nfour");

    cx.set_state("one two threeˇ");
    cx.simulate_keystrokes("alt-- alt-t");
    cx.assert_editor_state("one threeˇ two");
}

#[gpui::test]
async fn a_counted_motion_after_a_recenter_starts_the_cycle_again(cx: &mut TestAppContext) {
    let mut cx = editor_with(cx, &numbered(100, 50)).await;
    cx.update_editor(|editor, _window, cx| editor.set_vertical_scroll_margin(2, cx));
    cx.simulate_keystrokes("ctrl-l");
    let middle = rows(&mut cx).1;
    cx.simulate_keystrokes("ctrl-u 5 ctrl-n ctrl-l");
    // The counted motion was a command in between, so this recenter starts over in the middle.
    assert_eq!(rows(&mut cx).1, middle);
}
