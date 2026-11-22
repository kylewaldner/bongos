//! The label overlay: each candidate gets its label drawn over the first cells of its text, in
//! the editor that holds it, and the text under a label is faded. Showing and clearing only set
//! and remove editor overlays; neither touches buffer text or line layout.

use editor::{
    Editor, NavigationOverlayKey, NavigationOverlayLabel, NavigationTargetOverlay, ToOffset as _,
};
use gpui::{App, EntityId, WeakEntity, px};
use theme::ActiveTheme as _;

use crate::visible::Candidate;

enum AvyNavigationOverlay {}

/// The key the avy labels are set under on every editor.
pub const AVY_OVERLAY_KEY: NavigationOverlayKey =
    NavigationOverlayKey::unique::<AvyNavigationOverlay>();

/// Shows `labels` (candidates with their full labels, nearest candidate first) on their editors,
/// replacing what an earlier call showed there.
///
/// Each label covers as many cells of its candidate's line as the full label has keys, fewer at
/// the end of the line, and shows only the keys not typed yet. Labels that do not start with
/// `typed_prefix`, and labels it spells out completely, are not shown. Overlays are set farthest
/// first, so a nearer label paints over a farther one. Candidates whose editor is closed are
/// skipped; an editor left with no label has its overlays removed.
pub fn show(labels: &[(Candidate, String)], typed_prefix: &str, cx: &mut App) {
    let theme = cx.theme().clone();
    let text_color = theme.colors().editor_background;
    let background = theme.players().local().cursor;

    let mut groups: Vec<(EntityId, WeakEntity<Editor>, Vec<NavigationTargetOverlay>)> = Vec::new();
    for (candidate, label) in labels.iter().rev() {
        let Some(editor) = candidate.editor.upgrade() else {
            continue;
        };
        let index = match groups.iter().position(|(id, ..)| *id == editor.entity_id()) {
            Some(index) => index,
            None => {
                groups.push((editor.entity_id(), candidate.editor.clone(), Vec::new()));
                groups.len() - 1
            }
        };
        let Some(remaining) = label
            .strip_prefix(typed_prefix)
            .filter(|remaining| !remaining.is_empty())
        else {
            continue;
        };

        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let start = candidate.range.start;
        let start_offset = start.to_offset(&snapshot);
        let covered_bytes: usize = snapshot
            .chars_at(start_offset)
            .take(label.chars().count())
            .take_while(|ch| *ch != '\n')
            .map(char::len_utf8)
            .sum();
        let covered_text_range =
            (covered_bytes > 0).then(|| start..snapshot.anchor_after(start_offset + covered_bytes));

        groups[index].2.push(NavigationTargetOverlay {
            target_range: start..start,
            label: NavigationOverlayLabel {
                text: remaining.to_string().into(),
                text_color,
                x_offset: px(0.),
                scale_factor: 1.0,
                background: Some(background),
            },
            covered_text_range,
        });
    }

    for (_, editor, overlays) in groups {
        if let Some(editor) = editor.upgrade() {
            editor.update(cx, |editor, cx| {
                editor.set_navigation_overlays(AVY_OVERLAY_KEY, overlays, cx);
            });
        }
    }
}

/// Removes the labels and the fade from every editor in `editors` that is still open.
pub fn clear(editors: &[WeakEntity<Editor>], cx: &mut App) {
    for editor in editors {
        if let Some(editor) = editor.upgrade() {
            editor.update(cx, |editor, cx| {
                editor.clear_navigation_overlays(AVY_OVERLAY_KEY, cx);
            });
        }
    }
}
