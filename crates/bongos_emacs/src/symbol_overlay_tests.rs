use std::cell::RefCell;
use std::rc::Rc;

use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, HighlightKey};
use gpui::{AppContext as _, BorrowAppContext as _, Entity, Hsla, TestAppContext};
use multi_buffer::MultiBuffer;
use settings::{SettingsStore, ThemeName, ThemeSelection};
use theme::{ActiveTheme as _, ThemeRegistry};
use workspace::MinibufferRouting;

use crate::actions::{SymbolOverlayPut, SymbolOverlayRemoveAll, UniversalArgument};
use crate::apply_tests::init_test;
use crate::prefix_arg;
use crate::symbol_overlay::{MAX_RANGES, OVERLAY_ALPHA, overlay_color};

/// The themes of the One family, as they ship with the app.
const ONE_THEMES: &str = include_str!("../../../assets/themes/one/one.json");

async fn context(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state(marked);
    cx
}

type Shown = Vec<((u32, u32, u32), Hsla)>;

/// The overlay ranges of `editor` as (row, start column, end column) with their colors.
fn overlays_of(editor: &Entity<Editor>, cx: &mut EditorTestContext) -> Shown {
    let editor = editor.clone();
    cx.cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor
                .all_text_background_highlights(window, cx)
                .into_iter()
                .map(|(range, color)| {
                    (
                        (
                            range.start.row().0,
                            range.start.column(),
                            range.end.column(),
                        ),
                        color,
                    )
                })
                .collect()
        })
    })
}

fn overlays(cx: &mut EditorTestContext) -> Shown {
    cx.update_editor(|editor, window, cx| {
        editor
            .all_text_background_highlights(window, cx)
            .into_iter()
            .map(|(range, color)| {
                (
                    (
                        range.start.row().0,
                        range.start.column(),
                        range.end.column(),
                    ),
                    color,
                )
            })
            .collect()
    })
}

fn spans(shown: &Shown) -> Vec<(u32, u32, u32)> {
    shown.iter().map(|(span, _)| *span).collect()
}

fn palette(index: u32, cx: &mut EditorTestContext) -> Hsla {
    cx.update(|_, cx| overlay_color(cx.theme(), index))
}

#[gpui::test]
async fn put_highlights_whole_words_and_not_longer_identifiers(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo foobar foo_x foo").await;
    cx.dispatch_action(SymbolOverlayPut);
    let shown = overlays(&mut cx);
    assert_eq!(spans(&shown), [(0, 0, 3), (0, 17, 20)]);
    let color = palette(0, &mut cx);
    assert!(shown.iter().all(|(_, shown)| *shown == color));
    assert_eq!(color.a, OVERLAY_ALPHA);
    assert!(MAX_RANGES >= 1000);
}

#[gpui::test]
async fn put_again_removes_and_remove_all_clears(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar foo bar").await;
    cx.dispatch_action(SymbolOverlayPut);
    assert_eq!(overlays(&mut cx).len(), 2);
    cx.dispatch_action(SymbolOverlayPut);
    assert!(overlays(&mut cx).is_empty());

    cx.dispatch_action(SymbolOverlayPut);
    cx.set_selections_state("foo ˇbar foo bar");
    cx.dispatch_action(SymbolOverlayPut);
    assert_eq!(overlays(&mut cx).len(), 4);
    cx.dispatch_action(SymbolOverlayRemoveAll);
    assert!(overlays(&mut cx).is_empty());
    assert!(!cx.update_editor(|editor, _, _| {
        editor.has_background_highlights(HighlightKey::SymbolOverlay)
    }));
}

#[gpui::test]
async fn two_symbols_get_two_palette_colors(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar foo bar").await;
    cx.dispatch_action(SymbolOverlayPut);
    cx.set_selections_state("foo ˇbar foo bar");
    cx.dispatch_action(SymbolOverlayPut);
    let shown = overlays(&mut cx);
    assert_eq!(
        spans(&shown),
        [(0, 0, 3), (0, 4, 7), (0, 8, 11), (0, 12, 15)]
    );
    assert_eq!(shown[0].1, palette(0, &mut cx));
    assert_eq!(shown[1].1, palette(1, &mut cx));
    assert_eq!(shown[0].1, shown[2].1);
    assert_eq!(shown[1].1, shown[3].1);
    assert_ne!(shown[0].1, shown[1].1);
}

#[gpui::test]
async fn typing_an_occurrence_lights_it_and_editing_one_away_darkens_it(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar").await;
    cx.dispatch_action(SymbolOverlayPut);
    assert_eq!(spans(&overlays(&mut cx)), [(0, 0, 3)]);

    cx.set_selections_state("foo barˇ");
    cx.simulate_input(" foo");
    assert_eq!(spans(&overlays(&mut cx)), [(0, 0, 3), (0, 8, 11)]);

    cx.dispatch_action(editor::actions::Backspace);
    assert_eq!(spans(&overlays(&mut cx)), [(0, 0, 3)]);
}

