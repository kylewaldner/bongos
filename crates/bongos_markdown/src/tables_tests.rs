//! Tests of tables as grids: pipes off point line up across rows, the row under point shows its
//! pipes as typed, line and cell moves cross the grid as plain rows, an edit re-pads its column,
//! and only the rows in the window are padded.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use editor::{Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    AppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
    WindowHandle, point,
};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher, Point};
use project::Project;
use settings::KeymapFile;
use workspace::{MinibufferRouting, MultiWorkspace};

use crate::{FALLBACK_SCREEN_ROWS, Mode, conceal_tag, markdown, set_mode};

fn markdown_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Markdown".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["md".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

/// A live editor on `/root/doc.md` holding `text`, with the Emacs keys and `tab` bound to the
/// next table cell on a table row, the cursor at 0.
async fn live(
    text: &str,
    cx: &mut TestAppContext,
) -> (Entity<Buffer>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_emacs::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
        cx.default_global::<MinibufferRouting>()
            .set_routes(Vec::new());
        for path in [
            "keymaps/linux/emacs.json",
            "keymaps/specific-overrides.json",
        ] {
            let bindings =
                KeymapFile::load_asset_allow_partial_failure(path, cx).expect("the keymap loads");
            cx.bind_keys(bindings);
        }
        cx.bind_keys([KeyBinding::new(
            "tab",
            markdown::NextTableCell,
            Some("Editor && markdown_table"),
        )]);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs::Fs::create_dir(fs.as_ref(), Path::new("/root"))
        .await
        .unwrap();
    fs.insert_file("/root/doc.md", text.as_bytes().to_vec())
        .await;
    let project = Project::test(app_state.fs.clone(), [Path::new("/root")], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let buffer = project
        .update(&mut cx, |project, cx| {
            project.open_local_buffer("/root/doc.md", cx)
        })
        .await
        .unwrap();
    buffer.update(&mut cx, |buffer, cx| {
        buffer.set_language(Some(markdown_language()), cx)
    });
    cx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let editor =
            cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (buffer, editor, cx)
}

fn keys(keys: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

fn text_of(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) -> String {
    buffer.read_with(cx, |buffer, _| buffer.text())
}

/// The conceals on `editor`, as buffer ranges in order.
fn conceals(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<usize>> {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let buffer = display.buffer_snapshot();
        display
            .folds_in_range(MultiBufferOffset(0)..buffer.len())
            .filter(|fold| fold.placeholder.type_tag == Some(conceal_tag()))
            .map(|fold| fold.range.start.to_offset(buffer).0..fold.range.end.to_offset(buffer).0)
            .collect()
    })
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selections = editor.selections.all::<MultiBufferOffset>(&display);
        assert_eq!(selections.len(), 1);
        selections[0].head().0
    })
}

fn head_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.all::<Point>(&display)[0].head().row
    })
}

fn set_cursor(editor: &Entity<Editor>, offset: usize, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
        })
    });
    cx.run_until_parked();
}

/// The buffer offsets of the pipes of the line starting at `line_start` in `text`.
fn pipes_of_line(text: &str, line_start: usize) -> Vec<usize> {
    let line = text[line_start..].lines().next().unwrap_or("");
    line.match_indices('|')
        .map(|(at, _)| line_start + at)
        .collect()
}

/// The pipes of the line holding the first `needle`.
fn pipes_at(text: &str, needle: &str) -> Vec<usize> {
    let at = text.find(needle).expect("needle in text");
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    pipes_of_line(text, line_start)
}

/// Where each pipe ends at the last paint: the x of the character after it.
fn pipe_ends(editor: &Entity<Editor>, pipes: &[usize], cx: &mut VisualTestContext) -> Vec<f32> {
    editor.update(cx, |editor, _| {
        pipes
            .iter()
            .map(|pipe| {
                let x = editor
                    .painted_x(MultiBufferOffset(pipe + 1))
                    .expect("the row was painted");
                f32::from(x)
            })
            .collect()
    })
}

/// Fails unless the rows' pipes end at the same x, pipe by pipe. A pipe's width is whole pixels,
/// so each end lies within half a pixel of its column's edge, and two ends less than a pixel
/// apart.
fn assert_aligned(rows: &[Vec<f32>]) {
    for row in &rows[1..] {
        assert_eq!(row.len(), rows[0].len(), "{rows:?}");
        for (a, b) in rows[0].iter().zip(row) {
            assert!((a - b).abs() < 1.0, "pipes end apart: {rows:?}");
        }
    }
}

const TABLE: &str = "intro\n\n| name | n |\n|---|---|\n| a much longer cell | 1 |\n| b | 22 |\n\nend\n";

#[gpui::test]
async fn two_rows_pipes_end_at_equal_x(cx: &mut TestAppContext) {
    let (_, editor, mut cx) = live(TABLE, cx).await;
    let cx = &mut cx;
    let header = pipe_ends(&editor, &pipes_at(TABLE, "| name"), cx);
    let long = pipe_ends(&editor, &pipes_at(TABLE, "| a much"), cx);
    let short = pipe_ends(&editor, &pipes_at(TABLE, "| b |"), cx);
    assert_aligned(&[header.clone(), long, short]);
    // The short cells were padded: the header's second pipe ends past where its text would.
    let unpadded = editor.update(cx, |editor, _| {
        let after_name = TABLE.find("name").unwrap() + "name ".len();
        f32::from(editor.painted_x(MultiBufferOffset(after_name)).unwrap())
    });
    assert!(header[1] > unpadded + 50.0, "{header:?} vs {unpadded}");
}

