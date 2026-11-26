//! Tests of the live view's styles, its window and the background parse of the index.

use std::ops::Range;
use std::sync::Arc;

use editor::{Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    AppContext as _, Entity, FontWeight, HighlightStyle, TestAppContext, VisualTestContext, point,
};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher};
use theme::ActiveTheme as _;
use workspace::AppState;
use zed_actions::editor::{MoveDown, MoveUp};

use crate::{
    FALLBACK_SCREEN_ROWS, MarkdownCodeBlock, Mode, ParseStats, Style, fresh_index, parse_stats,
    set_mode,
};

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let _ = AppState::test(cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
    });
}

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

/// A Markdown buffer in live mode with an editor on it, its first parse done.
fn live_editor<'a>(
    text: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<Buffer>, Entity<Editor>, &'a mut VisualTestContext) {
    let text = text.to_string();
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(text, cx);
        buffer.set_language(Some(markdown()), cx);
        buffer
    });
    cx.update(|cx| set_mode(&buffer, Mode::Live, cx));
    let for_editor = buffer.clone();
    let (editor, vcx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(for_editor, None, window, cx));
    vcx.run_until_parked();
    (buffer, editor, vcx)
}

/// The buffer ranges of `style`'s highlight on `editor`.
fn styled(editor: &Entity<Editor>, style: Style, cx: &mut VisualTestContext) -> Vec<Range<usize>> {
    editor.read_with(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .text_highlights(style.key(), cx)
            .map(|(_, ranges)| {
                ranges
                    .iter()
                    .map(|range| {
                        range.start.to_offset(&snapshot).0..range.end.to_offset(&snapshot).0
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn code_block_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, cx| {
        editor.highlighted_rows::<MarkdownCodeBlock>(cx).count()
    })
}

fn stats(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> ParseStats {
    editor.read_with(cx, |editor, _| parse_stats(editor).expect("attached"))
}

/// The range of the first `needle` in `text`, less `open` bytes at its start and `close` at its
/// end.
fn inner(text: &str, needle: &str, open: usize, close: usize) -> Range<usize> {
    let start = text.find(needle).expect("needle in text");
    start + open..start + needle.len() - close
}

fn edit(buffer: &Entity<Buffer>, range: Range<usize>, new: &str, cx: &mut VisualTestContext) {
    buffer.update(cx, |buffer, cx| buffer.edit([(range, new)], None, cx));
}

const SAMPLE: &str = "# Title\n\n#### Small\n\n**bold** and [link](http://x.y) and `code` \
    and *it* and ~~gone~~\n\n- [x] done\n- [ ] open\n\n> quoted\n\n```\nlet x;\n```\n";

#[gpui::test]
async fn heading_bold_link_and_code_get_their_keys_over_the_right_ranges(cx: &mut TestAppContext) {
    init(cx);
    let (_, editor, vcx) = live_editor(SAMPLE, cx);
    let text = SAMPLE;
    assert_eq!(styled(&editor, Style::HeadingMajor, vcx), vec![2..7]);
    assert_eq!(
        styled(&editor, Style::HeadingMinor, vcx),
        vec![inner(text, "#### Small", 5, 0)]
    );
    assert_eq!(
        styled(&editor, Style::Strong, vcx),
        vec![inner(text, "**bold**", 2, 2)]
    );
    assert_eq!(
        styled(&editor, Style::Link, vcx),
        vec![inner(text, "[link](http://x.y)", 1, 13)]
    );
    assert_eq!(
        styled(&editor, Style::Code, vcx),
        vec![inner(text, "`code`", 1, 1)]
    );
    assert_eq!(
        styled(&editor, Style::Emphasis, vcx),
        vec![inner(text, "*it*", 1, 1)]
    );
    assert_eq!(
        styled(&editor, Style::Strike, vcx),
        vec![inner(text, "~~gone~~", 2, 2)]
    );
    assert_eq!(
        styled(&editor, Style::CheckedTask, vcx),
        vec![inner(text, "- [x] done", 6, 0)]
    );
    assert_eq!(styled(&editor, Style::Quote, vcx).len(), 1);
    assert_eq!(code_block_rows(&editor, vcx), 1);

    // The looks come from the theme.
    editor.read_with(vcx, |editor, cx| {
        let colors = cx.theme().colors();
        let (heading, _) = editor
            .text_highlights(Style::HeadingMajor.key(), cx)
            .unwrap();
        assert_eq!(heading.font_weight, Some(FontWeight(700.0)));
        assert_eq!(heading.color, Some(colors.text));
        let (minor, _) = editor
            .text_highlights(Style::HeadingMinor.key(), cx)
            .unwrap();
        assert_eq!(minor.font_weight, Some(FontWeight(600.0)));
        let (strong, _) = editor.text_highlights(Style::Strong.key(), cx).unwrap();
        assert_eq!(strong.font_weight, Some(FontWeight(700.0)));
        let (link, _) = editor.text_highlights(Style::Link.key(), cx).unwrap();
        assert_eq!(link.color, Some(colors.text_accent));
        assert!(link.underline.is_some());
        let (code, _) = editor.text_highlights(Style::Code.key(), cx).unwrap();
        assert_eq!(code.background_color, Some(colors.element_background));
        let (done, _) = editor
            .text_highlights(Style::CheckedTask.key(), cx)
            .unwrap();
        assert_eq!(done.color, Some(colors.text_muted));
        assert!(done.strikethrough.is_some());
    });
}

#[gpui::test]
async fn scrolling_a_long_buffer_moves_the_styled_window(cx: &mut TestAppContext) {
    init(cx);
    let lines = 400u32;
    let text: String = (0..lines).map(|row| format!("**b{row}**\n")).collect();
    let (buffer, editor, vcx) = live_editor(&text, cx);
    let rows_styled = |editor: &Entity<Editor>, vcx: &mut VisualTestContext| -> Vec<u32> {
        let snapshot = buffer.read_with(vcx, |buffer, _| buffer.snapshot());
        styled(editor, Style::Strong, vcx)
            .into_iter()
            .map(|range| snapshot.offset_to_point(range.start).row)
            .collect()
    };
    let screen = editor.read_with(vcx, |editor, _| {
        editor
            .visible_line_count()
            .map_or(FALLBACK_SCREEN_ROWS, |lines| lines.ceil() as u32)
    });
    assert_eq!(
        rows_styled(&editor, vcx),
        (0..screen * 2).collect::<Vec<_>>()
    );

    editor.update_in(vcx, |editor, window, cx| {
        editor.set_scroll_position(point(0.0, 200.0), window, cx);
    });
    vcx.run_until_parked();
    let top = editor.update(vcx, |editor, cx| editor.scroll_position(cx).y as u32);
    assert!(top > screen, "scrolled to {top}");
    let expected: Vec<u32> = (top - screen..(top + 2 * screen).min(lines)).collect();
    assert_eq!(rows_styled(&editor, vcx), expected);
}

#[gpui::test]
async fn an_edit_reparses_and_restyles(cx: &mut TestAppContext) {
    init(cx);
    let (buffer, editor, vcx) = live_editor("plain text\n", cx);
    assert!(styled(&editor, Style::Strong, vcx).is_empty());
    let before = stats(&editor, vcx);

    edit(&buffer, 0..0, "**x** ", vcx);
    vcx.run_until_parked();
    assert_eq!(styled(&editor, Style::Strong, vcx), vec![2..3]);
    let after = stats(&editor, vcx);
    assert_eq!(after.started, before.started + 1);
    assert_eq!(after.applied, before.applied + 1);

    // Removing the markup removes the style.
    edit(&buffer, 0..6, "", vcx);
    vcx.run_until_parked();
    assert!(styled(&editor, Style::Strong, vcx).is_empty());
}

#[gpui::test]
async fn a_result_for_an_old_version_is_dropped(cx: &mut TestAppContext) {
    init(cx);
    let (buffer, editor, vcx) = live_editor("one\n", cx);
    let before = stats(&editor, vcx);

    // The parse of "**a** one" is in flight when the buffer changes again.
    edit(&buffer, 0..0, "**a** ", vcx);
    assert_eq!(stats(&editor, vcx).started, before.started + 1);
    edit(&buffer, 0..0, "*e* ", vcx);
    vcx.run_until_parked();

    let after = stats(&editor, vcx);
    assert_eq!(after.dropped, before.dropped + 1, "{after:?}");
    assert_eq!(
        after.applied,
        before.applied + 1,
        "only the current result is applied"
    );
    let text = buffer.read_with(vcx, |buffer, _| buffer.text());
    let index = editor.read_with(vcx, |editor, cx| fresh_index(editor, cx).unwrap());
    assert_eq!(*index, bongos_markdown_model::parse(&text));
    assert_eq!(styled(&editor, Style::Emphasis, vcx), vec![1..2]);
    assert_eq!(styled(&editor, Style::Strong, vcx), vec![6..7]);
}

#[gpui::test]
async fn two_edits_during_one_parse_cause_exactly_one_more_parse(cx: &mut TestAppContext) {
    init(cx);
    let (buffer, editor, vcx) = live_editor("text\n", cx);
    let before = stats(&editor, vcx);

    edit(&buffer, 0..0, "a", vcx);
    edit(&buffer, 0..0, "b", vcx);
    edit(&buffer, 0..0, "c", vcx);
    vcx.run_until_parked();

    let after = stats(&editor, vcx);
    assert_eq!(after.started, before.started + 2, "{after:?}");
    assert_eq!(after.dropped, before.dropped + 1);
    assert_eq!(after.applied, before.applied + 1);
}

#[gpui::test]
async fn source_mode_leaves_no_highlight(cx: &mut TestAppContext) {
    init(cx);
    let (buffer, editor, vcx) = live_editor(SAMPLE, cx);
    assert!(!styled(&editor, Style::Strong, vcx).is_empty());
    assert_eq!(code_block_rows(&editor, vcx), 1);

    vcx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    vcx.run_until_parked();
    for style in Style::ALL {
        assert!(styled(&editor, style, vcx).is_empty(), "{style:?}");
    }
    assert_eq!(code_block_rows(&editor, vcx), 0);

    // An edit or a scroll in source mode styles nothing.
    edit(&buffer, 0..0, "**s** ", vcx);
    editor.update_in(vcx, |editor, window, cx| {
        editor.set_scroll_position(point(0.0, 1.0), window, cx);
    });
    vcx.run_until_parked();
    for style in Style::ALL {
        assert!(styled(&editor, style, vcx).is_empty(), "{style:?}");
    }

    // Back to live, the styles return over the edited text.
    vcx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    vcx.run_until_parked();
    assert_eq!(styled(&editor, Style::Strong, vcx)[0], 2..3);
}

#[gpui::test]
async fn the_buffer_text_is_unchanged_by_the_live_view(cx: &mut TestAppContext) {
    init(cx);
    let text = SAMPLE.repeat(30);
    let (buffer, editor, vcx) = live_editor(&text, cx);
    let version = buffer.read_with(vcx, |buffer, _| buffer.version());
    editor.update_in(vcx, |editor, window, cx| {
        editor.set_scroll_position(point(0.0, 150.0), window, cx);
    });
    vcx.run_until_parked();
    let end = buffer.read_with(vcx, |buffer, _| buffer.len());
    editor.update_in(vcx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections
                .select_ranges([editor::MultiBufferOffset(end)..editor::MultiBufferOffset(end)])
        });
    });
    vcx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    vcx.run_until_parked();
    vcx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    vcx.run_until_parked();
    buffer.read_with(vcx, |buffer, _| {
        assert_eq!(buffer.text(), text);
        assert_eq!(buffer.version(), version);
        assert!(!buffer.is_dirty());
    });
}

/// The look and the ranges of `style`'s highlight on `editor`.
fn look(
    editor: &Entity<Editor>,
    style: Style,
    cx: &mut VisualTestContext,
) -> Option<HighlightStyle> {
    editor.read_with(cx, |editor, cx| {
        editor
            .text_highlights(style.key(), cx)
            .map(|(look, _)| look)
    })
}

#[gpui::test]
async fn a_code_span_and_a_code_block_are_highlighted_in_the_code_font(cx: &mut TestAppContext) {
    init(cx);
    let (_, editor, vcx) = live_editor(SAMPLE, cx);
    let text = SAMPLE;
    assert_eq!(
        styled(&editor, Style::Code, vcx),
        vec![inner(text, "`code`", 1, 1)]
    );
    for style in [Style::Code, Style::CodeBlockText] {
        let look = look(&editor, style, vcx).expect("highlighted");
        assert_eq!(look.monospace, Some(true), "{style:?}");
    }
    let block = styled(&editor, Style::CodeBlockText, vcx);
    assert_eq!(block.len(), 1);
    assert!(text[block[0].clone()].contains("let x;"));
    for style in [
        Style::Strong,
        Style::Emphasis,
        Style::Link,
        Style::HeadingMajor,
    ] {
        assert_eq!(
            look(&editor, style, vcx).and_then(|look| look.monospace),
            None
        );
    }
}

#[gpui::test]
async fn source_mode_has_no_code_font_highlight(cx: &mut TestAppContext) {
    init(cx);
    let (buffer, editor, vcx) = live_editor(SAMPLE, cx);
    assert!(look(&editor, Style::Code, vcx).is_some());
    vcx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    vcx.run_until_parked();
    for style in [Style::Code, Style::CodeBlockText] {
        assert!(look(&editor, style, vcx).is_none(), "{style:?}");
    }
}

#[gpui::test]
async fn moving_down_and_up_from_a_line_with_a_code_span_keeps_the_goal_column(
    cx: &mut TestAppContext,
) {
    init(cx);
    let text = "plain text line\nab `code` cd\nplain text line\n";
    let (_, editor, vcx) = live_editor(text, cx);
    let line = "plain text line\n".len();
    let column = "ab `code` c".len();
    let cursor = |editor: &Entity<Editor>, vcx: &mut VisualTestContext| {
        editor.update(vcx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<MultiBufferOffset>(&snapshot)
                .head()
                .0
        })
    };
    let place = |editor: &Entity<Editor>, offset: usize, vcx: &mut VisualTestContext| {
        editor.update_in(vcx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
            });
        });
        vcx.run_until_parked();
    };

    place(&editor, line + column, vcx);
    editor.update_in(vcx, |editor, window, cx| {
        editor.move_down(&MoveDown, window, cx)
    });
    assert_eq!(cursor(&editor, vcx), line + "ab `code` cd\n".len() + column);

    place(&editor, line + column, vcx);
    editor.update_in(vcx, |editor, window, cx| {
        editor.move_up(&MoveUp, window, cx)
    });
    assert_eq!(cursor(&editor, vcx), column);
}
