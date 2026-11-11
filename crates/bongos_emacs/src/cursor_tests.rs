use editor::Editor;
use editor::test::editor_test_context::EditorTestContext;
use editor::{CursorStyleOverride, EditorSettings};
use gpui::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext};
use language::CursorShape;
use settings::{BaseKeymapContent, EmacsCursorStyle, Settings as _, SettingsStore};

use crate::actions::ToggleStyle;
use crate::apply_tests::init_test;

fn update_settings(
    cx: &mut EditorTestContext,
    change: impl FnOnce(&mut settings::SettingsContent),
) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| store.update_user_settings(cx, change));
    });
}

fn set_style(cx: &mut EditorTestContext, style: EmacsCursorStyle) {
    update_settings(cx, |content| {
        content.emacs.get_or_insert_default().cursor_style = Some(style);
    });
}

fn set_keymap(cx: &mut EditorTestContext, keymap: BaseKeymapContent) {
    update_settings(cx, |content| content.base_keymap = Some(keymap));
}

fn shape(cx: &mut EditorTestContext) -> CursorShape {
    cx.update_editor(|editor, _, _| editor.cursor_shape())
}

fn add_editor(cx: &mut EditorTestContext) -> Entity<Editor> {
    cx.update(|window, cx| cx.new(|cx| Editor::multi_line(window, cx)))
}

fn second_shape(cx: &mut EditorTestContext, editor: &Entity<Editor>) -> CursorShape {
    cx.update(|_, cx| editor.read(cx).cursor_shape())
}

fn settings_shape(cx: &mut EditorTestContext) -> Option<CursorShape> {
    cx.update(|_, cx| EditorSettings::get_global(cx).cursor_shape)
}

#[gpui::test]
async fn defaults_are_the_emacs_base_and_a_block(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        let content = cx.global::<SettingsStore>().merged_settings();
        assert_eq!(
            content.emacs.as_ref().and_then(|emacs| emacs.cursor_style),
            Some(EmacsCursorStyle::Auto)
        );
        assert_eq!(content.base_keymap, Some(BaseKeymapContent::Emacs));
        assert_eq!(CursorStyleOverride::get(cx), None);
    });
    assert_eq!(settings_shape(&mut cx), Some(CursorShape::Block));
    assert_eq!(shape(&mut cx), CursorShape::Block);
    set_keymap(&mut cx, BaseKeymapContent::VSCode);
    assert_eq!(shape(&mut cx), CursorShape::Bar);
}

#[gpui::test]
async fn auto_follows_the_base_keymap(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    set_keymap(&mut cx, BaseKeymapContent::Emacs);
    assert_eq!(shape(&mut cx), CursorShape::Block);
    set_keymap(&mut cx, BaseKeymapContent::VSCode);
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    update_settings(&mut cx, |content| {
        content.editor.cursor_shape = Some(settings::CursorShape::Underline);
    });
    assert_eq!(shape(&mut cx), CursorShape::Underline);
}

#[gpui::test]
async fn emacs_and_modern_override_the_cursor_shape(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    update_settings(&mut cx, |content| {
        content.editor.cursor_shape = Some(settings::CursorShape::Underline);
    });
    set_style(&mut cx, EmacsCursorStyle::Emacs);
    assert_eq!(shape(&mut cx), CursorShape::Block);
    set_style(&mut cx, EmacsCursorStyle::Modern);
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    set_keymap(&mut cx, BaseKeymapContent::Emacs);
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    set_style(&mut cx, EmacsCursorStyle::Emacs);
    set_keymap(&mut cx, BaseKeymapContent::VSCode);
    assert_eq!(shape(&mut cx), CursorShape::Block);
}

#[gpui::test]
async fn the_toggle_flips_every_editor_and_survives_a_reload(cx: &mut TestAppContext) {
    init_test(cx);
    let mut first = EditorTestContext::new(cx).await;
    let second = add_editor(&mut first);
    set_keymap(&mut first, BaseKeymapContent::VSCode);
    assert_eq!(shape(&mut first), CursorShape::Bar);

    first.dispatch_action(ToggleStyle);
    assert_eq!(shape(&mut first), CursorShape::Block);
    assert_eq!(second_shape(&mut first, &second), CursorShape::Block);

    set_keymap(&mut first, BaseKeymapContent::Emacs);
    set_style(&mut first, EmacsCursorStyle::Modern);
    assert_eq!(shape(&mut first), CursorShape::Block);
    assert_eq!(second_shape(&mut first, &second), CursorShape::Block);

    first.dispatch_action(ToggleStyle);
    assert_eq!(shape(&mut first), CursorShape::Bar);
    assert_eq!(second_shape(&mut first, &second), CursorShape::Bar);
}

#[gpui::test]
async fn a_new_editor_starts_with_the_toggled_shape(cx: &mut TestAppContext) {
    init_test(cx);
    let mut first = EditorTestContext::new(cx).await;
    set_keymap(&mut first, BaseKeymapContent::VSCode);
    first.dispatch_action(ToggleStyle);
    let second = add_editor(&mut first);
    assert_eq!(second_shape(&mut first, &second), CursorShape::Block);
}

#[gpui::test]
async fn the_editor_override_beats_the_toggle(cx: &mut TestAppContext) {
    init_test(cx);
    let mut first = EditorTestContext::new(cx).await;
    let second = add_editor(&mut first);
    set_keymap(&mut first, BaseKeymapContent::VSCode);
    first.update_editor(|editor, _, cx| {
        editor.set_cursor_shape_override(Some(CursorShape::Bar), cx);
    });
    first.dispatch_action(ToggleStyle);
    assert_eq!(shape(&mut first), CursorShape::Bar);
    assert_eq!(second_shape(&mut first, &second), CursorShape::Block);

    set_style(&mut first, EmacsCursorStyle::Emacs);
    assert_eq!(shape(&mut first), CursorShape::Bar);

    first.update_editor(|editor, _, cx| editor.set_cursor_shape_override(None, cx));
    assert_eq!(shape(&mut first), CursorShape::Block);
}