#[gpui::test]
async fn the_row_under_point_shows_raw_pipes(cx: &mut TestAppContext) {
    let (_, editor, mut cx) = live(TABLE, cx).await;
    let cx = &mut cx;
    let short = pipes_at(TABLE, "| b |");
    let header = pipes_at(TABLE, "| name");
    let all = conceals(&editor, cx);
    for pipe in short.iter().chain(&header) {
        assert!(all.contains(&(*pipe..pipe + 1)), "{pipe} in {all:?}");
    }

    set_cursor(&editor, TABLE.find("| b |").unwrap() + 2, cx);
    let shown = conceals(&editor, cx);
    for pipe in &short {
        assert!(!shown.contains(&(*pipe..pipe + 1)), "{pipe} in {shown:?}");
    }
    for pipe in &header {
        assert!(shown.contains(&(*pipe..pipe + 1)), "{pipe} in {shown:?}");
    }
    let row = TABLE[..TABLE.find("| b |").unwrap()].matches('\n').count();
    let display = editor.update(cx, |editor, cx| editor.display_text(cx));
    assert_eq!(display.lines().nth(row), Some("| b | 22 |"), "{display}");
}

#[gpui::test]
async fn ctrl_n_moves_one_row_per_press_through_a_table(cx: &mut TestAppContext) {
    let (_, editor, mut cx) = live(TABLE, cx).await;
    let cx = &mut cx;
    let rows = TABLE.lines().count() as u32;
    for expected in 1..rows {
        keys("ctrl-n", cx);
        assert_eq!(head_row(&editor, cx), expected, "C-n never skips a row");
    }
    for expected in (0..rows - 1).rev() {
        keys("ctrl-p", cx);
        assert_eq!(head_row(&editor, cx), expected, "C-p never skips a row");
    }
}

#[gpui::test]
async fn tab_across_a_grid_row_moves_cell_to_cell(cx: &mut TestAppContext) {
    let (buffer, editor, mut cx) = live(TABLE, cx).await;
    let cx = &mut cx;
    let at = |needle: &str| TABLE.find(needle).unwrap();
    set_cursor(&editor, at("a much"), cx);
    // The other rows are a grid while point crosses this one.
    let header = pipes_at(TABLE, "| name");
    assert!(conceals(&editor, cx).contains(&(header[1]..header[1] + 1)));

    for expected in [at("1 |"), at("b |"), at("22 |")] {
        keys("tab", cx);
        assert_eq!(head(&editor, cx), expected);
    }
    assert_eq!(text_of(&buffer, cx), TABLE, "cell moves edit nothing");
}

#[gpui::test]
async fn an_edit_widening_a_cell_re_pads_its_column(cx: &mut TestAppContext) {
    let (buffer, editor, mut cx) = live(TABLE, cx).await;
    let cx = &mut cx;
    let before = pipe_ends(&editor, &pipes_at(TABLE, "| name"), cx);

    let at = TABLE.find("| 22 |").unwrap() + 2;
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(at..at, "1234567890123456")], None, cx)
    });
    cx.run_until_parked();
    let text = text_of(&buffer, cx);
    let header = pipe_ends(&editor, &pipes_at(&text, "| name"), cx);
    let long = pipe_ends(&editor, &pipes_at(&text, "| a much"), cx);
    let short = pipe_ends(&editor, &pipes_at(&text, "| b |"), cx);
    assert_aligned(&[header.clone(), long, short]);
    assert!((header[1] - before[1]).abs() < 1.0, "column 1 kept: {header:?}");
    assert!(header[2] > before[2] + 50.0, "column 2 widened: {header:?}");
}

#[gpui::test]
async fn a_table_larger_than_the_window_pads_only_rows_in_the_window(cx: &mut TestAppContext) {
    let rows = 400usize;
    let mut text = String::from("intro\n\n| key | value |\n|---|---|\n");
    for row in 0..rows {
        text.push_str(&format!("| k{row} | v |\n"));
    }
    text.push_str("\nend\n");
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    let screen = editor.update(cx, |editor, _| {
        editor
            .visible_line_count()
            .map_or(FALLBACK_SCREEN_ROWS, |lines| lines.ceil() as u32)
    });
    assert!((2 * screen as usize) < rows, "the table is larger than the window");

    let piped_rows = |editor: &Entity<Editor>, cx: &mut VisualTestContext| -> Vec<u32> {
        let snapshot = editor.update(cx, |editor, cx| editor.buffer().read(cx).snapshot(cx));
        let mut rows: Vec<u32> = conceals(editor, cx)
            .into_iter()
            .filter(|range| range.len() == 1 && text[range.clone()] == *"|")
            .map(|range| snapshot.offset_to_point(MultiBufferOffset(range.start)).row)
            .collect();
        rows.dedup();
        rows
    };
    // From the top, the window is the screen and one screen below; the delimiter row (3) has no
    // pipe placeholders.
    let expected: Vec<u32> = (2..2 * screen).filter(|row| *row != 3).collect();
    assert_eq!(piped_rows(&editor, cx), expected);

    editor.update_in(cx, |editor, window, cx| {
        editor.set_scroll_position(point(0.0, 200.0), window, cx);
    });
    cx.run_until_parked();
    let top = editor.update(cx, |editor, cx| editor.scroll_position(cx).y as u32);
    assert!(top > screen, "scrolled to {top}");
    let expected: Vec<u32> = (top - screen..top + 2 * screen).collect();
    assert_eq!(piped_rows(&editor, cx), expected);

    // The rows on screen line up.
    let first = text.find(&format!("| k{} |", top)).unwrap();
    let second = text.find(&format!("| k{} |", top + 1)).unwrap();
    let a = pipe_ends(&editor, &pipes_of_line(&text, first), cx);
    let b = pipe_ends(&editor, &pipes_of_line(&text, second), cx);
    assert_aligned(&[a, b]);
}
