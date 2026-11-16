//! Tests of `!` in the decision view: the progress on the line while the batch runs, `C-g` in the
//! middle of it (the chunks made stay, whole replacements only), the keys it ignores meanwhile, an
//! edit made outside the batch between two chunks, and one undo step for the whole session. The
//! keys are typed through the real rows of `specific-overrides.json`; `window.dispatch_action` and
//! single ticks stop the batch between its chunks.

use editor::Editor;
use gpui::TestAppContext;
use multi_buffer::MultiBufferOffset;

use crate::actions::{All, Exit, Help, QueryReplace, Undo};
use crate::commands_tests::{Scene, scene};
use crate::engine::SCAN_CHUNK;
use crate::session::{BATCH_PROMPT, batch_running, batch_status};

async fn asking(text: &str, from: &str, to: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene(text, cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, from, to);
    assert!(scene.view_open(), "the decision view opens");
    scene
}

impl Scene {
    /// Dispatches `action` in the window without running the tasks it starts.
    fn press(&mut self, action: impl gpui::Action) {
        let action = action.boxed_clone();
        self.cx
            .update(|window, cx| window.dispatch_action(action, cx));
    }

    /// Runs one task at a time until `needle` is in the text `count` times.
    fn tick_until(&mut self, needle: &str, count: usize) {
        let mut ticks = 0;
        while self.text().matches(needle).count() < count {
            assert!(ticks < 1_000, "{count} of {needle} never landed");
            self.cx.executor().tick();
            ticks += 1;
        }
        assert_eq!(self.text().matches(needle).count(), count);
    }

    fn batch_running(&mut self) -> bool {
        self.editor
            .read_with(&self.cx, |editor: &Editor, _| batch_running(editor))
    }
}

#[test]
fn the_status_leaves_out_an_unknown_total() {
    assert_eq!(batch_status(12_000, Some(50_000)), "Replacing… 12000/50000");
    assert_eq!(batch_status(12_000, None), "Replacing… 12000");
}

#[gpui::test]
async fn the_line_shows_the_progress_of_the_batch(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + SCAN_CHUNK / 2;
    let original = "foo\n".repeat(lines);
    let mut scene = asking(&original, "foo", "bar", cx).await;
    scene.press(All);
    assert_eq!(scene.prompt(), BATCH_PROMPT);
    assert_eq!(scene.status(), batch_status(0, Some(lines)));
    scene.tick_until("bar", SCAN_CHUNK);
    assert!(scene.batch_running());
    assert_eq!(scene.prompt(), BATCH_PROMPT);
    assert_eq!(scene.status(), batch_status(SCAN_CHUNK, Some(lines)));

    scene.cx.run_until_parked();
    assert_eq!(scene.text(), "bar\n".repeat(lines));
    assert!(!scene.view_open());
    assert!(!scene.batch_running());
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {lines} occurrences")]
    );
    // The cursor is at the end of the last replacement.
    assert_eq!(scene.selections(), vec![4 * lines - 1..4 * lines - 1]);
    // One undo reverts the whole batch and puts the cursor back.
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn ctrl_g_mid_batch_keeps_the_chunks_made_and_says_interrupted(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + SCAN_CHUNK / 2;
    let original = "foo\n".repeat(lines);
    let mut scene = asking(&original, "foo", "bar", cx).await;
    scene.press(All);
    scene.tick_until("bar", SCAN_CHUNK);

    // Only C-g acts while the batch runs.
    scene.press(Exit);
    scene.press(Undo);
    scene.press(Help);
    assert!(scene.view_open());
    assert!(scene.batch_running());
    assert_eq!(scene.text().matches("bar").count(), SCAN_CHUNK);
    assert!(scene.help_lines().is_empty());

    scene.cx.simulate_keystrokes("ctrl-g");
    scene.cx.run_until_parked();
    // Whole replacements only: the first chunk, and nothing of the next.
    let expected = "bar\n".repeat(SCAN_CHUNK) + &"foo\n".repeat(lines - SCAN_CHUNK);
    assert_eq!(scene.text(), expected);
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {SCAN_CHUNK} occurrences (interrupted)")]
    );
    assert!(!scene.view_open());
    assert!(!scene.batch_running());
    assert_eq!(
        scene.selections(),
        vec![4 * SCAN_CHUNK - 1..4 * SCAN_CHUNK - 1]
    );
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn y_n_then_bang_is_one_undo_step(cx: &mut TestAppContext) {
    let original = "foo foo foo foo foo\n";
    let mut scene = asking(original, "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("n");
    scene.keys("!");
    assert_eq!(scene.text(), "bar foo bar bar bar\n");
    assert!(!scene.view_open());
    assert_eq!(scene.messages(), vec!["Replaced 4, skipped 1"]);
    assert_eq!(scene.selections(), vec![19..19]);
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn bang_after_comma_replaces_the_rest(cx: &mut TestAppContext) {
    let original = "foo foo foo\n";
    let mut scene = asking(original, "foo", "bar", cx).await;
    scene.keys(",");
    scene.keys("!");
    assert_eq!(scene.text(), "bar bar bar\n");
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
    scene.undo();
    assert_eq!(scene.text(), original);
}

#[gpui::test]
async fn an_edit_between_chunks_is_kept_and_its_match_left_alone(cx: &mut TestAppContext) {
    let lines = 2 * SCAN_CHUNK + 10;
    let original = "foo\n".repeat(lines);
    let mut scene = asking(&original, "foo", "bar", cx).await;
    scene.press(All);
    scene.tick_until("bar", SCAN_CHUNK);

    // Text inserted before everything, and inside a match the batch has not reached.
    let changed = SCAN_CHUNK + 5;
    scene.editor.update(&mut scene.cx, |editor, cx| {
        let inside = MultiBufferOffset(4 * changed + 1);
        editor.edit(
            [
                (MultiBufferOffset(0)..MultiBufferOffset(0), "X\n"),
                (inside..inside, "Z"),
            ],
            cx,
        );
    });
    scene.cx.run_until_parked();

    let line = |index: usize| if index == changed { "fZoo\n" } else { "bar\n" };
    let after: String = (0..lines).map(line).collect();
    assert_eq!(scene.text(), format!("X\n{after}"));
    assert_eq!(
        scene.messages(),
        vec![format!("Replaced {} occurrences", lines - 1)]
    );
    // The outside edit is its own undo step above the session's.
    scene.undo();
    let unedited: String = (0..lines)
        .map(|index| if index == changed { "foo\n" } else { "bar\n" })
        .collect();
    assert_eq!(scene.text(), unedited);
    scene.undo();
    assert_eq!(scene.text(), original);
}
