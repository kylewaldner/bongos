//! The markdown mode of a buffer (live view or source) and the addon that carries it on the
//! editors of that buffer. The addon attaches to a full editor on a singleton buffer whose
//! language is exactly `Markdown`, never while the prompts mode is on the editor. In the live
//! mode it sets the live typography (UI font, 16 px scaled by the buffer font zoom, line height
//! 1.5, a bar cursor, no gutter), keeps the buffer's index fresh and refreshes the live layers;
//! in source mode and on detach the editor gets back what it had and every layer is cleared.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bongos_markdown_model::MarkdownIndex;
use collections::HashMap;
use editor::{Addon, Editor, EditorEvent};
use gpui::{
    App, BorrowAppContext as _, Context, Entity, EntityId, Global, KeyContext, SharedString,
    TextStyleRefinement, px, relative,
};
use language::Buffer;
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;

use crate::MarkdownSettings;
use crate::index::{IndexState, ParseStats};
use crate::layers::{self, LiveContext, LiveLayer};

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

/// The live view state of one editor: its index and its layers.
pub struct Live {
    pub index: IndexState,
    pub layers: Vec<Box<dyn LiveLayer>>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            index: IndexState::default(),
            layers: layers::default_layers(),
        }
    }
}

/// The addon that marks an editor as a markdown editor.
#[derive(Default)]
pub struct MarkdownAddon {
    /// Set while the live typography is applied: what to give back.
    saved: RefCell<Option<Saved>>,
    live: Rc<RefCell<Live>>,
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

fn live_of(editor: &Editor) -> Option<Rc<RefCell<Live>>> {
    editor
        .addon::<MarkdownAddon>()
        .map(|addon| addon.live.clone())
}

/// The index of `editor`'s buffer in the offsets of its current text (the last parse result,
/// mapped through the edits since), when the addon is attached.
pub fn fresh_index(editor: &Editor, cx: &App) -> Option<Arc<MarkdownIndex>> {
    let live = live_of(editor)?;
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let snapshot = buffer.read(cx).text_snapshot();
    Some(live.borrow_mut().index.fresh(&snapshot))
}

/// The parse counts of `editor`, when the addon is attached.
pub fn parse_stats(editor: &Editor) -> Option<ParseStats> {
    Some(live_of(editor)?.borrow().index.stats())
}

/// Whether `editor`'s buffer is in the live mode (and the addon is attached).
fn is_live(editor: &Editor, cx: &App) -> bool {
    is_attached(editor)
        && editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| mode_for_buffer(&buffer, cx))
            == Some(Mode::Live)
}

/// Watches every new full editor on a single buffer: it gets the addon while its buffer's
/// language is Markdown and the prompts mode is not on it.
pub(crate) fn init(cx: &mut App) {
    editor::register_conceal_tag(crate::conceals::conceal_tag(), cx);
    cx.observe_new(|editor: &mut Editor, window, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
            return;
        }
        let this = cx.entity();
        cx.subscribe(&this, |editor, _, event: &EditorEvent, cx| match event {
            // Also emitted on a language change; the language is checked again.
            EditorEvent::Reparsed(_) => refresh(editor, cx),
            EditorEvent::BufferEdited => request_parse(editor, cx),
            EditorEvent::ScrollPositionChanged { .. } | EditorEvent::SelectionsChanged { .. } => {
                refresh_layers(editor, false, cx)
            }
            _ => {}
        })
        .detach();
        if let Some(window) = window {
            // A resize changes the screen height, which the editor learns at its next layout.
            cx.observe_window_bounds(window, |_, window, cx| {
                cx.on_next_frame(window, |editor, _, cx| refresh_layers(editor, false, cx));
            })
            .detach();
        }
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
    clear_layers(editor, cx);
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
        request_parse(editor, cx);
        refresh_layers(editor, true, cx);
    } else {
        restore(editor, cx);
        clear_layers(editor, cx);
    }
}

/// Asks for a parse of the buffer as it is now, in the live mode.
fn request_parse(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !is_live(editor, cx) {
        return;
    }
    let (Some(live), Some(buffer)) = (live_of(editor), editor.buffer().read(cx).as_singleton())
    else {
        return;
    };
    let snapshot = buffer.read(cx).text_snapshot();
    crate::index::request(&mut live.borrow_mut().index, snapshot, parsed, cx);
}

/// Takes a parse result: a current one refreshes the layers, and a wanted next parse starts.
fn parsed(
    editor: &mut Editor,
    version: clock::Global,
    index: MarkdownIndex,
    cx: &mut Context<Editor>,
) {
    let (Some(live), Some(buffer)) = (live_of(editor), editor.buffer().read(cx).as_singleton())
    else {
        return;
    };
    let current = buffer.read(cx).version();
    let finished = live.borrow_mut().index.finish(version, index, &current);
    if finished.accepted {
        refresh_layers(editor, false, cx);
    }
    if finished.again {
        request_parse(editor, cx);
    }
}

/// Refreshes every layer over the window, in the live mode. `force` makes the layers refresh
/// even when the index, the text and the window are as they were.
fn refresh_layers(editor: &mut Editor, force: bool, cx: &mut Context<Editor>) {
    if !is_live(editor, cx) {
        return;
    }
    let (Some(live), Some(buffer)) = (live_of(editor), editor.buffer().read(cx).as_singleton())
    else {
        return;
    };
    let snapshot = buffer.read(cx).snapshot();
    let window = layers::window(editor, cx);
    let (index, generation, mut list) = {
        let mut live = live.borrow_mut();
        let index = live.index.fresh(&snapshot);
        (
            index,
            live.index.generation(),
            std::mem::take(&mut live.layers),
        )
    };
    let context = LiveContext {
        index: &index,
        generation,
        snapshot: &snapshot,
        window,
        force,
    };
    for layer in &mut list {
        layer.refresh(&context, editor, cx);
    }
    live.borrow_mut().layers = list;
}

/// Clears every layer.
fn clear_layers(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(live) = live_of(editor) else {
        return;
    };
    let mut list = std::mem::take(&mut live.borrow_mut().layers);
    for layer in &mut list {
        layer.clear(editor, cx);
    }
    live.borrow_mut().layers = list;
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
