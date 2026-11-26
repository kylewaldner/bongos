//! The markdown mode of a buffer (live view or source) and the addon that carries it on the
//! editors of that buffer. The addon attaches to a full editor on a singleton buffer whose
//! language is exactly `Markdown`, never while the prompts mode is on the editor. In the live
//! mode it sets the live typography (UI font, 16 px scaled by the buffer font zoom, line height
//! 1.5, a bar cursor, no gutter); in source mode and on detach the editor gets back what it had.

use std::any::Any;
use std::cell::RefCell;

use collections::HashMap;
use editor::{Addon, Editor};
use gpui::{
    App, BorrowAppContext as _, Context, Entity, EntityId, Global, KeyContext, SharedString,
    TextStyleRefinement, px, relative,
};
use language::Buffer;
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;

use crate::MarkdownSettings;

/// The key context identifier the addon adds, so the keymap context is `Editor && markdown_mode`.
pub const KEY_CONTEXT: &str = "markdown_mode";

/// The name of the language the addon attaches to.
const LANGUAGE_NAME: &str = "Markdown";

/// The live body font size, in pixels, before the buffer font zoom ratio.
pub const LIVE_FONT_SIZE: f32 = 16.0;

/// The live line height, relative to the font size.
pub const LIVE_LINE_HEIGHT: f32 = 1.5;

/// How a markdown buffer is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Markup hidden and styled, set in the UI font.
    Live,
    /// The plain text in the editor's own style.
    Source,
}

/// The mode of each markdown buffer, by the buffer's entity id. An entry appears when an editor
/// first attaches to the buffer and goes when the buffer is released.
#[derive(Default)]
pub struct MarkdownModes(HashMap<EntityId, Mode>);

impl Global for MarkdownModes {}

impl MarkdownModes {
    /// The mode of the buffer with this id, when an editor has attached to it.
    pub fn get(&self, buffer: EntityId) -> Option<Mode> {
        self.0.get(&buffer).copied()
    }

    /// How many buffers have a mode.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The mode of `buffer`, when it has one.
pub fn mode_for_buffer(buffer: &Entity<Buffer>, cx: &App) -> Option<Mode> {
    cx.try_global::<MarkdownModes>()?.get(buffer.entity_id())
}

/// Sets the mode of `buffer`; every editor of the buffer follows.
pub fn set_mode(buffer: &Entity<Buffer>, mode: Mode, cx: &mut App) {
    let id = buffer.entity_id();
    let new = !cx.default_global::<MarkdownModes>().0.contains_key(&id);
    cx.update_global::<MarkdownModes, _>(|modes, _| {
        modes.0.insert(id, mode);
    });
    if new {
        watch_release(buffer, cx);
    }
}

/// Drops the entry of `buffer` when the buffer is released.
fn watch_release(buffer: &Entity<Buffer>, cx: &mut App) {
    let id = buffer.entity_id();
    cx.observe_release(buffer, move |_, cx| {
        cx.update_global::<MarkdownModes, _>(|modes, _| {
            modes.0.remove(&id);
        });
    })
    .detach();
}

/// The mode a buffer starts in: live when the setting says so and the buffer is not over the
/// size limit.
fn initial_mode(buffer: &Entity<Buffer>, cx: &App) -> Mode {
    let settings = MarkdownSettings::get_global(cx);
    let limit = settings.live_view_max_file_size;
    let too_large = limit != 0 && buffer.read(cx).len() as u64 > limit;
    if settings.live_view && !too_large {
        Mode::Live
    } else {
        Mode::Source
    }
}

/// What the editor had before the live typography, to give back.
struct Saved {
    refinement: Option<TextStyleRefinement>,
    cursor: Option<language::CursorShape>,
    gutter: bool,
}

/// The addon that marks an editor as a markdown editor.
#[derive(Default)]
pub struct MarkdownAddon {
    /// Set while the live typography is applied: what to give back.
    saved: RefCell<Option<Saved>>,
}

impl Addon for MarkdownAddon {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add(KEY_CONTEXT);
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

/// Whether `editor` has the markdown addon.
pub fn is_attached(editor: &Editor) -> bool {
    editor.addon::<MarkdownAddon>().is_some()
}

/// Watches every new full editor on a single buffer: it gets the addon while its buffer's
/// language is Markdown and the prompts mode is not on it.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
            return;
        }
        let this = cx.entity();
        cx.subscribe(&this, |editor, _, event: &editor::EditorEvent, cx| {
            // Also emitted on a language change; the language is checked again.
            if matches!(event, editor::EditorEvent::Reparsed(_)) {
                refresh(editor, cx);
            }
        })
        .detach();
        cx.observe_global::<SettingsStore>(refresh).detach();
        cx.observe_global::<MarkdownModes>(refresh).detach();
        theme_settings::observe_buffer_font_size_adjustment(cx, refresh).detach();
        refresh(editor, cx);
    })
    .detach();
}

