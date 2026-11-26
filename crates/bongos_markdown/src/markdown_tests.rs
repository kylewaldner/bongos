//! Tests of the markdown addon: when it attaches, the per-buffer mode, the live typography and
//! what source mode and detach give back, the settings and the hidden actions.

use std::sync::Arc;

use bongos_emacs::prefix_arg::{self, Meaning};
use command_palette_hooks::CommandPaletteFilter;
use editor::{Editor, MultiBuffer};
use gpui::BorrowAppContext as _;
use gpui::{
    Action as _, AppContext as _, Entity, Focusable as _, TestAppContext, TextStyleRefinement,
    VisualContext as _, VisualTestContext, px, relative,
};
use language::{Buffer, CursorShape, Language, LanguageConfig, LanguageMatcher, LanguageRegistry};
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;
use workspace::AppState;

use crate::{
    LIVE_FONT_SIZE, LIVE_LINE_HEIGHT, MarkdownModes, MarkdownSettings, Mode, action_types,
    is_attached, markdown, mode_for_buffer, set_mode,
};

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let _ = AppState::test(cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
        crate::init_palette(cx);
    });
}

fn set_live_view(cx: &mut gpui::App, live: bool) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.markdown.get_or_insert_default().live_view = Some(live);
        });
    });
}

fn language(name: &'static str, suffix: &str) -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: name.into(),
            matcher: LanguageMatcher {
                path_suffixes: vec![suffix.to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

fn buffer_in(
    text: &str,
    language: Option<Arc<Language>>,
    cx: &mut TestAppContext,
) -> Entity<Buffer> {
    let text = text.to_string();
    cx.new(|cx| {
        let mut buffer = Buffer::local(text, cx);
        buffer.set_language(language, cx);
        buffer
    })
}

fn editor_on<'a>(
    buffer: &Entity<Buffer>,
    cx: &'a mut TestAppContext,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    let buffer = buffer.clone();
    let (editor, cx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(buffer, None, window, cx));
    cx.run_until_parked();
    (editor, cx)
}

/// Another editor on `buffer`, in the window of `vcx`.
fn another_editor_on(buffer: &Entity<Buffer>, vcx: &mut VisualTestContext) -> Entity<Editor> {
    let buffer = buffer.clone();
    let editor = vcx.new_window_entity(|window, cx| Editor::for_buffer(buffer, None, window, cx));
    vcx.run_until_parked();
    editor
}

fn attached(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, _| is_attached(editor))
}

fn refinement(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<TextStyleRefinement> {
    editor.read_with(cx, |editor, _| editor.text_style_refinement().cloned())
}

fn live_size(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f32 {
    let size = refinement(editor, cx)
        .and_then(|refinement| refinement.font_size)
        .expect("a live font size");
    f32::from(size.to_pixels(px(16.0)))
}

#[gpui::test]
async fn defaults_are_live_off_and_five_megabytes(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let settings = MarkdownSettings::get_global(cx);
        assert!(!settings.live_view);
        assert_eq!(settings.live_view_max_file_size, 5_000_000);
        assert!(
            cx.try_global::<MarkdownModes>()
                .is_none_or(|modes| modes.is_empty()),
            "no mode entry before an editor attaches"
        );
    });
}

#[gpui::test]
async fn attaches_on_markdown_only(cx: &mut TestAppContext) {
    init(cx);
    let markdown = language("Markdown", "md");
    // An empty Markdown buffer attaches, and its entry appears then.
    let empty = buffer_in("", Some(markdown.clone()), cx);
    assert!(cx.update(|cx| mode_for_buffer(&empty, cx)).is_none());
    let (editor, vcx) = editor_on(&empty, cx);
    assert!(attached(&editor, vcx));
    let context = editor.update_in(vcx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx);
        editor.key_context(window, cx)
    });
    assert!(context.contains("markdown_mode"), "{context:?}");
    assert_eq!(
        vcx.update(|_, cx| mode_for_buffer(&empty, cx)),
        Some(Mode::Source)
    );

    let rust = buffer_in("fn main() {}", Some(language("Rust", "rs")), vcx);
    let rust_editor = another_editor_on(&rust, vcx);
    assert!(!attached(&rust_editor, vcx));

    // A prompts buffer, in the prompts mode, never gets it.
    let registry = Arc::new(LanguageRegistry::test(vcx.executor()));
    bongos_prompts::register_language(&registry);
    let prompts_language = registry.language_for_name("Prompts").await.unwrap();
    let prompts = buffer_in("=== prompt ===\nhello", Some(prompts_language), vcx);
    let prompts_editor = another_editor_on(&prompts, vcx);
    assert!(prompts_editor.read_with(vcx, |editor, _| bongos_prompts::is_attached(editor)));
    assert!(!attached(&prompts_editor, vcx));
    vcx.update(|_, cx| set_live_view(cx, true));
    vcx.run_until_parked();
    assert!(!attached(&prompts_editor, vcx));

    // A multibuffer editor never gets it, even over Markdown buffers.
    let multi = vcx.update(|_, cx| {
        MultiBuffer::build_multi(
            [
                ("one", vec![language_point_range()]),
                ("two", vec![language_point_range()]),
            ],
            cx,
        )
    });
    for buffer in vcx.update(|_, cx| multi.read(cx).all_buffers()) {
        buffer.update(vcx, |buffer, cx| {
            buffer.set_language(Some(markdown.clone()), cx)
        });
    }
    let multi_editor =
        vcx.new_window_entity(|window, cx| Editor::for_multibuffer(multi, None, window, cx));
    vcx.run_until_parked();
    assert!(!attached(&multi_editor, vcx));
}

