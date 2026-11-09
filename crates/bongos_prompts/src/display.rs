//! What the prompts mode draws (designs D7 and D8): a grey rule with the marker's stamps in place
//! of each marker line, the current block's rows brighter, and the bodies of sent blocks dimmed,
//! except the current one.

use std::sync::Arc;

use bongos_prompts_model::{BlockInfo, MarkerLine, format_stamp};
use collections::HashSet;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::{Anchor, Editor, HighlightKey, MultiBufferSnapshot, RowHighlightOptions};
use gpui::{App, Context, HighlightStyle, Hsla, SharedString, px};
use language::Point;
use multi_buffer::MultiBufferRow;
use ui::{Label, LabelSize, prelude::*};

use crate::mode::{PromptsMode, head_row};

/// The row highlight type of the current block.
pub enum PromptsCurrent {}

/// The text at the right of a rule: "2026-11-05 13:40" or "2026-11-05 13:40 · sent 2026-11-05 14:02".
pub fn rule_label(marker: &MarkerLine) -> String {
    match &marker.sent {
        Some(sent) => format!(
            "{} · sent {}",
            format_stamp(&marker.created),
            format_stamp(sent)
        ),
        None => format_stamp(&marker.created),
    }
}

fn line_end(snapshot: &MultiBufferSnapshot, row: u32) -> Point {
    Point::new(row, snapshot.line_len(MultiBufferRow(row)))
}

fn rule_block(
    snapshot: &MultiBufferSnapshot,
    row: u32,
    label: SharedString,
) -> BlockProperties<Anchor> {
    let start = snapshot.anchor_before(Point::new(row, 0));
    let end = snapshot.anchor_after(line_end(snapshot, row));
    BlockProperties {
        placement: BlockPlacement::Replace(start..=end),
        height: Some(1),
        style: BlockStyle::Flex,
        priority: 0,
        render: Arc::new(move |cx: &mut BlockContext| render_rule(label.clone(), cx)),
    }
}

fn render_rule(label: SharedString, cx: &mut BlockContext) -> AnyElement {
    let color = cx.app.theme().colors().border_variant;
    h_flex()
        .w_full()
        .h(cx.line_height)
        .gap_2()
        .pl(cx.margins.gutter.full_width())
        .pr_2()
        .child(div().flex_1().h(px(1.)).bg(color))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .into_any_element()
}

/// Removes the rules of `removed` and adds those of `added`, one call each to the editor.
pub(crate) fn sync_rules(
    mode: &mut PromptsMode,
    editor: &mut Editor,
    removed: &[u64],
    added: &[u64],
    cx: &mut Context<Editor>,
) {
    let stale: HashSet<_> = removed
        .iter()
        .filter_map(|id| mode.rules.remove(id))
        .collect();
    if !stale.is_empty() {
        editor.remove_blocks(stale, None, cx);
        mode.stats.remove_blocks_calls += 1;
    }
    if added.is_empty() {
        return;
    }
    let added: HashSet<u64> = added.iter().copied().collect();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let (ids, blocks): (Vec<u64>, Vec<_>) = mode
        .index
        .markers()
        .iter()
        .filter(|marker| added.contains(&marker.id))
        .map(|marker| {
            let label = SharedString::from(rule_label(&marker.marker));
            (marker.id, rule_block(&snapshot, marker.row, label))
        })
        .unzip();
    if blocks.is_empty() {
        return;
    }
    let block_ids = editor.insert_blocks(blocks, None, cx);
    mode.stats.insert_blocks_calls += 1;
    mode.rules.extend(ids.into_iter().zip(block_ids));
}

fn current_color(cx: &App) -> Hsla {
    cx.theme().colors().editor_active_line_background
}

/// The anchors of a body's rows, or `None` for an empty body.
fn body_range(
    snapshot: &MultiBufferSnapshot,
    block: &BlockInfo,
) -> Option<std::ops::Range<Anchor>> {
    if block.body.is_empty() {
        return None;
    }
    let start = snapshot.anchor_before(Point::new(block.body.start, 0));
    let end = snapshot.anchor_after(line_end(snapshot, block.body.end - 1));
    Some(start..end)
}

/// Finds the block of the newest selection's head. Its rows are highlighted again when it is
/// another block or its rows changed (or `force`), and the sent bodies are dimmed again when it is
/// another block (or `force`); a cursor move inside the block changes nothing.
pub(crate) fn update_current(
    mode: &mut PromptsMode,
    editor: &mut Editor,
    force: bool,
    cx: &mut Context<Editor>,
) {
    let block = mode.index.block_at(head_row(editor, cx));
    let previous = mode.current.as_ref();
    let other_block = previous.is_none_or(|current| current.id != block.id);
    let other_rows = previous.is_none_or(|current| current.body != block.body);
    if !force && !other_block && !other_rows {
        mode.current = Some(block);
        return;
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    editor.clear_row_highlights::<PromptsCurrent>();
    if let Some(range) = body_range(&snapshot, &block) {
        editor.highlight_rows::<PromptsCurrent>(
            range,
            current_color,
            RowHighlightOptions {
                include_gutter: false,
                autoscroll: false,
            },
            cx,
        );
    }
    if force || other_block {
        let sent: Vec<_> = (0..mode.index.block_count())
            .filter_map(|index| mode.index.block(index))
            .filter(|other| !other.is_pending() && other.id != block.id)
            .filter_map(|other| body_range(&snapshot, &other))
            .collect();
        if sent.is_empty() {
            editor.clear_highlights(HighlightKey::PromptsSent, cx);
        } else {
            let muted = cx.theme().colors().text_muted;
            editor.highlight_text(
                HighlightKey::PromptsSent,
                sent,
                HighlightStyle {
                    color: Some(muted),
                    fade_out: Some(0.35),
                    ..HighlightStyle::default()
                },
                cx,
            );
        }
    }
    mode.current = Some(block);
    cx.notify();
}

/// Removes every rule and highlight of the mode.
pub(crate) fn clear(mode: &mut PromptsMode, editor: &mut Editor, cx: &mut Context<Editor>) {
    let ids: HashSet<_> = mode.rules.drain().map(|(_, id)| id).collect();
    if !ids.is_empty() {
        editor.remove_blocks(ids, None, cx);
        mode.stats.remove_blocks_calls += 1;
    }
    editor.clear_row_highlights::<PromptsCurrent>();
    editor.clear_highlights(HighlightKey::PromptsSent, cx);
    mode.current = None;
}