fn wants_addon(editor: &Editor, cx: &App) -> bool {
    if bongos_prompts::is_attached(editor) {
        return false;
    }
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language().cloned())
        .is_some_and(|language| language.name().as_ref() == LANGUAGE_NAME)
}

/// Attaches or detaches the addon to match the buffer's language, then brings the typography in
/// line with the buffer's mode.
fn refresh(editor: &mut Editor, cx: &mut Context<Editor>) {
    match (wants_addon(editor, cx), is_attached(editor)) {
        (true, false) => attach(editor, cx),
        (false, true) => detach(editor, cx),
        _ => {}
    }
    apply(editor, cx);
}

fn attach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    if mode_for_buffer(&buffer, cx).is_none() {
        let mode = initial_mode(&buffer, cx);
        set_mode(&buffer, mode, cx);
    }
    editor.register_addon(MarkdownAddon::default());
    cx.notify();
}

fn detach(editor: &mut Editor, cx: &mut Context<Editor>) {
    restore(editor, cx);
    editor.unregister_addon::<MarkdownAddon>();
    cx.notify();
}

/// Applies the typography the buffer's mode asks for.
fn apply(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !is_attached(editor) {
        return;
    }
    let live = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| mode_for_buffer(&buffer, cx))
        == Some(Mode::Live);
    if live {
        apply_live(editor, cx);
    } else {
        restore(editor, cx);
    }
}

/// The live font family: the system UI font on macOS, the `ui_font_family` setting elsewhere.
fn live_font_family(cx: &App) -> SharedString {
    if cfg!(target_os = "macos") {
        SharedString::from(".SystemUIFont")
    } else {
        ThemeSettings::get_global(cx).ui_font.family.clone()
    }
}

/// The buffer font zoom ratio: the adjusted buffer font size over the `buffer_font_size` setting.
fn zoom_ratio(cx: &App) -> f32 {
    let settings = ThemeSettings::get_global(cx);
    let base = f32::from(settings.buffer_font_size_settings());
    if base <= 0.0 {
        return 1.0;
    }
    f32::from(settings.buffer_font_size(cx)) / base
}

fn apply_live(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon::<MarkdownAddon>() else {
        return;
    };
    let base = {
        let mut saved = addon.saved.borrow_mut();
        if saved.is_none() {
            *saved = Some(Saved {
                refinement: editor.text_style_refinement().cloned(),
                cursor: editor.cursor_shape_override(),
                gutter: editor.show_gutter(),
            });
        }
        saved
            .as_ref()
            .and_then(|saved| saved.refinement.clone())
            .unwrap_or_default()
    };
    let mut refinement = base;
    refinement.font_family = Some(live_font_family(cx));
    refinement.font_size = Some(px(LIVE_FONT_SIZE * zoom_ratio(cx)).into());
    refinement.line_height = Some(relative(LIVE_LINE_HEIGHT));
    editor.set_text_style_refinement(refinement);
    editor.set_cursor_shape_override(Some(language::CursorShape::Bar), cx);
    editor.set_show_gutter(false, cx);
    cx.notify();
}

/// Gives back what the editor had before the live typography, when it was applied.
fn restore(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(saved) = editor
        .addon::<MarkdownAddon>()
        .and_then(|addon| addon.saved.borrow_mut().take())
    else {
        return;
    };
    match saved.refinement {
        Some(refinement) => editor.set_text_style_refinement(refinement),
        None => editor.clear_text_style_refinement(),
    }
    editor.set_cursor_shape_override(saved.cursor, cx);
    editor.set_show_gutter(saved.gutter, cx);
    cx.notify();
}
