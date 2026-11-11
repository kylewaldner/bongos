//! Expand and contract region. Both ride on the editor's syntax-node selection and its history
//! stack, so contracting returns to exactly the selection before the last expansion, and
//! contracting with an empty history does nothing. A negative count runs the other direction.

use editor::Editor;
use editor::actions::{SelectLargerSyntaxNode, SelectSmallerSyntaxNode};
use gpui::{App, Entity, Window};

use crate::actions::{ContractRegion, ExpandRegion};

/// Steps `count` times in one editor update; `larger` picks the direction.
fn step(
    editor: &Entity<Editor>,
    count: Option<i32>,
    larger: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let count = count.unwrap_or(1);
    let larger = larger == (count >= 0);
    editor.update(cx, |editor, cx| {
        for _ in 0..count.unsigned_abs() {
            if larger {
                editor.select_larger_syntax_node(&SelectLargerSyntaxNode, window, cx);
            } else {
                editor.select_smaller_syntax_node(&SelectSmallerSyntaxNode, window, cx);
            }
        }
    });
}

pub(crate) fn expand_region_command(
    action: &ExpandRegion,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    step(editor, action.count, true, window, cx);
}

pub(crate) fn contract_region_command(
    action: &ContractRegion,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    step(editor, action.count, false, window, cx);
}
