//! Tests of hidden markup with reveal at point: the conceals around the cursor as it moves by
//! keys, clicks and isearch, and the kill and yank commands, which see the text as typed.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use editor::{Bias, Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    AppContext as _, Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext,
    WindowHandle, point, px,
};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher};
use project::Project;
use settings::KeymapFile;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::{Mode, conceal_tag, set_mode};

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

/// Starts the app with the Emacs keys, isearch and the markdown addon.
fn init(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_emacs::init(cx);
        bongos_isearch::init(cx);
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
    })
}

/// A workspace on a project holding `/root/<name>` for each file.
async fn workspace(
    files: &[(&str, &str)],
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<Project>,
    Arc<AppState>,
    VisualTestContext,
) {
    let app_state = init(cx);
    let fs = app_state.fs.as_fake();
    fs::Fs::create_dir(fs.as_ref(), Path::new("/root"))
        .await
        .unwrap();
    for (name, text) in files {
        fs.insert_file(format!("/root/{name}"), text.as_bytes().to_vec())
            .await;
    }
    let project = Project::test(app_state.fs.clone(), [Path::new("/root")], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    (workspace, project, app_state, cx)
}

/// Opens `/root/<name>` as Markdown in `mode`, in a focused editor, the cursor at 0.
async fn open(
    name: &str,
    mode: Mode,
    workspace: &Entity<Workspace>,
    project: &Entity<Project>,
    cx: &mut VisualTestContext,
) -> (Entity<Buffer>, Entity<Editor>) {
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(format!("/root/{name}"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| buffer.set_language(Some(markdown()), cx));
    cx.update(|_, cx| set_mode(&buffer, mode, cx));
    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let editor =
            cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (buffer, editor)
}

/// A workspace with one live Markdown file of `text`.
async fn live(
    text: &str,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let (workspace, project, _, mut cx) = workspace(&[("doc.md", text)], cx).await;
    let (_, editor) = open("doc.md", Mode::Live, &workspace, &project, &mut cx).await;
    (workspace, editor, cx)
}

fn keys(keys: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
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

/// Every selection as `tail..head`.
fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<usize>> {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor
            .selections
            .all::<MultiBufferOffset>(&display)
            .iter()
            .map(|selection| selection.tail().0..selection.head().0)
            .collect()
    })
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    let selections = selections(editor, cx);
    assert_eq!(selections.len(), 1, "one selection: {selections:?}");
    selections[0].end
}

fn set_cursor(editor: &Entity<Editor>, offset: usize, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
        })
    });
    cx.run_until_parked();
}

fn display_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.display_text(cx))
}

/// Fails when the cursor sits strictly inside a conceal, or where the display cannot show it.
fn assert_point_visible(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    let head = head(editor, cx);
    for conceal in conceals(editor, cx) {
        assert!(
            !(conceal.start < head && head < conceal.end),
            "point {head} inside the conceal {conceal:?}"
        );
    }
    let round_trip = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let buffer = display.buffer_snapshot();
        let point = buffer.offset_to_point(MultiBufferOffset(head));
        let shown = display.point_to_display_point(point, Bias::Left);
        let back = display.display_point_to_point(shown, Bias::Left);
        buffer.point_to_offset(back).0
    });
    assert_eq!(
        round_trip, head,
        "point {head} is not where the display shows it"
    );
}

fn newest_kill(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| {
        editor::kill_ring::read(cx)
            .and_then(|ring| ring.get(0))
            .map(|entry| entry.text.clone())
    })
}

#[gpui::test]
async fn forward_char_reaches_the_boundary_then_steps_through_the_markup(cx: &mut TestAppContext) {
    let text = "a **bold** c\n";
    let (_, editor, mut cx) = live(text, cx).await;
    let cx = &mut cx;
    assert_eq!(head(&editor, cx), 0);
    assert_eq!(display_text(&editor, cx), "a ⋯bold⋯ c\n");
    assert_eq!(conceals(&editor, cx), vec![2..4, 8..10]);

    keys("ctrl-f", cx);
    assert_eq!(head(&editor, cx), 1);
    assert_eq!(display_text(&editor, cx), "a ⋯bold⋯ c\n");

    // On the boundary the asterisks show, and point steps through them one at a time.
    for expected in 2..=10 {
        keys("ctrl-f", cx);
        assert_eq!(head(&editor, cx), expected);
        assert_eq!(display_text(&editor, cx), text, "at {expected}");
        assert!(conceals(&editor, cx).is_empty());
        assert_point_visible(&editor, cx);
    }
    // Past the element, the markup hides again.
    keys("ctrl-f", cx);
    assert_eq!(head(&editor, cx), 11);
    assert_eq!(display_text(&editor, cx), "a ⋯bold⋯ c\n");
}