fn language_point_range() -> std::ops::Range<language::Point> {
    language::Point::new(0, 0)..language::Point::new(0, 3)
}

#[gpui::test]
async fn live_sets_the_typography_and_source_gives_back_what_was_there(cx: &mut TestAppContext) {
    init(cx);
    let buffer = buffer_in("# title\n\ntext", Some(language("Markdown", "md")), cx);
    let (editor, vcx) = editor_on(&buffer, cx);
    // Something was set before the addon wanted anything.
    editor.update(vcx, |editor, cx| {
        editor.set_cursor_shape_override(Some(CursorShape::Underline), cx);
    });
    assert_eq!(
        refinement(&editor, vcx),
        None,
        "source mode adds no refinement"
    );

    vcx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    vcx.run_until_parked();
    let live = refinement(&editor, vcx).expect("a refinement in live mode");
    let family = vcx.update(|_, cx| {
        if cfg!(target_os = "macos") {
            ".SystemUIFont".into()
        } else {
            ThemeSettings::get_global(cx).ui_font.family.clone()
        }
    });
    assert_eq!(live.font_family, Some(family));
    assert_eq!(live_size(&editor, vcx), LIVE_FONT_SIZE);
    assert_eq!(live.line_height, Some(relative(LIVE_LINE_HEIGHT)));
    editor.read_with(vcx, |editor, _| {
        assert_eq!(editor.cursor_shape_override(), Some(CursorShape::Bar));
        assert!(!editor.show_gutter());
    });

    vcx.update(|_, cx| set_mode(&buffer, Mode::Source, cx));
    vcx.run_until_parked();
    assert_eq!(
        refinement(&editor, vcx),
        None,
        "a None refinement is cleared again"
    );
    editor.read_with(vcx, |editor, _| {
        assert_eq!(editor.cursor_shape_override(), Some(CursorShape::Underline));
        assert!(editor.show_gutter());
    });

    // A refinement that was there comes back, and detaching gives back as well.
    let before = TextStyleRefinement {
        font_size: Some(px(11.0).into()),
        ..Default::default()
    };
    editor.update(vcx, |editor, _| {
        editor.set_text_style_refinement(before.clone())
    });
    vcx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    vcx.run_until_parked();
    assert_eq!(live_size(&editor, vcx), LIVE_FONT_SIZE);
    let rust = language("Rust", "rs");
    buffer.update(vcx, |buffer, cx| buffer.set_language(Some(rust), cx));
    vcx.run_until_parked();
    assert!(!attached(&editor, vcx), "a language change detaches");
    assert_eq!(refinement(&editor, vcx), Some(before));
    editor.read_with(vcx, |editor, _| {
        assert_eq!(editor.cursor_shape_override(), Some(CursorShape::Underline));
        assert!(editor.show_gutter());
    });
}

