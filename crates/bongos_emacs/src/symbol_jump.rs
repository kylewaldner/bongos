//! Symbol jump: M-n and M-p move the caret to the next or previous whole-word occurrence of the
//! symbol at point, wrapping at the ends of the buffer. The landing is not a navigation entry,
//! and no mark is pushed. The occurrence flashes: its background starts at the theme's active
//! search match color and fades over four steps, then goes away. The model lives in
//! `bongos_emacs_model::symbol`.

use std::ops::Range;
use std::time::Duration;

use bongos_emacs_model::symbol::{Direction, next_occurrence};
use collections::HashMap;
use editor::scroll::Autoscroll;
use editor::{Editor, HighlightKey, SelectionEffects};
use gpui::{App, Context, Entity, EntityId, Global, WeakEntity, Window};
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};
use theme::ActiveTheme as _;

use crate::actions::{SymbolJumpNext, SymbolJumpPrev};
use crate::lines::propagate_in_single_line;
use crate::snapshot_text::SnapshotText;

const NO_SYMBOL: &str = "No symbol at point";
const WRAPPED: &str = "wrapped";

/// How many times the flash is drawn before it is cleared; the last draw is the faintest.
pub(crate) const FLASH_STEPS: u32 = 4;

/// How much of the theme's active match color the flash shows at its strongest. The full color
/// would leave ordinary text under 4.5 to 1 against it in One Dark.
pub(crate) const FLASH_PEAK: f32 = 0.4;

/// The time each draw of the flash lasts.
pub(crate) const FLASH_STEP: Duration = Duration::from_millis(100);

/// The newest flash of each editor. A fade task stops as soon as its number is no longer the
/// editor's, so a new jump takes over from an older flash.
#[derive(Default)]
struct Flashes {
    editors: HashMap<EntityId, (WeakEntity<Editor>, u64)>,
}

impl Global for Flashes {}

/// Starts a new flash for `editor` and returns its number. Entries of dropped editors go.
fn begin_flash(editor: &Entity<Editor>, cx: &mut App) -> u64 {
    let flashes = cx.default_global::<Flashes>();
    flashes
        .editors
        .retain(|_, (weak, _)| weak.upgrade().is_some());
    let entry = flashes
        .editors
        .entry(editor.entity_id())
        .or_insert_with(|| (editor.downgrade(), 0));
    entry.1 += 1;
    entry.1
}

fn is_current(editor: EntityId, flash: u64, cx: &App) -> bool {
    cx.try_global::<Flashes>()
        .and_then(|flashes| flashes.editors.get(&editor))
        .is_some_and(|(_, current)| *current == flash)
}

/// The fade factor of the `step`th draw: 1.0, then a quarter less each time.
fn fade(step: u32) -> f32 {
    1.0 - step as f32 / FLASH_STEPS as f32
}

/// The flash color the theme gives at `factor`: the active search match background with its
/// opacity scaled down to `FLASH_PEAK` and by `factor`. It is read from the theme at paint time, so a theme switch shows at once.
pub(crate) fn flash_color(theme: &theme::Theme, factor: f32) -> gpui::Hsla {
    let mut color = theme.colors().search_active_match_background;
    color.a *= factor * FLASH_PEAK;
    color
}

fn draw_flash(editor: &mut Editor, range: &Range<Anchor>, factor: f32, cx: &mut Context<Editor>) {
    editor.highlight_background(
        HighlightKey::SymbolJumpFlash,
        std::slice::from_ref(range),
        move |_, theme| flash_color(theme, factor),
        cx,
    );
}

fn jump(direction: Direction, editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if propagate_in_single_line(editor, cx) {
        return;
    }
    let landed = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0;
        let found = next_occurrence(&SnapshotText::new(&snapshot), head, direction)?;
        let caret = MultiBufferOffset(found.caret);
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::fit()).nav_history(false),
            window,
            cx,
            |s| s.select_ranges([caret..caret]),
        );
        editor.set_selection_mark_mode(false, cx);
        let range = snapshot.anchor_before(MultiBufferOffset(found.range.start))
            ..snapshot.anchor_after(MultiBufferOffset(found.range.end));
        Some((range, found.wrapped))
    });
    let Some((range, wrapped)) = landed else {
        bongos_minibuffer::message(window, cx, NO_SYMBOL);
        return;
    };
    if wrapped {
        bongos_minibuffer::message(window, cx, WRAPPED);
    }
    let flash = begin_flash(editor, cx);
    editor.update(cx, |editor, cx| {
        draw_flash(editor, &range, fade(0), cx);
        let id = cx.entity_id();
        cx.spawn(async move |editor, cx| {
            for step in 1..=FLASH_STEPS {
                cx.background_executor().timer(FLASH_STEP).await;
                let range = range.clone();
                let alive = editor.update(cx, |editor, cx| {
                    if !is_current(id, flash, cx) {
                        return false;
                    }
                    if step == FLASH_STEPS {
                        editor.clear_background_highlights(HighlightKey::SymbolJumpFlash, cx);
                    } else {
                        draw_flash(editor, &range, fade(step), cx);
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    });
}

pub(crate) fn symbol_jump_next_command(
    _: &SymbolJumpNext,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    jump(Direction::Forward, editor, window, cx);
}

pub(crate) fn symbol_jump_prev_command(
    _: &SymbolJumpPrev,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    jump(Direction::Backward, editor, window, cx);
}
