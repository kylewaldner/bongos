//! Tests of images below their links: the block under a local image's link, its height in rows,
//! the line moves across it, and the cases that get no block.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use editor::{Bias, Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle, px,
    size,
};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher, Point};
use project::Project;
use settings::KeymapFile;
use workspace::{MinibufferRouting, MultiWorkspace};

use crate::{
    ImageSize, LIVE_FONT_SIZE, LIVE_LINE_HEIGHT, MAX_IMAGE_ROWS, Mode, conceal_tag, image_rows,
    set_mode,
};

fn markdown() -> Arc<Language> {
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

/// Writes a gray PNG of `width` by `height` pixels to `dir/name` and gives its path.
fn png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let path = dir.join(name);
    image::GrayImage::from_pixel(width, height, image::Luma([128]))
        .save(&path)
        .expect("the png is written");
    path
}

/// A live editor on `/root/doc.md` (a fake file) holding `text`, the cursor at 0.
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
        buffer.set_language(Some(markdown()), cx)
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

/// The display rows between buffer row `row` and the next buffer row: the block under it.
fn rows_below(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let here = display.point_to_display_point(Point::new(row, 0), Bias::Left);
        let next = display.point_to_display_point(Point::new(row + 1, 0), Bias::Left);
        next.row().0 - here.row().0 - 1
    })
}

/// Every display row that holds no buffer row.
fn block_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let buffer_rows = display.buffer_snapshot().max_point().row + 1;
        display.max_point().row().0 + 1 - buffer_rows
    })
}

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

fn head_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selections = editor.selections.all::<Point>(&display);
        assert_eq!(selections.len(), 1);
        selections[0].head().row
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

/// The editor's width, as the image layer reads it.
fn text_width(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f32 {
    editor.update(cx, |editor, _| {
        f32::from(editor.last_bounds().expect("laid out").size.width)
    })
}

fn line_height() -> f32 {
    (LIVE_FONT_SIZE * LIVE_LINE_HEIGHT).round()
}

fn expected_rows(width: u32, height: u32, text_width: f32) -> u32 {
    image_rows(ImageSize { width, height }, text_width, line_height()).0
}

#[gpui::test]
async fn a_local_image_gets_a_block_and_its_link_hides_off_its_line(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = png(dir.path(), "small.png", 40, 100);
    let link = format!("![pic]({})", path.display());
    let text = format!("intro\n{link}\nafter\n");
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;

    let rows = expected_rows(40, 100, text_width(&editor, cx));
    assert_eq!(rows, (100.0 / line_height()).ceil() as u32);
    assert_eq!(rows_below(&editor, 1, cx), rows);
    assert_eq!(block_rows(&editor, cx), rows);
    assert_eq!(head_row(&editor, cx), 0, "point stays where it was");
    let start = text.find("![").unwrap();
    assert!(
        conceals(&editor, cx).contains(&(start..start + link.len())),
        "the link hides while point is off its line: {:?}",
        conceals(&editor, cx)
    );

    set_cursor(&editor, start, cx);
    assert!(
        conceals(&editor, cx)
            .iter()
            .all(|conceal| conceal.end <= start || conceal.start >= start + link.len()),
        "the link shows with point on it"
    );
    assert_eq!(rows_below(&editor, 1, cx), rows, "the block stays");
}

#[gpui::test]
async fn line_moves_cross_the_block_in_one_press(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = png(dir.path(), "pic.png", 30, 90);
    let text = format!("intro\n![pic]({})\nafter\nlast\n", path.display());
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    assert!(rows_below(&editor, 1, cx) > 1);

    keys("ctrl-n", cx);
    assert_eq!(head_row(&editor, cx), 1);
    keys("ctrl-n", cx);
    assert_eq!(head_row(&editor, cx), 2, "C-n crosses the block");
    keys("ctrl-p", cx);
    assert_eq!(head_row(&editor, cx), 1, "C-p returns to the link line");
    keys("ctrl-p", cx);
    assert_eq!(head_row(&editor, cx), 0);
}

#[gpui::test]
async fn missing_and_remote_images_get_no_block_and_stay_as_text(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.png");
    let local = format!("![gone]({})", missing.display());
    let remote = "![web](https://example.com/pic.png)";
    let text = format!("intro\n{local}\n{remote}\nafter\n");
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;

    assert_eq!(block_rows(&editor, cx), 0);
    let display = editor.update(cx, |editor, cx| editor.display_text(cx));
    assert!(display.contains(&local), "{display}");
    assert!(display.contains(remote), "{display}");
    assert!(
        conceals(&editor, cx).is_empty(),
        "{:?}",
        conceals(&editor, cx)
    );
}

#[gpui::test]
async fn editing_the_path_swaps_the_block(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let short = png(dir.path(), "a.png", 20, 40);
    png(dir.path(), "b.png", 20, 160);
    let text = format!("intro\n![pic]({})\nafter\n", short.display());
    let (buffer, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    let width = text_width(&editor, cx);
    let short_rows = expected_rows(20, 40, width);
    let tall_rows = expected_rows(20, 160, width);
    assert_ne!(short_rows, tall_rows);
    assert_eq!(rows_below(&editor, 1, cx), short_rows);

    let name = text.find("a.png").unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(name..name + 1, "b")], None, cx)
    });
    cx.run_until_parked();
    assert_eq!(rows_below(&editor, 1, cx), tall_rows);
    assert_eq!(block_rows(&editor, cx), tall_rows, "one block");

    let end = text.find(")\nafter").unwrap();
    buffer.update(cx, |buffer, cx| buffer.edit([(end..end, "x")], None, cx));
    cx.run_until_parked();
    assert_eq!(
        block_rows(&editor, cx),
        0,
        "a path that does not exist has no block"
    );
}