#[gpui::test]
async fn buffer_font_zoom_rescales_the_live_size_by_the_same_ratio(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| set_live_view(cx, true));
    let buffer = buffer_in("text", Some(language("Markdown", "md")), cx);
    let (editor, vcx) = editor_on(&buffer, cx);
    assert_eq!(live_size(&editor, vcx), LIVE_FONT_SIZE);
    let base =
        vcx.update(|_, cx| f32::from(ThemeSettings::get_global(cx).buffer_font_size_settings()));

    vcx.update(|_, cx| theme_settings::increase_buffer_font_size(cx));
    vcx.run_until_parked();
    let zoomed = LIVE_FONT_SIZE * (base + 1.0) / base;
    assert!((live_size(&editor, vcx) - zoomed).abs() < 0.001);

    vcx.update(|_, cx| {
        theme_settings::decrease_buffer_font_size(cx);
        theme_settings::decrease_buffer_font_size(cx);
    });
    vcx.run_until_parked();
    let zoomed = LIVE_FONT_SIZE * (base - 1.0) / base;
    assert!((live_size(&editor, vcx) - zoomed).abs() < 0.001);
}

#[gpui::test]
async fn two_editors_of_one_buffer_share_the_mode(cx: &mut TestAppContext) {
    init(cx);
    let buffer = buffer_in("text", Some(language("Markdown", "md")), cx);
    let other = buffer_in("other", Some(language("Markdown", "md")), cx);
    let (first, vcx) = editor_on(&buffer, cx);
    let second = another_editor_on(&buffer, vcx);
    let unrelated = another_editor_on(&other, vcx);

    vcx.update(|_, cx| set_mode(&buffer, Mode::Live, cx));
    vcx.run_until_parked();
    for editor in [&first, &second] {
        assert_eq!(live_size(editor, vcx), LIVE_FONT_SIZE);
        editor.read_with(vcx, |editor, _| assert!(!editor.show_gutter()));
    }
    assert_eq!(
        refinement(&unrelated, vcx),
        None,
        "other buffers are untouched"
    );
    assert_eq!(
        vcx.update(|_, cx| mode_for_buffer(&other, cx)),
        Some(Mode::Source)
    );
}

#[gpui::test]
async fn a_released_buffer_loses_its_entry(cx: &mut TestAppContext) {
    init(cx);
    let buffer = buffer_in("text", None, cx);
    let kept = buffer_in("text", None, cx);
    cx.update(|cx| {
        set_mode(&buffer, Mode::Live, cx);
        set_mode(&kept, Mode::Source, cx);
        assert_eq!(cx.global::<MarkdownModes>().len(), 2);
    });
    drop(buffer);
    // The release is processed when the app flushes its effects.
    cx.update(|_| {});
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(cx.global::<MarkdownModes>().len(), 1);
        assert_eq!(mode_for_buffer(&kept, cx), Some(Mode::Source));
    });
}

#[gpui::test]
async fn the_actions_are_hidden_from_the_command_palette(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let filter = CommandPaletteFilter::try_global(cx).expect("the filter is installed");
        assert_eq!(action_types().len(), 7);
        for name in [
            markdown::ToggleLiveView::name_for_type(),
            markdown::InsertBlock::name_for_type(),
            markdown::MoveBlockUp::name_for_type(),
            markdown::MoveBlockDown::name_for_type(),
            markdown::ToggleCheckbox::name_for_type(),
            markdown::NextTableCell::name_for_type(),
            markdown::PreviousTableCell::name_for_type(),
        ] {
            let action = cx.build_action(name, None).expect("a registered action");
            assert!(filter.is_hidden(action.as_ref()), "{name}");
        }
    });
}

#[gpui::test]
async fn each_action_has_a_declared_prefix_meaning(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        for name in [
            markdown::MoveBlockUp::name_for_type(),
            markdown::MoveBlockDown::name_for_type(),
        ] {
            assert!(
                matches!(prefix_arg::meaning(cx, name), Some(Meaning::Reads)),
                "{name}"
            );
        }
        for name in [
            markdown::ToggleLiveView::name_for_type(),
            markdown::InsertBlock::name_for_type(),
            markdown::ToggleCheckbox::name_for_type(),
            markdown::NextTableCell::name_for_type(),
            markdown::PreviousTableCell::name_for_type(),
        ] {
            assert!(
                matches!(prefix_arg::meaning(cx, name), Some(Meaning::Ignored)),
                "{name}"
            );
        }
    });
}