#[gpui::test]
async fn forward_then_backward_char_returns_to_the_same_point(cx: &mut TestAppContext) {
    let text = "# Head\n\nx **b**[l](http://u.v) y *e*\n";
    let (_, editor, mut cx) = live(text, cx).await;
    let cx = &mut cx;
    for start in 0..text.len() {
        set_cursor(&editor, start, cx);
        assert_point_visible(&editor, cx);
        keys("ctrl-f", cx);
        assert_point_visible(&editor, cx);
        keys("ctrl-b", cx);
        assert_eq!(head(&editor, cx), start, "C-f C-b from {start}");
        assert_point_visible(&editor, cx);
    }
}

#[gpui::test]
async fn line_moves_and_a_click_never_leave_point_in_hidden_text(cx: &mut TestAppContext) {
    let text = "# Heading\n\nSome **boldword** and [a link](http://x.y/z) end\n\n\
        - item **one**\n- [ ] task\n\n> quoted *it*\n\n---\n\n```\ncode\n```\n";
    let (_, editor, mut cx) = live(text, cx).await;
    let cx = &mut cx;
    let rows = text.lines().count();
    for _ in 0..rows {
        for key in ["ctrl-e", "ctrl-a", "ctrl-e", "ctrl-n"] {
            keys(key, cx);
            assert_point_visible(&editor, cx);
        }
    }

    // A click on the bold word reveals its asterisks and leaves the link hidden.
    set_cursor(&editor, 0, cx);
    let link = text.find("](http").unwrap()..text.find(" end").unwrap();
    assert!(conceals(&editor, cx).contains(&link));
    let bold = text.find("boldword").unwrap();
    let position = editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        let buffer = snapshot.buffer_snapshot();
        let target = buffer.offset_to_point(MultiBufferOffset(bold + 3));
        let shown = snapshot.point_to_display_point(target, Bias::Left);
        let at = editor
            .display_to_pixel_point(shown, &snapshot, window, cx)
            .expect("on screen");
        let origin = editor.last_bounds().expect("laid out").origin;
        origin + at + point(px(2.0), px(4.0))
    });
    cx.simulate_click(position, Modifiers::none());
    cx.run_until_parked();
    let clicked = head(&editor, cx);
    assert!(
        (bold..bold + "boldword".len()).contains(&clicked),
        "the click lands in the word: {clicked}"
    );
    assert_point_visible(&editor, cx);
    let shown = conceals(&editor, cx);
    assert!(!shown.contains(&(bold - 2..bold)), "{shown:?}");
    assert!(shown.contains(&link), "the link stays hidden: {shown:?}");
}

#[gpui::test]
async fn isearch_into_a_hidden_url_lands_and_reveals_it(cx: &mut TestAppContext) {
    let text = "see [docs](http://example.com/zebra) now\n";
    let (_, editor, mut cx) = live(text, cx).await;
    let cx = &mut cx;
    let url = text.find("](").unwrap()..text.find(" now").unwrap();
    assert!(conceals(&editor, cx).contains(&url));

    cx.dispatch_action(bongos_isearch::actions::Forward);
    cx.run_until_parked();
    for character in "zebra".chars() {
        cx.simulate_input(&character.to_string());
        cx.run_until_parked();
    }
    let found = text.find("zebra").unwrap();
    let head = selections(&editor, cx)[0].end;
    assert_eq!(head, found + "zebra".len());
    assert!(
        conceals(&editor, cx)
            .iter()
            .all(|conceal| conceal.end <= found || conceal.start >= found + 5),
        "the match shows"
    );
    assert!(display_text(&editor, cx).contains("http://example.com/zebra"));
    assert_point_visible(&editor, cx);

    keys("enter", cx);
    assert_eq!(selections(&editor, cx)[0].end, found + "zebra".len());
    assert!(display_text(&editor, cx).contains("zebra"));
}

#[gpui::test]
async fn an_aborted_isearch_leaves_the_conceals_as_before(cx: &mut TestAppContext) {
    let text = "x\nsee [docs](http://example.com/zebra) and **b**\n";
    let (_, editor, mut cx) = live(text, cx).await;
    let cx = &mut cx;
    let before = conceals(&editor, cx);
    assert!(!before.is_empty());

    cx.dispatch_action(bongos_isearch::actions::Forward);
    cx.run_until_parked();
    for character in "zebra".chars() {
        cx.simulate_input(&character.to_string());
        cx.run_until_parked();
    }
    assert_ne!(
        conceals(&editor, cx),
        before,
        "the landing revealed the url"
    );

    keys("ctrl-g", cx);
    assert_eq!(head(&editor, cx), 0);
    assert_eq!(conceals(&editor, cx), before);
    assert_point_visible(&editor, cx);
}