#[gpui::test]
async fn a_narrower_editor_gives_fewer_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = png(dir.path(), "wide.png", 3000, 300);
    let text = format!("intro\n![wide]({})\nafter\n", path.display());
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    let wide = text_width(&editor, cx);
    assert!(wide < 3000.0, "the image is wider than the editor");
    let before = rows_below(&editor, 1, cx);
    assert_scaled_to(before, wide);

    cx.simulate_resize(size(px(wide / 2.0), px(800.0)));
    cx.run_until_parked();
    let narrow = text_width(&editor, cx);
    assert!(narrow < wide, "{narrow} < {wide}");
    let after = rows_below(&editor, 1, cx);
    assert_scaled_to(after, narrow);
    assert!(after < before, "{after} < {before}");
}

/// Fails unless `rows` is the height of the 3000 by 300 image scaled to the text area of an
/// editor `width` wide (the editor less its right margin, at most `TEXT_MARGIN` pixels).
fn assert_scaled_to(rows: u32, width: f32) {
    const TEXT_MARGIN: f32 = 64.0;
    let most = expected_rows(3000, 300, width);
    let least = expected_rows(3000, 300, width - TEXT_MARGIN);
    assert!(
        (least..=most).contains(&rows),
        "{rows} rows for an editor {width} wide, expected {least}..={most}"
    );
}

#[gpui::test]
async fn source_mode_removes_the_blocks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let first = png(dir.path(), "one.png", 30, 60);
    let second = png(dir.path(), "two.png", 30, 120);
    let text = format!(
        "![one]({})\ntext\n![two]({})\n",
        first.display(),
        second.display()
    );
    let (buffer, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    let live_rows = block_rows(&editor, cx);
    assert!(live_rows > 0);

    cx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    cx.run_until_parked();
    assert_eq!(block_rows(&editor, cx), 0);
    assert!(conceals(&editor, cx).is_empty());

    cx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    cx.run_until_parked();
    assert_eq!(block_rows(&editor, cx), live_rows);
}

#[gpui::test]
async fn a_tall_image_is_capped_at_twenty_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = png(dir.path(), "tall.png", 10, 2000);
    let text = format!("intro\n![tall]({})\nafter\n", path.display());
    let (_, editor, mut cx) = live(&text, cx).await;
    let cx = &mut cx;
    assert!(2000.0 / line_height() > MAX_IMAGE_ROWS as f32);
    assert_eq!(rows_below(&editor, 1, cx), MAX_IMAGE_ROWS);
}