#[gpui::test]
async fn another_editor_on_the_buffer_shares_and_another_buffer_shows_none(
    cx: &mut TestAppContext,
) {
    let mut cx = context(cx, "ˇfoo bar foo").await;
    let buffer = cx.editor(|editor, _, _| editor.buffer().clone());
    let (twin, other) = cx.cx.update(|window, cx| {
        let twin = cx.new(|cx| Editor::for_multibuffer(buffer, None, window, cx));
        let unrelated = MultiBuffer::build_simple("foo bar foo", cx);
        let other = cx.new(|cx| Editor::for_multibuffer(unrelated, None, window, cx));
        (twin, other)
    });
    cx.dispatch_action(SymbolOverlayPut);
    cx.run_until_parked();
    let own = overlays(&mut cx);
    assert_eq!(spans(&own), [(0, 0, 3), (0, 8, 11)]);
    assert_eq!(overlays_of(&twin, &mut cx), own);
    assert!(overlays_of(&other, &mut cx).is_empty());
}

#[gpui::test]
async fn overlays_are_not_selections_text_or_undo_entries(cx: &mut TestAppContext) {
    let mut cx = context(cx, "foo bˇar foo").await;
    cx.simulate_input("x");
    cx.set_selections_state("ˇfoo bxar foo");
    let before = cx.update_editor(|editor, _, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.read_with(cx, |buffer, _| {
            (
                buffer.text(),
                buffer.version(),
                buffer.peek_undo_stack().map(|entry| entry.transaction_id()),
            )
        })
    });
    let state = cx.editor_state();
    cx.dispatch_action(SymbolOverlayPut);
    assert_eq!(overlays(&mut cx).len(), 2);
    assert_eq!(cx.editor_state(), state);
    let after = cx.update_editor(|editor, _, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.read_with(cx, |buffer, _| {
            (
                buffer.text(),
                buffer.version(),
                buffer.peek_undo_stack().map(|entry| entry.transaction_id()),
            )
        })
    });
    assert!(before == after);
    let carets = cx.update_editor(|editor, _, _| editor.selections.count());
    assert_eq!(carets, 1);
}

#[gpui::test]
async fn the_commands_say_nothing_in_the_minibuffer(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar foo").await;
    let messages = Rc::new(RefCell::new(Vec::<String>::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    cx.dispatch_action(SymbolOverlayPut);
    cx.dispatch_action(SymbolOverlayPut);
    cx.dispatch_action(SymbolOverlayRemoveAll);
    assert!(messages.borrow().is_empty());
}

fn use_theme(name: &str, cx: &mut EditorTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.theme.theme = Some(ThemeSelection::Static(ThemeName(name.into())));
            });
        });
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.theme().name.as_ref(), name, "the theme is active"));
}

/// WCAG contrast ratio of two opaque colors.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    fn luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        let linear = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[gpui::test]
async fn every_palette_color_is_readable_in_one_light_and_one_dark(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar").await;
    cx.update(|_, cx| {
        let registry = ThemeRegistry::global(cx);
        theme_settings::load_user_theme(&registry, ONE_THEMES.as_bytes())
            .expect("the One themes load");
    });
    cx.dispatch_action(SymbolOverlayPut);

    for name in ["One Light", "One Dark"] {
        use_theme(name, &mut cx);
        let shown = overlays(&mut cx);
        assert_eq!(shown.len(), 1, "{name}");
        let (background, text, accents) = cx.update(|_, cx| {
            let colors = cx.theme().colors();
            (
                colors.editor_background,
                colors.editor_foreground,
                cx.theme().accents().0.len() as u32,
            )
        });
        for index in 0..accents {
            let color = palette(index, &mut cx);
            let blended = background.blend(color);
            assert_ne!(blended, background, "{name} {index}: the overlay shows");
            let ratio = contrast(blended, text);
            assert!(ratio >= 4.5, "{name} {index}: contrast {ratio}");
        }
    }
}

/// What the key interceptor does when a bound key follows C-u: the argument goes to the command.
fn hand_off(cx: &mut EditorTestContext) {
    cx.update(|window, cx| prefix_arg::hand_off(window, cx));
}

#[gpui::test]
async fn the_commands_ignore_the_argument(cx: &mut TestAppContext) {
    let marked = "ˇfoo bar foo";
    let mut cx = context(cx, marked).await;
    cx.dispatch_action(UniversalArgument);
    hand_off(&mut cx);
    cx.dispatch_action(SymbolOverlayPut);
    assert_eq!(overlays(&mut cx).len(), 2);
    assert!(
        cx.update(|window, cx| prefix_arg::pending(window, cx))
            .is_none()
    );

    cx.dispatch_action(UniversalArgument);
    hand_off(&mut cx);
    cx.dispatch_action(SymbolOverlayRemoveAll);
    assert!(overlays(&mut cx).is_empty());
    assert!(
        cx.update(|window, cx| prefix_arg::pending(window, cx))
            .is_none()
    );
    cx.assert_editor_state(marked);
}
