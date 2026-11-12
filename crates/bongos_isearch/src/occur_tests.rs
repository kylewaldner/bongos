//! Tests of occur: the results tab of the matching lines, its excerpts, its line numbers, the jump
//! back to the buffer and the cap. The keys are the real rows of `specific-overrides.json`.

use bongos_minibuffer::{Minibuffer, minibuffer_for_window};
use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use language::ToPoint as _;
use workspace::Workspace;

use crate::actions::Forward;
use crate::engine::build_query;
use crate::occur::{LINE_CAP, group, results, title};
use crate::session_tests::{keys, session_open, start, text, type_slowly, workspace_text};

/// `count` lines `line N` (N from 1), with `needle` appended to the lines in `with`.
fn lines(count: usize, with: &[usize]) -> String {
    (1..=count)
        .map(|number| {
            if with.contains(&number) {
                format!("line {number} needle\n")
            } else {
                format!("line {number}\n")
            }
        })
        .collect()
}

/// Routes the echo area too, so messages are readable.
fn route_messages(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.default_global::<workspace::MinibufferRouting>()
            .set_routes(vec![
                crate::session::HOST.to_string(),
                bongos_minibuffer::MESSAGES.to_string(),
            ]);
    });
}

fn strip(cx: &mut VisualTestContext) -> Entity<Minibuffer> {
    cx.update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip")
}

fn messages(cx: &mut VisualTestContext) -> Vec<String> {
    let strip = strip(cx);
    strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    })
}

/// The active tab's editor.
fn active_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Editor> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item_as::<Editor>(cx)
            .expect("the active tab is an editor")
    })
}

/// Searches for `needle` from the start and runs occur.
async fn occur_for(
    source: &str,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let (workspace, editor, mut cx) = workspace_text(source, true, cx).await;
    route_messages(&mut cx);
    start(Forward, &workspace, &mut cx);
    type_slowly("needle", &mut cx);
    keys("ctrl-c ctrl-o", &mut cx);
    cx.run_until_parked();
    (workspace, editor, cx)
}

/// The rows (zero-based) of the source each excerpt of `results` starts and ends on.
fn excerpt_rows(
    results: &Entity<Editor>,
    source: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> Vec<(u32, u32)> {
    let buffer = source.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().expect("singleton")
    });
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    results.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .excerpts()
            .map(|excerpt| {
                (
                    excerpt.context.start.to_point(&snapshot).row,
                    excerpt.context.end.to_point(&snapshot).row,
                )
            })
            .collect()
    })
}

#[gpui::test]
async fn occur_lists_non_adjacent_lines_one_excerpt_each(cx: &mut TestAppContext) {
    let source = lines(200, &[10, 50, 120]);
    let (workspace, editor, mut cx) = occur_for(&source, cx).await;
    let cx = &mut cx;
    assert!(!session_open(&workspace, cx), "the session closed");
    let results = active_editor(&workspace, cx);
    assert_ne!(results, editor);
    results.read_with(cx, |results, cx| {
        let buffer = results.buffer().read(cx);
        assert_eq!(buffer.title(cx), title("needle"));
        assert_eq!(buffer.title(cx), "Occur: needle");
        assert!(buffer.read_only());
    });
    // Rows are zero-based: the lines 10, 50 and 120 of the text.
    assert_eq!(
        excerpt_rows(&results, &editor, cx),
        [(9, 9), (49, 49), (119, 119)]
    );
    assert_eq!(text(&editor, cx), source, "the source is untouched");
}

#[gpui::test]
async fn occur_merges_adjacent_lines_into_one_excerpt(cx: &mut TestAppContext) {
    let source = lines(200, &[30, 31, 90]);
    let (workspace, editor, mut cx) = occur_for(&source, cx).await;
    let cx = &mut cx;
    let results = active_editor(&workspace, cx);
    assert_eq!(excerpt_rows(&results, &editor, cx), [(29, 30), (89, 89)]);
}

#[gpui::test]
async fn occur_two_adjacent_lines_are_one_excerpt(cx: &mut TestAppContext) {
    let source = lines(5, &[2, 3]);
    let (workspace, editor, mut cx) = occur_for(&source, cx).await;
    let cx = &mut cx;
    let results = active_editor(&workspace, cx);
    assert_eq!(excerpt_rows(&results, &editor, cx), [(1, 2)]);
}

