use editor::CursorStyleOverride;
use language::CursorShape;
use settings::{BaseKeymapContent, EmacsCursorStyle, SettingsContent, SettingsStore};

use crate::state::Mode;
use crate::test::VimTestContext;

fn update_settings(cx: &mut VimTestContext, change: impl FnOnce(&mut SettingsContent)) {
    cx.update_global(|store: &mut SettingsStore, cx| store.update_user_settings(cx, change));
}

/// A vim editor under a base keymap whose settings shape is a bar.
async fn vim_with_bar_settings(cx: &mut gpui::TestAppContext) -> VimTestContext {
    let mut cx = VimTestContext::new(cx, true).await;
    update_settings(&mut cx, |content| {
        content.base_keymap = Some(BaseKeymapContent::VSCode);
        content.editor.cursor_shape = Some(settings::CursorShape::Bar);
    });
    cx
}

fn shape(cx: &mut VimTestContext) -> CursorShape {
    cx.update_editor(|editor, _, _| editor.cursor_shape())
}

fn set_override(cx: &mut VimTestContext, shape: Option<CursorShape>) {
    cx.update_editor(|editor, _, cx| editor.set_cursor_shape_override(shape, cx));
}

#[gpui::test]
async fn each_mode_draws_its_own_shape(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Block);

    cx.simulate_keystrokes("i");
    assert_eq!(cx.mode(), Mode::Insert);
    assert_eq!(shape(&mut cx), CursorShape::Bar);

    cx.simulate_keystrokes("escape shift-r");
    assert_eq!(cx.mode(), Mode::Replace);
    assert_eq!(shape(&mut cx), CursorShape::Underline);

    cx.simulate_keystrokes("escape v");
    assert_eq!(cx.mode(), Mode::Visual);
    assert_eq!(shape(&mut cx), CursorShape::Block);

    cx.simulate_keystrokes("escape");
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Block);
}

#[gpui::test]
async fn an_unfocused_block_is_hollow(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    assert_eq!(shape(&mut cx), CursorShape::Block);
    // The test workspace hands focus straight back to the editor, so the blur is driven
    // through the editor's own handler.
    cx.update_editor(|editor, window, cx| editor.handle_blur(window, cx));
    assert_eq!(shape(&mut cx), CursorShape::Hollow);
    cx.update_editor(|_, _, cx| cx.emit(editor::EditorEvent::Focused));
    assert_eq!(shape(&mut cx), CursorShape::Block);
}

#[gpui::test]
async fn the_editor_override_survives_a_mode_change_and_a_reload(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    set_override(&mut cx, Some(CursorShape::Bar));
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Bar);

    cx.simulate_keystrokes("v");
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Bar);

    update_settings(&mut cx, |content| {
        content.editor.cursor_shape = Some(settings::CursorShape::Underline);
    });
    assert_eq!(shape(&mut cx), CursorShape::Bar);

    set_override(&mut cx, None);
    assert_eq!(shape(&mut cx), CursorShape::Block);
}

#[gpui::test]
async fn an_explicit_cursor_style_beats_the_mode(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    update_settings(&mut cx, |content| {
        content.emacs.get_or_insert_default().cursor_style = Some(EmacsCursorStyle::Emacs);
    });
    cx.simulate_keystrokes("i");
    assert_eq!(cx.mode(), Mode::Insert);
    assert_eq!(shape(&mut cx), CursorShape::Block);

    update_settings(&mut cx, |content| {
        content.emacs.get_or_insert_default().cursor_style = Some(EmacsCursorStyle::Modern);
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Bar);

    // The editor's own override still beats the style, as it does without vim.
    set_override(&mut cx, Some(CursorShape::Underline));
    assert_eq!(shape(&mut cx), CursorShape::Underline);
}

#[gpui::test]
async fn the_toggle_beats_the_mode(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    assert_eq!(shape(&mut cx), CursorShape::Block);
    cx.update(|_, cx| CursorStyleOverride::set(CursorShape::Bar, cx));
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    cx.simulate_keystrokes("v");
    assert_eq!(cx.mode(), Mode::Visual);
    assert_eq!(shape(&mut cx), CursorShape::Bar);
    cx.simulate_keystrokes("escape i");
    assert_eq!(shape(&mut cx), CursorShape::Bar);
}

#[gpui::test]
async fn turning_vim_off_restores_the_settings_shape(cx: &mut gpui::TestAppContext) {
    let mut cx = vim_with_bar_settings(cx).await;
    update_settings(&mut cx, |content| {
        content.editor.cursor_shape = Some(settings::CursorShape::Underline);
    });
    assert_eq!(shape(&mut cx), CursorShape::Block);
    cx.disable_vim();
    assert_eq!(shape(&mut cx), CursorShape::Underline);
    cx.enable_vim();
    assert_eq!(cx.mode(), Mode::Normal);
    assert_eq!(shape(&mut cx), CursorShape::Block);
}