const KILL_TEXT: &str = "# Notes\n\nSome **bold** and [a link](http://x.y) here.\n\n\
    plain one\nplain two\nplain six\n\n- item one\n- [ ] task\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";

/// Kills, yanks, the mark ring, a rectangle and undo, from the start of the buffer.
const KILL_KEYS: &[&str] = &[
    "ctrl-n ctrl-a ctrl-n ctrl-a",
    "ctrl-k",
    "ctrl-k",
    "ctrl-y",
    "ctrl-n ctrl-a",
    "ctrl-space ctrl-e alt-w",
    "ctrl-a ctrl-space ctrl-f ctrl-f ctrl-n ctrl-x r k",
    "ctrl-e ctrl-x r y",
    "ctrl-n ctrl-a ctrl-k",
    "ctrl-y alt-y",
    "ctrl-space ctrl-space ctrl-n ctrl-a ctrl-n ctrl-a ctrl-x ctrl-x ctrl-g",
    "ctrl-a ctrl-space ctrl-n ctrl-a ctrl-n ctrl-e ctrl-w",
    "ctrl-/",
    "ctrl-n ctrl-a ctrl-n ctrl-a ctrl-k ctrl-k ctrl-y ctrl-y",
];

#[gpui::test]
async fn kill_and_yank_give_the_same_text_in_live_and_source(cx: &mut TestAppContext) {
    let (workspace, project, _, mut cx) =
        workspace(&[("live.md", KILL_TEXT), ("source.md", KILL_TEXT)], cx).await;
    let cx = &mut cx;
    let mut results = Vec::new();
    for (name, mode) in [("live.md", Mode::Live), ("source.md", Mode::Source)] {
        let (buffer, editor) = open(name, mode, &workspace, &project, cx).await;
        if mode == Mode::Live {
            assert!(!conceals(&editor, cx).is_empty());
        } else {
            assert!(conceals(&editor, cx).is_empty());
        }
        for step in KILL_KEYS {
            keys(step, cx);
        }
        results.push(buffer.read_with(cx, |buffer, _| buffer.text()));
    }
    assert_ne!(results[0], KILL_TEXT, "the keys changed the text");
    assert_eq!(results[0], results[1]);
}

#[gpui::test]
async fn a_region_over_a_list_and_a_table_kills_exactly_its_characters(cx: &mut TestAppContext) {
    let (_, editor, mut cx) = live(KILL_TEXT, cx).await;
    let cx = &mut cx;
    let start = KILL_TEXT.find("- item").unwrap();
    let end = KILL_TEXT.find("| 2 |").unwrap() + "| 2 |".len();
    set_cursor(&editor, start, cx);
    keys("ctrl-space", cx);
    for _ in 0..5 {
        keys("ctrl-n", cx);
    }
    keys("ctrl-e", cx);
    assert_eq!(selections(&editor, cx), vec![start..end]);
    keys("ctrl-w", cx);

    assert_eq!(newest_kill(cx).as_deref(), Some(&KILL_TEXT[start..end]));
    let mut expected = KILL_TEXT.to_string();
    expected.replace_range(start..end, "");
    assert_eq!(editor.update(cx, |editor, cx| editor.text(cx)), expected);
}

#[gpui::test]
async fn an_untouched_buffer_stays_clean_and_saves_byte_identical(cx: &mut TestAppContext) {
    let text = "# T\n\n**b** [l](http://u.v) `c`\n\n- [x] done\n> q\n\n---\n\n| a |\n|---|\n";
    let (workspace, project, app_state, mut cx) = workspace(&[("doc.md", text)], cx).await;
    let cx = &mut cx;
    let (buffer, editor) = open("doc.md", Mode::Live, &workspace, &project, cx).await;
    assert!(!conceals(&editor, cx).is_empty());
    for step in [
        "ctrl-n ctrl-n ctrl-e",
        "ctrl-f ctrl-f ctrl-n",
        "ctrl-b ctrl-n ctrl-n",
    ] {
        keys(step, cx);
    }
    cx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    cx.run_until_parked();
    assert!(conceals(&editor, cx).is_empty());
    cx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    cx.run_until_parked();
    assert!(!conceals(&editor, cx).is_empty());
    buffer.read_with(cx, |buffer, _| {
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.text(), text);
    });

    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();
    let saved = app_state
        .fs
        .load_bytes(Path::new("/root/doc.md"))
        .await
        .unwrap();
    assert_eq!(saved, text.as_bytes());
    buffer.read_with(cx, |buffer, _| assert!(!buffer.is_dirty()));
}