#[gpui::test]
async fn occur_open_excerpts_lands_on_the_line_of_the_source(cx: &mut TestAppContext) {
    let source = lines(200, &[10, 50, 120]);
    let (workspace, editor, mut cx) = occur_for(&source, cx).await;
    let cx = &mut cx;
    let results = active_editor(&workspace, cx);
    // The cursor goes to the second excerpt, whose first row is the third row of the results
    // (each excerpt has a header row above it).
    results.update_in(cx, |results, window, cx| {
        let snapshot = results.buffer().read(cx).snapshot(cx);
        let second = snapshot.excerpts().nth(1).expect("a second excerpt");
        let anchor = snapshot
            .anchor_in_excerpt(second.context.start)
            .expect("the excerpt start has an anchor");
        results.change_selections(Default::default(), window, cx, |selections| {
            selections.select_anchor_ranges([anchor..anchor]);
        });
    });
    cx.dispatch_action(editor::actions::OpenExcerpts);
    cx.run_until_parked();
    let landed = active_editor(&workspace, cx);
    assert_eq!(landed, editor);
    let row = landed.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.newest_display(&display).head().row().0
    });
    assert_eq!(row, 49);
}

#[gpui::test]
async fn occur_stops_at_the_line_cap_and_says_so(cx: &mut TestAppContext) {
    let all: Vec<usize> = (1..=LINE_CAP + 50).collect();
    let source = lines(LINE_CAP + 50, &all);
    let (workspace, editor, mut cx) = occur_for(&source, cx).await;
    let cx = &mut cx;
    let results = active_editor(&workspace, cx);
    // Every line matches and the lines are adjacent: one excerpt of exactly the cap.
    assert_eq!(
        excerpt_rows(&results, &editor, cx),
        [(0, LINE_CAP as u32 - 1)]
    );
    assert!(
        messages(cx)
            .iter()
            .any(|message| message == "Occur stopped at 10000 lines"),
        "{:?}",
        messages(cx)
    );
}

#[gpui::test]
async fn occur_below_the_cap_says_nothing_of_it(cx: &mut TestAppContext) {
    let source = lines(20, &[3, 9]);
    let (_workspace, _editor, mut cx) = occur_for(&source, cx).await;
    assert!(
        !messages(&mut cx)
            .iter()
            .any(|message| message.starts_with("Occur stopped"))
    );
}

#[gpui::test]
async fn occur_leaves_point_at_the_match_and_pushes_the_mark(cx: &mut TestAppContext) {
    let source = lines(40, &[5, 20]);
    let (workspace, editor, mut cx) = workspace_text(&source, true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    keys("ctrl-c ctrl-o", cx);
    let first_match = source.find("needle").unwrap() + "needle".len();
    assert_eq!(crate::session_tests::cursor(&editor, cx), first_match);
    assert_eq!(crate::session_tests::mark(&editor, cx), Some(0));
}

#[gpui::test]
async fn occur_skips_rows_the_buffer_lost_while_the_pass_ran(cx: &mut TestAppContext) {
    let source = lines(6, &[2, 5]);
    let (_workspace, editor, mut cx) = workspace_text(&source, true, cx).await;
    let cx = &mut cx;
    let mut spec = bongos_isearch_model::SearchSpec::new(false);
    spec.string = "needle".to_string();
    let query = build_query(&bongos_isearch_model::compile(&spec)).unwrap();
    let (buffer, old) = editor.update(cx, |editor, cx| {
        (
            editor.buffer().clone(),
            editor.buffer().read(cx).snapshot(cx),
        )
    });
    let found = group(&old, &query);
    assert_eq!(found.len(), 2);

    // The buffer shrinks to two lines after the pass; both matching lines are gone or stale.
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("one\ntwo needle", window, cx)
    });
    cx.run_until_parked();
    let list = cx.update(|_, cx| results(&buffer, &found, "needle", cx));
    let rows = list.read_with(cx, |list, cx| list.snapshot(cx).max_point().row);
    assert_eq!(rows, 0, "only the surviving row is listed");
}
